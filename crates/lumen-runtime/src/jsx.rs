//! A from-scratch JSX → JavaScript transformer (no third-party crate). lumen's engine parses
//! JavaScript, not JSX, so the runtime lowers `.jsx` source to plain JS before evaluating it:
//! `<div a={x}>hi</div>` becomes `React.createElement("div", { a: x }, "hi")`.
//!
//! It is a single left-to-right scan. Ordinary JavaScript is copied through verbatim; the scanner
//! only needs enough lexical awareness to (a) skip over regions where `<` isn't JSX — strings,
//! template literals, comments, and regex literals — and (b) recognize when a `<` in expression
//! position begins a JSX element. JSX regions are parsed recursively (attribute and child
//! `{ … }` containers hold ordinary JS, which may contain more JSX) and re-emitted as factory
//! calls. The classic runtime is used, so a component's own `import React` supplies the factory.
//!
//! Scope: JSX, not TypeScript. A `.tsx` file's type annotations are not stripped here.

const FACTORY: &str = "React.createElement";
const FRAGMENT: &str = "React.Fragment";

/// Transform JSX source to JavaScript. Returns an error string on malformed JSX.
pub fn transform(src: &str) -> Result<String, String> {
    let mut t = T {
        c: src.chars().collect(),
        i: 0,
        out: String::with_capacity(src.len() + src.len() / 2),
        prev: Prev::Start,
    };
    t.scan_js(Stop::Eof)?;
    Ok(t.out)
}

/// What the previous significant token was — enough to disambiguate `<` (JSX vs less-than) and `/`
/// (regex vs divide), both of which begin only in *expression* position.
#[derive(Clone, Copy, PartialEq)]
enum Prev {
    /// Start of input, or after a punctuator/keyword that begins an expression (`(`, `=`, `return`,
    /// `=>`, …). A `<` or `/` here starts JSX / a regex.
    Start,
    /// After a value (identifier, `)`, `]`, literal). A `<` here is less-than; `/` is divide.
    Value,
}

#[derive(Clone, Copy, PartialEq)]
enum Stop {
    /// Scan to end of input.
    Eof,
    /// Scan a `{ … }` expression container's body; stop at the matching `}` (not consumed).
    Brace,
}

struct T {
    c: Vec<char>,
    i: usize,
    out: String,
    prev: Prev,
}

impl T {
    fn at(&self, k: usize) -> char {
        self.c.get(self.i + k).copied().unwrap_or('\0')
    }
    fn cur(&self) -> char {
        self.at(0)
    }
    fn done(&self) -> bool {
        self.i >= self.c.len()
    }
    fn bump_copy(&mut self) {
        self.out.push(self.c[self.i]);
        self.i += 1;
    }

    /// Scan JavaScript, copying it to the output and lowering any JSX it contains, until `stop`.
    fn scan_js(&mut self, stop: Stop) -> Result<(), String> {
        while !self.done() {
            let ch = self.cur();
            match ch {
                '}' if stop == Stop::Brace => return Ok(()),
                '/' if self.at(1) == '/' => self.copy_line_comment(),
                '/' if self.at(1) == '*' => self.copy_block_comment(),
                '/' if self.prev == Prev::Start => self.copy_regex(),
                '"' | '\'' => self.copy_string(ch),
                '`' => self.copy_template()?,
                '<' if self.prev == Prev::Start && starts_jsx(self.at(1)) => {
                    self.parse_element()?;
                    // A JSX element is a value, so a following `<` is less-than, `/` is divide.
                    self.prev = Prev::Value;
                }
                c if c.is_whitespace() => self.bump_copy(), // leaves `prev` unchanged
                c if c.is_alphabetic() || c == '_' || c == '$' => {
                    // Copy a whole identifier/keyword run at once: whether a keyword introduces an
                    // expression (`return`, `typeof`, …) decides if a following `<`/`/` is JSX/regex.
                    let start = self.i;
                    while !self.done() {
                        let w = self.cur();
                        if w.is_alphanumeric() || w == '_' || w == '$' {
                            self.bump_copy();
                        } else {
                            break;
                        }
                    }
                    let word: String = self.c[start..self.i].iter().collect();
                    self.prev = if is_expr_keyword(&word) { Prev::Start } else { Prev::Value };
                }
                c if c.is_ascii_digit() => {
                    // A numeric literal is a value; copy its run (digits, `.`, exponent, radix).
                    while !self.done() {
                        let d = self.cur();
                        if d.is_alphanumeric() || d == '.' || d == '_' {
                            self.bump_copy();
                        } else {
                            break;
                        }
                    }
                    self.prev = Prev::Value;
                }
                // A punctuator: most introduce an expression; closers are values.
                _ => {
                    self.prev = match ch {
                        ')' | ']' | '}' => Prev::Value,
                        _ => Prev::Start,
                    };
                    self.bump_copy();
                }
            }
        }
        if stop == Stop::Brace {
            return Err("unterminated `{ … }` in JSX".to_string());
        }
        Ok(())
    }

