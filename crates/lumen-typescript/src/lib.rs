//! Dependency-free TypeScript parsing and checking for Lumen.
//!
//! This crate owns TypeScript syntax and semantic analysis. The JavaScript engine remains unaware
//! of types; executable source is erased only after this checker has consumed the typed program.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: u16,
    pub message: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Any,
    Unknown,
    Never,
    Void,
    Undefined,
    Null,
    Boolean,
    Number,
    BigInt,
    String,
    StringLiteral(String),
    NumberLiteral(f64),
    Reference { name: String, arguments: Vec<Type> },
    Array(Box<Type>),
    Tuple(Vec<Type>),
    Union(Vec<Type>),
    Intersection(Vec<Type>),
    Object(Vec<Property>),
    Function { parameters: Vec<Type>, returns: Box<Type> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub name: String,
    pub optional: bool,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Ident(String),
    String(String),
    Number(f64),
    Punct(char),
    Arrow,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: Kind,
    span: Span,
}

pub fn parse_type_expression(source: &str) -> Result<Type, Diagnostic> {
    let tokens = tokenize(source)?;
    let mut parser = Parser { tokens, cursor: 0 };
    let ty = parser.parse_type()?;
    if !matches!(parser.current().kind, Kind::Eof) {
        return Err(parser.error(1005, "Unexpected token after type expression"));
    }
    Ok(ty)
}

fn tokenize(source: &str) -> Result<Vec<Token>, Diagnostic> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let (mut index, mut line, mut column) = (0, 1, 1);
    while index < chars.len() {
        let ch = chars[index];
        if ch.is_whitespace() {
            if ch == '\n' { line += 1; column = 1; } else { column += 1; }
            index += 1;
            continue;
        }
        if ch == '/' && chars.get(index + 1) == Some(&'/') {
            index += 2;
            column += 2;
            while index < chars.len() && chars[index] != '\n' { index += 1; column += 1; }
            continue;
        }
        if ch == '/' && chars.get(index + 1) == Some(&'*') {
            let start = Span { start: index, end: index + 2, line, column };
            index += 2;
            column += 2;
            let mut closed = false;
            while index < chars.len() {
                if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                    index += 2;
                    column += 2;
                    closed = true;
                    break;
                }
                if chars[index] == '\n' { line += 1; column = 1; } else { column += 1; }
                index += 1;
            }
            if !closed { return Err(diagnostic(1010, "Unterminated comment", start)); }
            continue;
        }
        let start = index;
        let token_line = line;
        let token_column = column;
        let kind = if is_ident_start(ch) {
            index += 1;
            column += 1;
            while index < chars.len() && is_ident_part(chars[index]) { index += 1; column += 1; }
            Kind::Ident(chars[start..index].iter().collect())
        } else if ch.is_ascii_digit() {
            index += 1;
            column += 1;
            while index < chars.len() && (chars[index].is_ascii_digit() || chars[index] == '.') {
                index += 1;
                column += 1;
            }
            let text: String = chars[start..index].iter().collect();
            let value = text.parse().map_err(|_| diagnostic(1005, "Invalid numeric literal", Span {
                start, end: index, line: token_line, column: token_column,
            }))?;
            Kind::Number(value)
        } else if ch == '\'' || ch == '"' {
            let quote = ch;
            index += 1;
            column += 1;
            let mut value = String::new();
            let mut closed = false;
            while index < chars.len() {
                let current = chars[index];
                if current == quote { index += 1; column += 1; closed = true; break; }
                if current == '\\' {
                    index += 1;
                    column += 1;
                    let Some(escaped) = chars.get(index).copied() else { break };
                    value.push(match escaped { 'n' => '\n', 'r' => '\r', 't' => '\t', other => other });
                } else { value.push(current); }
                index += 1;
                column += 1;
            }
            if !closed {
                return Err(diagnostic(1002, "Unterminated string literal", Span {
                    start, end: index, line: token_line, column: token_column,
                }));
            }
            Kind::String(value)
        } else if ch == '=' && chars.get(index + 1) == Some(&'>') {
            index += 2;
            column += 2;
            Kind::Arrow
        } else if "()[]{}<>,:;?|&".contains(ch) {
            index += 1;
            column += 1;
            Kind::Punct(ch)
        } else {
            return Err(diagnostic(1127, format!("Invalid character '{ch}'"), Span {
                start, end: start + 1, line: token_line, column: token_column,
            }));
        };
        tokens.push(Token { kind, span: Span { start, end: index, line: token_line, column: token_column } });
    }
    tokens.push(Token { kind: Kind::Eof, span: Span { start: chars.len(), end: chars.len(), line, column } });
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    cursor: usize,
}

