//! Hand-written tokenizer. Produces the full `Vec<Token>` up front (scripts are small enough that
//! streaming buys nothing) and resolves the classic `/`-is-it-a-regex-or-division ambiguity by
//! tracking whether the previously emitted token can end an expression.

use crate::token::{Tok, Token, TplPart, KEYWORDS, PUNCTUATORS};

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
    /// Set while reading a string that contained a legacy octal / `\8` / `\9` escape.
    pending_legacy: bool,
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
        pending_legacy: false,
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
            Some(
                Tok::Num(_) | Tok::BigInt(_) | Tok::Str(_) | Tok::Template(_) | Tok::Regex { .. },
            ) => false,
            Some(Tok::Ident(_)) => false,
            Some(Tok::Keyword(k)) => !matches!(*k, "this" | "super" | "true" | "false" | "null"),
            Some(Tok::Punct(p)) => !matches!(*p, ")" | "]" | "}"),
            Some(Tok::Eof) => false,
        }
    }

    fn push(&mut self, kind: Tok) {
        let nl = self.nl_pending;
        self.nl_pending = false;
        self.out.push(Token { kind, line: self.line, nl_before: nl, legacy_octal: false });
    }
    /// Flag the most recently pushed token as a legacy-octal construct.
    fn mark_legacy_octal(&mut self) {
        if let Some(t) = self.out.last_mut() {
            t.legacy_octal = true;
        }
    }

    fn run(&mut self) -> Result<(), LexError> {
        // Hashbang comment: `#!...` only at the very start of the source.
        if self.peek() == Some('#') && self.peek2() == Some('!') {
            while let Some(c) = self.peek() {
                if is_line_terminator(c) {
                    break;
                }
                self.bump();
            }
        }
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
            } else if is_ident_start(c) || c == '#' || (c == '\\' && self.peek2() == Some('u')) {
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
        // `\uXXXX` / `\u{...}` escapes may appear in an identifier; track that so an escaped reserved
        // word stays an Identifier (a keyword written with an escape is not the keyword).
        let mut had_escape = false;
        loop {
            match self.peek() {
                Some('\\') if self.peek2() == Some('u') => {
                    self.bump();
                    self.bump();
                    match self.read_unicode_escape_char() {
                        Some(ch) => {
                            had_escape = true;
                            s.push(ch);
                        }
                        None => break,
                    }
                }
                Some(c) if is_ident_part(c) => {
                    s.push(c);
                    self.bump();
                }
                _ => break,
            }
        }
        if !had_escape {
            if let Some(kw) = KEYWORDS.iter().find(|k| **k == s) {
                self.push(Tok::Keyword(kw));
                return;
            }
        }
        self.push(Tok::Ident(s));
    }

    /// Read the body of a `\u` identifier/string escape (already consumed `\u`): either `{HEX+}` or
    /// exactly four hex digits, yielding the code point as a `char`.
    fn read_unicode_escape_char(&mut self) -> Option<char> {
        let mut hex = String::new();
        if self.peek() == Some('{') {
            self.bump();
            while let Some(c) = self.peek() {
                if c == '}' {
                    self.bump();
                    break;
                } else if c.is_ascii_hexdigit() {
                    hex.push(c);
                    self.bump();
                } else {
                    return None;
                }
            }
        } else {
            for _ in 0..4 {
                match self.peek() {
                    Some(c) if c.is_ascii_hexdigit() => {
                        hex.push(c);
                        self.bump();
                    }
                    _ => return None,
                }
            }
        }
        u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
    }

    fn read_string(&mut self, quote: char) -> Result<(), LexError> {
        self.bump();
        let mut s = String::new();
        self.pending_legacy = false;
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
        if self.pending_legacy {
            self.mark_legacy_octal();
            self.pending_legacy = false;
        }
        Ok(())
    }

    fn read_template(&mut self) -> Result<(), LexError> {
        self.bump(); // opening backtick
        let mut parts: Vec<TplPart> = Vec::new();
        let mut cooked = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated template literal")),
                Some('`') => break,
                Some('\\') => self.read_escape(&mut cooked)?,
                Some('$') if self.peek() == Some('{') => {
                    self.bump(); // consume '{'
                    parts.push(TplPart::Str(std::mem::take(&mut cooked)));
                    parts.push(TplPart::Sub(self.read_template_sub()?));
                }
                Some(c) => cooked.push(c),
            }
        }
        parts.push(TplPart::Str(cooked));
        self.push(Tok::Template(parts));
        Ok(())
    }

    /// Read the raw source inside a `${ ... }` hole, returning it verbatim for the parser to
    /// sub-parse. Tracks `{}` nesting and skips over string/template literals so their braces and
    /// backticks don't confuse the matching.
    fn read_template_sub(&mut self) -> Result<String, LexError> {
        let mut src = String::new();
        let mut depth = 0i32;
        // Last non-whitespace char emitted, to disambiguate `/` (regex vs division) the same way the
        // main lexer does — so quotes/braces inside a regex literal don't confuse the brace scan.
        let mut last_sig: Option<char> = None;
        loop {
            // Comments: copy verbatim (their `'"{}` are inert).
            if self.peek() == Some('/') && self.peek2() == Some('/') {
                while let Some(c) = self.peek() {
                    if is_line_terminator(c) {
                        break;
                    }
                    src.push(c);
                    self.bump();
                }
                continue;
            }
            if self.peek() == Some('/') && self.peek2() == Some('*') {
                src.push_str("/*");
                self.bump();
                self.bump();
                loop {
                    match self.bump() {
                        None => return Err(self.err("unterminated comment in template")),
                        Some('*') if self.peek() == Some('/') => {
                            src.push_str("*/");
                            self.bump();
                            break;
                        }
                        Some(c) => src.push(c),
                    }
                }
                continue;
            }
            // Regex literal: copy verbatim (its `'"{}` are inert).
            if self.peek() == Some('/') && regex_allowed_after(last_sig) {
                self.copy_regex(&mut src)?;
                last_sig = Some(')'); // a regex is a value: a following `/` is division
                continue;
            }
            match self.bump() {
                None => return Err(self.err("unterminated template substitution")),
                Some('}') if depth == 0 => return Ok(src),
                Some('}') => {
                    depth -= 1;
                    src.push('}');
                    last_sig = Some('}');
                }
                Some('{') => {
                    depth += 1;
                    src.push('{');
                    last_sig = Some('{');
                }
                Some(q @ ('"' | '\'')) => {
                    self.copy_quoted(q, &mut src)?;
                    last_sig = Some(')'); // string is a value
                }
                Some('`') => {
                    src.push('`');
                    // Nested template: copy verbatim to its closing backtick (one level).
                    loop {
                        match self.bump() {
                            None => return Err(self.err("unterminated nested template")),
                            Some('\\') => {
                                src.push('\\');
                                if let Some(c) = self.bump() {
                                    src.push(c);
                                }
                            }
                            Some('`') => {
                                src.push('`');
                                break;
                            }
                            Some(c) => src.push(c),
                        }
                    }
                    last_sig = Some(')');
                }
                Some(c) => {
                    src.push(c);
                    if !c.is_whitespace() {
                        last_sig = Some(c);
                    }
                }
            }
        }
    }

    /// Copy a string literal (already past the opening `quote` is NOT consumed — we push it) into
    /// `out`, respecting escapes, up to and including the matching quote.
    fn copy_quoted(&mut self, quote: char, out: &mut String) -> Result<(), LexError> {
        out.push(quote);
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string in template")),
                Some('\\') => {
                    out.push('\\');
                    if let Some(c) = self.bump() {
                        out.push(c);
                    }
                }
                Some(c) if c == quote => {
                    out.push(c);
                    return Ok(());
                }
                Some(c) => out.push(c),
            }
        }
    }

    /// Copy a regex literal `/body/flags` verbatim into `out` (the leading `/` is at the cursor).
    fn copy_regex(&mut self, out: &mut String) -> Result<(), LexError> {
        out.push('/');
        self.bump(); // opening /
        let mut in_class = false;
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated regex in template")),
                Some('\\') => {
                    out.push('\\');
                    if let Some(c) = self.bump() {
                        out.push(c);
                    }
                }
                Some('[') => {
                    in_class = true;
                    out.push('[');
                }
                Some(']') => {
                    in_class = false;
                    out.push(']');
                }
                Some('/') if !in_class => {
                    out.push('/');
                    break;
                }
                Some(c) => out.push(c),
            }
        }
        while let Some(c) = self.peek() {
            if is_ident_part(c) {
                out.push(c);
                self.bump();
            } else {
                break;
            }
        }
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
            // Legacy octal escape: 1-3 octal digits (first three only if value <= 0o377).
            Some(c @ '0'..='7') => {
                let mut val = c.to_digit(8).unwrap();
                let max = if c <= '3' { 2 } else { 1 };
                let mut taken = 0;
                while taken < max && self.peek().is_some_and(|d| ('0'..='7').contains(&d)) {
                    val = val * 8 + self.bump().unwrap().to_digit(8).unwrap();
                    taken += 1;
                }
                out.push(char::from_u32(val).unwrap_or('\u{FFFD}'));
                self.pending_legacy = true;
                Ok(())
            }
            // `\8` / `\9` (NonOctalDecimalEscape): the digit itself, but still legacy.
            Some(c @ ('8' | '9')) => {
                out.push(c);
                self.pending_legacy = true;
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
            if self.peek() == Some('n') {
                self.bump();
                let n = i128::from_str_radix(&digits, radix)
                    .map_err(|_| self.err("invalid BigInt literal"))?;
                self.push(Tok::BigInt(n));
                return Ok(());
            }
            let n = u64::from_str_radix(&digits, radix)
                .map_err(|_| self.err("invalid numeric literal"))?;
            self.push(Tok::Num(n as f64));
            return Ok(());
        }
        // Decimal: integer . fraction e exponent
        while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '_') {
            self.bump();
        }
        // Legacy octal (`010`) / non-octal-decimal (`08`): a leading-zero integer with no fraction,
        // exponent, or `n` suffix. Octal value in sloppy mode; the parser rejects it in strict.
        if self.chars[start] == '0'
            && self.pos - start > 1
            && !matches!(self.peek(), Some('.' | 'e' | 'E' | 'n' | '_'))
        {
            let text: String = self.chars[start..self.pos].iter().collect();
            if text.chars().all(|c| ('0'..='7').contains(&c)) {
                let n = i64::from_str_radix(&text, 8).unwrap_or(0);
                self.push(Tok::Num(n as f64));
                self.mark_legacy_octal();
                return Ok(());
            } else if text.chars().all(|c| c.is_ascii_digit()) {
                let n: f64 = text.parse().unwrap_or(0.0);
                self.push(Tok::Num(n));
                self.mark_legacy_octal();
                return Ok(());
            }
        }
        // A BigInt literal is an integer immediately followed by `n` (no fraction/exponent).
        if self.peek() == Some('n') {
            let text: String = self.chars[start..self.pos].iter().filter(|c| **c != '_').collect();
            self.bump(); // n
            let n: i128 = text.parse().map_err(|_| self.err("invalid BigInt literal"))?;
            self.push(Tok::BigInt(n));
            return Ok(());
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
        // `?.` followed by a digit is `?` then `.` (a conditional like `x ? .5 : .3`), not optional
        // chaining.
        if rest.starts_with("?.") && self.chars.get(self.pos + 2).is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
            self.push(Tok::Punct("?"));
            return Ok(());
        }
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

