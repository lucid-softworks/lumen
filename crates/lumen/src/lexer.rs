//! Hand-written tokenizer. Produces the full `Vec<Token>` up front (scripts are small enough that
//! streaming buys nothing) and resolves the classic `/`-is-it-a-regex-or-division ambiguity by
//! tracking whether the previously emitted token can end an expression.

use crate::token::{Tok, Token, KEYWORDS, PUNCTUATORS};

pub struct LexError {
    pub message: String,
    pub line: u32,
}

struct Lexer<'a> {
    src: &'a [u8],
    chars: Vec<char>,
    pos: usize,
    line: u32,
    out: Vec<Token>,
    nl_pending: bool,
}

/// Tokenize `src`. A lex error is reported as a SyntaxError by the caller.
pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    let mut lx = Lexer {
        src: src.as_bytes(),
        chars: src.chars().collect(),
        pos: 0,
        line: 1,
        out: Vec::new(),
        nl_pending: false,
    };
    lx.run()?;
    Ok(lx.out)
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn peek2(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        if let Some(c) = c {
            self.pos += 1;
            if c == '\n' {
                self.line += 1;
            }
        }
        c
    }
    fn err(&self, message: impl Into<String>) -> LexError {
        LexError { message: message.into(), line: self.line }
    }

    /// Whether the previously emitted token permits a regex literal to follow (i.e. we are at the
    /// start of an expression). After a value-producing token (`)`, `]`, identifier, number, etc.)
    /// a `/` is division; otherwise it begins a regex.
    fn regex_allowed(&self) -> bool {
        match self.out.last().map(|t| &t.kind) {
            None => true,
            Some(Tok::Num(_) | Tok::Str(_) | Tok::Template(_) | Tok::Regex { .. }) => false,
            Some(Tok::Ident(_)) => false,
            Some(Tok::Keyword(k)) => !matches!(*k, "this" | "super" | "true" | "false" | "null"),
            Some(Tok::Punct(p)) => !matches!(*p, ")" | "]" | "}"),
            Some(Tok::Eof) => false,
        }
    }

    fn push(&mut self, kind: Tok) {
        let nl = self.nl_pending;
        self.nl_pending = false;
        self.out.push(Token { kind, line: self.line, nl_before: nl });
    }

    fn run(&mut self) -> Result<(), LexError> {
        while let Some(c) = self.peek() {
            if is_line_terminator(c) {
                self.nl_pending = true;
                self.bump();
            } else if c.is_whitespace() {
                self.bump();
            } else if c == '/' && self.peek2() == Some('/') {
                self.skip_line_comment();
            } else if c == '/' && self.peek2() == Some('*') {
                self.skip_block_comment()?;
            } else if c == '/' && self.regex_allowed() {
                self.read_regex()?;
            } else if c == '"' || c == '\'' {
                self.read_string(c)?;
            } else if c == '`' {
                self.read_template()?;
            } else if c.is_ascii_digit() || (c == '.' && self.peek2().is_some_and(|d| d.is_ascii_digit()))
            {
                self.read_number()?;
            } else if is_ident_start(c) || c == '#' {
                self.read_ident();
            } else {
                self.read_punct()?;
            }
        }
        self.push(Tok::Eof);
        Ok(())
    }

    fn skip_line_comment(&mut self) {
        while let Some(c) = self.peek() {
            if is_line_terminator(c) {
                break;
            }
            self.bump();
        }
    }

    fn skip_block_comment(&mut self) -> Result<(), LexError> {
        self.bump();
        self.bump();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated block comment")),
                Some('*') if self.peek() == Some('/') => {
                    self.bump();
                    return Ok(());
                }
                Some(c) if is_line_terminator(c) => self.nl_pending = true,
                _ => {}
            }
        }
    }

    fn read_ident(&mut self) {
        let mut s = String::new();
        // A leading `#` (private name) is part of the identifier but not an ident-continue char.
        if self.peek() == Some('#') {
            s.push('#');
            self.bump();
        }
        while let Some(c) = self.peek() {
            if is_ident_part(c) {
                s.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if let Some(kw) = KEYWORDS.iter().find(|k| **k == s) {
            self.push(Tok::Keyword(kw));
        } else {
            self.push(Tok::Ident(s));
        }
    }

    fn read_string(&mut self, quote: char) -> Result<(), LexError> {
        self.bump();
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string literal")),
                Some(c) if c == quote => break,
                Some('\\') => self.read_escape(&mut s)?,
                Some(c) if is_line_terminator(c) => {
                    return Err(self.err("unterminated string literal"))
                }
                Some(c) => s.push(c),
            }
        }
        self.push(Tok::Str(s));
        Ok(())
    }

    fn read_template(&mut self) -> Result<(), LexError> {
        self.bump();
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated template literal")),
                Some('`') => break,
                Some('$') if self.peek() == Some('{') => {
                    return Err(self.err("template substitutions ${...} are not supported yet"))
                }
                Some('\\') => self.read_escape(&mut s)?,
                Some(c) => s.push(c),
            }
        }
        self.push(Tok::Template(s));
        Ok(())
    }

    fn read_escape(&mut self, out: &mut String) -> Result<(), LexError> {
        match self.bump() {
            None => Err(self.err("unterminated escape")),
            Some('n') => {
                out.push('\n');
                Ok(())
            }
            Some('t') => {
                out.push('\t');
                Ok(())
            }
            Some('r') => {
                out.push('\r');
                Ok(())
            }
            Some('b') => {
                out.push('\u{0008}');
                Ok(())
            }
            Some('f') => {
                out.push('\u{000C}');
                Ok(())
            }
            Some('v') => {
                out.push('\u{000B}');
                Ok(())
            }
            Some('0') if !self.peek().is_some_and(|c| c.is_ascii_digit()) => {
                out.push('\0');
                Ok(())
            }
            Some('x') => {
                let hi = self.bump().ok_or_else(|| self.err("bad \\x escape"))?;
                let lo = self.bump().ok_or_else(|| self.err("bad \\x escape"))?;
                let n = u32::from_str_radix(&format!("{hi}{lo}"), 16)
                    .map_err(|_| self.err("bad \\x escape"))?;
                out.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                Ok(())
            }
            Some('u') => self.read_unicode_escape(out),
            Some(c) if is_line_terminator(c) => Ok(()), // line continuation
            Some(c) => {
                out.push(c);
                Ok(())
            }
        }
    }

    fn read_unicode_escape(&mut self, out: &mut String) -> Result<(), LexError> {
        let mut hex = String::new();
        if self.peek() == Some('{') {
            self.bump();
            while let Some(c) = self.peek() {
                if c == '}' {
                    break;
                }
                hex.push(c);
                self.bump();
            }
            if self.bump() != Some('}') {
                return Err(self.err("unterminated \\u{...} escape"));
            }
        } else {
            for _ in 0..4 {
                hex.push(self.bump().ok_or_else(|| self.err("bad \\u escape"))?);
            }
        }
        let n = u32::from_str_radix(&hex, 16).map_err(|_| self.err("bad \\u escape"))?;
        out.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
        Ok(())
    }

    fn read_number(&mut self) -> Result<(), LexError> {
        let start = self.pos;
        let mut radix = 10u32;
        if self.peek() == Some('0') {
            match self.peek2() {
                Some('x' | 'X') => radix = 16,
                Some('o' | 'O') => radix = 8,
                Some('b' | 'B') => radix = 2,
                _ => {}
            }
        }
        if radix != 10 {
            self.bump();
            self.bump();
            let digits_start = self.pos;
            while let Some(c) = self.peek() {
                if c == '_' || c.is_digit(radix) {
                    self.bump();
                } else {
                    break;
                }
            }
            let digits: String =
                self.chars[digits_start..self.pos].iter().filter(|c| **c != '_').collect();
            let n = u64::from_str_radix(&digits, radix)
                .map_err(|_| self.err("invalid numeric literal"))?;
            self.push(Tok::Num(n as f64));
            return Ok(());
        }
        // Decimal: integer . fraction e exponent
        while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '_') {
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '_') {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.bump();
            }
        }
        let text: String = self.chars[start..self.pos].iter().filter(|c| **c != '_').collect();
        let n: f64 = text.parse().map_err(|_| self.err("invalid numeric literal"))?;
        self.push(Tok::Num(n));
        Ok(())
    }

    fn read_regex(&mut self) -> Result<(), LexError> {
        self.bump(); // opening /
        let mut body = String::new();
        let mut in_class = false;
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated regular expression")),
                Some(c) if is_line_terminator(c) => {
                    return Err(self.err("unterminated regular expression"))
                }
                Some('\\') => {
                    body.push('\\');
                    if let Some(c) = self.bump() {
                        body.push(c);
                    }
                }
                Some('[') => {
                    in_class = true;
                    body.push('[');
                }
                Some(']') => {
                    in_class = false;
                    body.push(']');
                }
                Some('/') if !in_class => break,
                Some(c) => body.push(c),
            }
        }
        let mut flags = String::new();
        while let Some(c) = self.peek() {
            if is_ident_part(c) {
                flags.push(c);
                self.bump();
            } else {
                break;
            }
        }
        self.push(Tok::Regex { body, flags });
        Ok(())
    }

    fn read_punct(&mut self) -> Result<(), LexError> {
        let rest: String = self.chars[self.pos..(self.pos + 4).min(self.chars.len())].iter().collect();
        for p in PUNCTUATORS {
            if rest.starts_with(p) {
                for _ in 0..p.chars().count() {
                    self.bump();
                }
                self.push(Tok::Punct(p));
                return Ok(());
            }
        }
        let _ = self.src; // keep field used; byte view reserved for future fast paths
        Err(self.err(format!("unexpected character {:?}", self.peek().unwrap_or('\0'))))
    }
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}
fn is_ident_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphabetic()
}
fn is_ident_part(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric()
}