    fn copy_line_comment(&mut self) {
        while !self.done() && self.cur() != '\n' {
            self.bump_copy();
        }
    }

    fn copy_block_comment(&mut self) {
        self.bump_copy(); // '/'
        self.bump_copy(); // '*'
        while !(self.done() || (self.cur() == '*' && self.at(1) == '/')) {
            self.bump_copy();
        }
        if !self.done() {
            self.bump_copy(); // '*'
            self.bump_copy(); // '/'
        }
    }

    fn copy_string(&mut self, quote: char) {
        self.bump_copy(); // opening quote
        while !self.done() {
            let ch = self.cur();
            if ch == '\\' {
                self.bump_copy();
                if !self.done() {
                    self.bump_copy();
                }
                continue;
            }
            self.bump_copy();
            if ch == quote {
                break;
            }
        }
        self.prev = Prev::Value;
    }

    fn copy_template(&mut self) -> Result<(), String> {
        self.bump_copy(); // opening backtick
        while !self.done() {
            let ch = self.cur();
            if ch == '\\' {
                self.bump_copy();
                if !self.done() {
                    self.bump_copy();
                }
                continue;
            }
            if ch == '`' {
                self.bump_copy();
                break;
            }
            if ch == '$' && self.at(1) == '{' {
                self.bump_copy(); // '$'
                self.bump_copy(); // '{'
                self.prev = Prev::Start;
                self.scan_js(Stop::Brace)?; // the substitution is ordinary JS (may hold JSX)
                if self.cur() == '}' {
                    self.bump_copy();
                }
                continue;
            }
            self.bump_copy();
        }
        self.prev = Prev::Value;
        Ok(())
    }

    fn copy_regex(&mut self) {
        self.bump_copy(); // '/'
        let mut in_class = false;
        while !self.done() {
            let ch = self.cur();
            match ch {
                '\\' => {
                    self.bump_copy();
                    if !self.done() {
                        self.bump_copy();
                    }
                    continue;
                }
                '[' => in_class = true,
                ']' => in_class = false,
                '/' if !in_class => {
                    self.bump_copy();
                    break;
                }
                _ => {}
            }
            self.bump_copy();
        }
        // Copy flags.
        while !self.done() && (self.cur().is_alphabetic()) {
            self.bump_copy();
        }
        self.prev = Prev::Value;
    }

    // ---- JSX ---------------------------------------------------------------------------------

    /// Parse one JSX element or fragment at the current `<` and emit the factory call.
    fn parse_element(&mut self) -> Result<(), String> {
        self.i += 1; // consume '<'
        self.skip_ws_and_comments();

        // Fragment: `<> … </>`.
        if self.cur() == '>' {
            self.i += 1;
            self.out.push_str(FACTORY);
            self.out.push('(');
            self.out.push_str(FRAGMENT);
            self.out.push_str(", null");
            self.emit_children("")?;
            self.out.push(')');
            return Ok(());
        }

        let name = self.read_element_name();
        if name.is_empty() {
            return Err("expected a JSX element name after `<`".to_string());
        }

        self.out.push_str(FACTORY);
        self.out.push('(');
        self.emit_type(&name);
        self.out.push_str(", ");
        let self_closing = self.emit_attributes()?;
        if self_closing {
            self.out.push(')');
        } else {
            self.emit_children(&name)?;
            self.out.push(')');
        }
        Ok(())
    }

    /// A JSX element name: `div`, `Foo`, `a.b.c` (member), `svg:rect` (namespaced).
    fn read_element_name(&mut self) -> String {
        let mut s = String::new();
        while !self.done() {
            let ch = self.cur();
            if ch.is_alphanumeric() || ch == '_' || ch == '$' || ch == '.' || ch == '-' || ch == ':'
            {
                s.push(ch);
                self.i += 1;
            } else {
                break;
            }
        }
        s
    }

    /// The element type argument: a lowercase, dot-free, colon-free name is an intrinsic tag
    /// (string literal); anything else (capitalized, `a.b`, …) is an identifier/member expression.
    fn emit_type(&mut self, name: &str) {
        let intrinsic = name
            .chars()
            .next()
            .map(|c| c.is_ascii_lowercase())
            .unwrap_or(false)
            && !name.contains('.')
            && !name.contains(':');
        if intrinsic {
            self.out.push('"');
            self.out.push_str(name);
            self.out.push('"');
        } else {
            self.out.push_str(name);
        }
    }