/// Whether a `/` following `last` (the previous significant char) starts a regex rather than being
/// a division operator. A value-terminator (identifier char, `)`, `]`, `}`) means division.
fn regex_allowed_after(last: Option<char>) -> bool {
    match last {
        None => true,
        Some(c) => !(c.is_alphanumeric() || matches!(c, '_' | '$' | ')' | ']' | '}')),
    }
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}
fn is_ident_start(c: char) -> bool {
    // ID_Start ≈ alphabetic + a few Other_ID_Start characters.
    c == '_'
        || c == '$'
        || c.is_alphabetic()
        || matches!(c, '\u{1885}' | '\u{1886}' | '\u{2118}' | '\u{212E}' | '\u{309B}' | '\u{309C}')
}
fn is_ident_part(c: char) -> bool {
    // ZWNJ / ZWJ are valid IdentifierPart characters.
    // ID_Continue ≈ ID_Start + combining marks (Mn/Mc) + connector punctuation + a few extras.
    c == '_'
        || c == '$'
        || c == '\u{200C}'
        || c == '\u{200D}'
        || c.is_alphanumeric()
        || is_ident_start(c)
        || matches!(c,
            '\u{00B7}' | '\u{0387}' | '\u{19DA}'
            | '\u{0300}'..='\u{036F}'  // combining diacritical marks (Mn)
            | '\u{0483}'..='\u{0489}'
            | '\u{0591}'..='\u{05BD}'
            | '\u{0610}'..='\u{061A}'
            | '\u{064B}'..='\u{065F}'
            | '\u{0670}'
            | '\u{06D6}'..='\u{06DC}'
            | '\u{0E31}' | '\u{0E34}'..='\u{0E3A}'
            | '\u{203F}' | '\u{2040}' | '\u{2054}'  // connector punctuation (Pc)
            | '\u{FE33}' | '\u{FE34}' | '\u{FE4D}'..='\u{FE4F}' | '\u{FF3F}'
        )
}