impl Parser {
    fn current(&self) -> &Token { &self.tokens[self.cursor] }
    fn advance(&mut self) -> Token { let token = self.current().clone(); self.cursor += 1; token }
    fn error(&self, code: u16, message: impl Into<String>) -> Diagnostic {
        diagnostic(code, message, self.current().span)
    }
    fn eat_punct(&mut self, punct: char) -> bool {
        if self.current().kind == Kind::Punct(punct) { self.advance(); true } else { false }
    }
    fn expect_punct(&mut self, punct: char) -> Result<(), Diagnostic> {
        if self.eat_punct(punct) { Ok(()) } else { Err(self.error(1005, format!("Expected '{punct}'"))) }
    }
    fn parse_type(&mut self) -> Result<Type, Diagnostic> { self.parse_union() }
    fn parse_union(&mut self) -> Result<Type, Diagnostic> {
        let mut members = vec![self.parse_intersection()?];
        while self.eat_punct('|') { members.push(self.parse_intersection()?); }
        Ok(if members.len() == 1 { members.pop().unwrap() } else { Type::Union(members) })
    }
    fn parse_intersection(&mut self) -> Result<Type, Diagnostic> {
        let mut members = vec![self.parse_postfix()?];
        while self.eat_punct('&') { members.push(self.parse_postfix()?); }
        Ok(if members.len() == 1 { members.pop().unwrap() } else { Type::Intersection(members) })
    }
    fn parse_postfix(&mut self) -> Result<Type, Diagnostic> {
        let mut ty = self.parse_primary()?;
        while self.eat_punct('[') {
            self.expect_punct(']')?;
            ty = Type::Array(Box::new(ty));
        }
        Ok(ty)
    }
    fn parse_primary(&mut self) -> Result<Type, Diagnostic> {
        match self.advance().kind {
            Kind::String(value) => Ok(Type::StringLiteral(value)),
            Kind::Number(value) => Ok(Type::NumberLiteral(value)),
            Kind::Ident(name) => self.parse_named(name),
            Kind::Punct('[') => self.parse_tuple(),
            Kind::Punct('{') => self.parse_object(),
            Kind::Punct('(') => self.parse_parenthesized_or_function(),
            _ => Err(self.error(1110, "Type expected")),
        }
    }
    fn parse_named(&mut self, name: String) -> Result<Type, Diagnostic> {
        let primitive = match name.as_str() {
            "any" => Some(Type::Any), "unknown" => Some(Type::Unknown), "never" => Some(Type::Never),
            "void" => Some(Type::Void), "undefined" => Some(Type::Undefined), "null" => Some(Type::Null),
            "boolean" => Some(Type::Boolean), "number" => Some(Type::Number), "bigint" => Some(Type::BigInt),
            "string" => Some(Type::String), _ => None,
        };
        if let Some(ty) = primitive { return Ok(ty); }
        let mut arguments = Vec::new();
        if self.eat_punct('<') {
            loop {
                arguments.push(self.parse_type()?);
                if !self.eat_punct(',') { break; }
            }
            self.expect_punct('>')?;
        }
        Ok(Type::Reference { name, arguments })
    }
    fn parse_tuple(&mut self) -> Result<Type, Diagnostic> {
        let mut elements = Vec::new();
        while !self.eat_punct(']') {
            elements.push(self.parse_type()?);
            if !self.eat_punct(',') { self.expect_punct(']')?; break; }
        }
        Ok(Type::Tuple(elements))
    }
    fn parse_object(&mut self) -> Result<Type, Diagnostic> {
        let mut properties = Vec::new();
        while !self.eat_punct('}') {
            let name = match self.advance().kind {
                Kind::Ident(name) | Kind::String(name) => name,
                _ => return Err(self.error(1131, "Property name expected")),
            };
            let optional = self.eat_punct('?');
            self.expect_punct(':')?;
            let ty = self.parse_type()?;
            properties.push(Property { name, optional, ty });
            if !self.eat_punct(';') && !self.eat_punct(',') { self.expect_punct('}')?; break; }
        }
        Ok(Type::Object(properties))
    }
    fn parse_parenthesized_or_function(&mut self) -> Result<Type, Diagnostic> {
        let saved = self.cursor;
        let mut parameters = Vec::new();
        let mut function_shape = true;
        while !self.eat_punct(')') {
            if !matches!(self.current().kind, Kind::Ident(_)) { function_shape = false; break; }
            self.advance();
            self.eat_punct('?');
            if !self.eat_punct(':') { function_shape = false; break; }
            parameters.push(self.parse_type()?);
            if !self.eat_punct(',') { self.expect_punct(')')?; break; }
        }
        if function_shape && matches!(self.current().kind, Kind::Arrow) {
            self.advance();
            return Ok(Type::Function { parameters, returns: Box::new(self.parse_type()?) });
        }
        self.cursor = saved;
        let ty = self.parse_type()?;
        self.expect_punct(')')?;
        Ok(ty)
    }
}

fn diagnostic(code: u16, message: impl Into<String>, span: Span) -> Diagnostic {
    Diagnostic { code, message: message.into(), span }
}

fn is_ident_start(ch: char) -> bool { ch == '_' || ch == '$' || ch.is_ascii_alphabetic() }
fn is_ident_part(ch: char) -> bool { is_ident_start(ch) || ch.is_ascii_digit() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_composite_types() {
        assert_eq!(
            parse_type_expression("{ id: number; label?: string; tags: Array<string> } | null").unwrap(),
            Type::Union(vec![
                Type::Object(vec![
                    Property { name: "id".into(), optional: false, ty: Type::Number },
                    Property { name: "label".into(), optional: true, ty: Type::String },
                    Property {
                        name: "tags".into(), optional: false,
                        ty: Type::Reference { name: "Array".into(), arguments: vec![Type::String] },
                    },
                ]),
                Type::Null,
            ])
        );
    }

    #[test]
    fn parses_function_tuple_and_array_types() {
        assert_eq!(
            parse_type_expression("(value: [number, string[]]) => boolean").unwrap(),
            Type::Function {
                parameters: vec![Type::Tuple(vec![Type::Number, Type::Array(Box::new(Type::String))])],
                returns: Box::new(Type::Boolean),
            }
        );
    }

    #[test]
    fn reports_source_location() {
        let error = parse_type_expression("{\n value number\n}").unwrap_err();
        assert_eq!((error.code, error.span.line), (1005, 2));
    }
}