    /// Emit the props object (or `null`), returning whether the tag self-closed (`/>`).
    fn emit_attributes(&mut self) -> Result<bool, String> {
        let mut props: Vec<String> = Vec::new();
        loop {
            self.skip_ws_and_comments();
            match self.cur() {
                '\0' => return Err("unterminated JSX opening tag".to_string()),
                '/' if self.at(1) == '>' => {
                    self.i += 2;
                    self.write_props(&props);
                    return Ok(true);
                }
                '>' => {
                    self.i += 1;
                    self.write_props(&props);
                    return Ok(false);
                }
                '{' => {
                    // Spread attribute: `{...expr}`.
                    self.i += 1;
                    self.skip_ws_and_comments();
                    if self.cur() == '.' && self.at(1) == '.' && self.at(2) == '.' {
                        self.i += 3;
                    }
                    let expr = self.capture_brace_expr()?;
                    props.push(format!("...{}", expr.trim()));
                }
                _ => {
                    let attr = self.read_attribute_name();
                    if attr.is_empty() {
                        return Err("expected a JSX attribute name".to_string());
                    }
                    self.skip_ws_and_comments();
                    if self.cur() == '=' {
                        self.i += 1;
                        self.skip_ws_and_comments();
                        let value = self.read_attribute_value()?;
                        props.push(format!("{}: {}", quote_key(&attr), value));
                    } else {
                        // Bare attribute → `true`.
                        props.push(format!("{}: true", quote_key(&attr)));
                    }
                }
            }
        }
    }

    fn write_props(&mut self, props: &[String]) {
        if props.is_empty() {
            self.out.push_str("null");
        } else {
            self.out.push_str("{ ");
            self.out.push_str(&props.join(", "));
            self.out.push_str(" }");
        }
    }

    fn read_attribute_name(&mut self) -> String {
        let mut s = String::new();
        while !self.done() {
            let ch = self.cur();
            if ch.is_alphanumeric() || ch == '_' || ch == '-' || ch == ':' {
                s.push(ch);
                self.i += 1;
            } else {
                break;
            }
        }
        s
    }

    fn read_attribute_value(&mut self) -> Result<String, String> {
        match self.cur() {
            '"' | '\'' => Ok(self.read_jsx_string()),
            '{' => {
                self.i += 1;
                let expr = self.capture_brace_expr()?;
                Ok(expr.trim().to_string())
            }
            '<' => {
                // An element-valued attribute: `attr=<Foo/>`.
                let start = self.out.len();
                self.parse_element()?;
                let emitted = self.out.split_off(start);
                Ok(emitted)
            }
            _ => Err("expected a JSX attribute value".to_string()),
        }
    }

    /// A quoted JSX attribute string → a JS string literal (JSX strings don't process escapes).
    fn read_jsx_string(&mut self) -> String {
        let quote = self.cur();
        self.i += 1;
        let mut s = String::new();
        while !self.done() && self.cur() != quote {
            s.push(self.cur());
            self.i += 1;
        }
        if !self.done() {
            self.i += 1; // closing quote
        }
        json_string(&s)
    }

    /// Capture a `{ … }` expression body (the opening `{` already consumed), transforming any
    /// nested JSX, and consume the closing `}`. Returns the transformed expression text.
    fn capture_brace_expr(&mut self) -> Result<String, String> {
        let start = self.out.len();
        self.prev = Prev::Start;
        self.scan_js(Stop::Brace)?;
        let expr = self.out.split_off(start);
        if self.cur() != '}' {
            return Err("unterminated `{ … }` in JSX".to_string());
        }
        self.i += 1; // closing '}'
        Ok(expr)
    }

    /// Emit the children of an element (or fragment when `close` is empty), each preceded by
    /// `, `. Consumes through the closing tag.
    fn emit_children(&mut self, close: &str) -> Result<(), String> {
        loop {
            // Raw text up to the next `<` or `{`.
            let mut text = String::new();
            while !self.done() && self.cur() != '<' && self.cur() != '{' {
                text.push(self.cur());
                self.i += 1;
            }
            if let Some(lit) = clean_jsx_text(&text) {
                self.out.push_str(", ");
                self.out.push_str(&json_string(&lit));
            }

            match self.cur() {
                '\0' => return Err("unterminated JSX element".to_string()),
                '{' => {
                    self.i += 1;
                    // An empty `{}` / whitespace-or-comment-only container yields no child.
                    let expr = self.capture_brace_expr()?;
                    let trimmed = expr.trim();
                    if !trimmed.is_empty() {
                        self.out.push_str(", ");
                        self.out.push_str(trimmed);
                    }
                }
                '<' if self.at(1) == '/' => {
                    // Closing tag `</name>` (or `</>`): consume and finish.
                    self.i += 2;
                    let _ = self.read_element_name();
                    self.skip_ws_and_comments();
                    if self.cur() == '>' {
                        self.i += 1;
                    }
                    let _ = close;
                    return Ok(());
                }
                '<' => {
                    self.out.push_str(", ");
                    self.parse_element()?;
                }
                _ => unreachable!(),
            }
        }
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while !self.done() && self.cur().is_whitespace() {
                self.i += 1;
            }
            if self.cur() == '/' && self.at(1) == '/' {
                while !self.done() && self.cur() != '\n' {
                    self.i += 1;
                }
            } else if self.cur() == '/' && self.at(1) == '*' {
                self.i += 2;
                while !(self.done() || (self.cur() == '*' && self.at(1) == '/')) {
                    self.i += 1;
                }
                self.i += 2;
            } else {
                break;
            }
        }
    }
}

/// Whether `c` can begin a JSX element name / fragment / closing tag after `<`.
fn starts_jsx(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$' || c == '>' || c == '/'
}

/// Keywords after which an expression (and thus a JSX element or regex) may follow. After any
/// other identifier — a variable, `this`, `true` — a `<` is a comparison and `/` is division.
fn is_expr_keyword(word: &str) -> bool {
    matches!(
        word,
        "return"
            | "typeof"
            | "instanceof"
            | "in"
            | "of"
            | "new"
            | "do"
            | "else"
            | "yield"
            | "await"
            | "void"
            | "delete"
            | "case"
            | "throw"
            | "default"
            | "extends"
    )
}

/// An attribute key: a plain identifier stays bare, anything else (containing `-`/`:`) is quoted.
fn quote_key(name: &str) -> String {
    let ident = !name.is_empty()
        && name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
            .unwrap_or(false)
        && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    if ident {
        name.to_string()
    } else {
        json_string(name)
    }
}

/// A double-quoted JS string literal for `s`.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Normalize JSX text per the JSX whitespace rules: within each line strip the leading whitespace
/// (except the first line) and trailing whitespace (except the last), drop now-empty lines, and
/// join the rest with a single space. Returns `None` if nothing remains (whitespace-only).
fn clean_jsx_text(text: &str) -> Option<String> {
    if !text.contains('\n') {
        // Single line: keep as-is only if it isn't pure whitespace.
        return if text.trim().is_empty() { None } else { Some(text.to_string()) };
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len() - 1;
    let mut parts: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let mut l = *line;
        if i != 0 {
            l = l.trim_start();
        }
        if i != last {
            l = l.trim_end();
        }
        if !l.is_empty() {
            parts.push(l.to_string());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::transform;

    fn t(src: &str) -> String {
        transform(src).unwrap()
    }

    #[test]
    fn intrinsic_and_component() {
        assert_eq!(t("<div/>"), r#"React.createElement("div", null)"#);
        assert_eq!(t("<Foo/>"), "React.createElement(Foo, null)");
        assert_eq!(t("<a.b.C/>"), "React.createElement(a.b.C, null)");
    }

    #[test]
    fn attributes() {
        assert_eq!(
            t(r#"<div id="x" n={1+2} flag/>"#),
            r#"React.createElement("div", { id: "x", n: 1+2, flag: true })"#
        );
        assert_eq!(
            t("<div {...props} a={1}/>"),
            "React.createElement(\"div\", { ...props, a: 1 })"
        );
    }

    #[test]
    fn children_text_and_expr() {
        assert_eq!(
            t("<h1>hello</h1>"),
            r#"React.createElement("h1", null, "hello")"#
        );
        assert_eq!(
            t("<p>{name}!</p>"),
            r#"React.createElement("p", null, name, "!")"#
        );
    }

    #[test]
    fn nested_and_fragment() {
        assert_eq!(
            t("<><a/><b/></>"),
            "React.createElement(React.Fragment, null, React.createElement(\"a\", null), React.createElement(\"b\", null))"
        );
    }

    #[test]
    fn whitespace_between_tags_dropped() {
        let out = t("<ul>\n  <li>a</li>\n  <li>b</li>\n</ul>");
        assert_eq!(
            out,
            "React.createElement(\"ul\", null, React.createElement(\"li\", null, \"a\"), React.createElement(\"li\", null, \"b\"))"
        );
    }

    #[test]
    fn less_than_is_not_jsx() {
        assert_eq!(t("const b = a < 3;"), "const b = a < 3;");
        assert_eq!(t("if (x<y) {}"), "if (x<y) {}");
    }

    #[test]
    fn jsx_in_map_callback() {
        assert_eq!(
            t("items.map(x => <li>{x}</li>)"),
            "items.map(x => React.createElement(\"li\", null, x))"
        );
    }

    #[test]
    fn text_with_apostrophe() {
        assert_eq!(
            t("<p>don't</p>"),
            "React.createElement(\"p\", null, \"don't\")"
        );
    }
}
