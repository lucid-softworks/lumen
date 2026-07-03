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
    /// Set while reading a string that contained a lone (unpaired) surrogate `\u` escape.
    pending_lone_surrogate: bool,
    /// One entry per open `{`: `true` if it opened a block/function body (statement position),
    /// `false` if an object literal (expression position). Used to disambiguate a `/` after `}`.
    brace_stack: Vec<bool>,
    /// Classification of the most recently closed `}` (`true` = a block). A `/` after a block-closing
    /// `}` begins a regex; after an object-literal-closing `}` it is division.
    last_close_block: bool,
    /// One entry per `function` keyword whose body has not opened yet: `true` if the function is an
    /// *expression* (so its body `}` is followed by division), `false` if a *declaration* (its `}`
    /// ends a statement, so a `/` after it is a regex).
    pending_fn: Vec<bool>,
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
        pending_lone_surrogate: false,
        brace_stack: Vec::new(),
        last_close_block: false,
        pending_fn: Vec::new(),
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
    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
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
        LexError {
            message: message.into(),
            line: self.line,
        }
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
            // `await`/`yield` are contextual: when they are keywords (module top level, async or
            // generator bodies) they prefix an expression, so a following `/` starts a regex. They
            // are `Ident` tokens here since the lexer lacks that context; allow the regex form —
            // division right after a bare `await`/`yield` *identifier* is vanishingly rare.
            Some(Tok::Ident(w)) => matches!(w.as_str(), "await" | "yield"),
            Some(Tok::Keyword(k)) => !matches!(*k, "this" | "super" | "true" | "false" | "null"),
            // A `/` after `)` or `]` is division; after `}` it depends on whether the `}` closed a
            // block (statement → regex) or an object literal (value → division).
            Some(Tok::Punct(p)) => match *p {
                ")" | "]" => false,
                "}" => self.last_close_block,
                _ => true,
            },
            Some(Tok::Eof) => false,
        }
    }

    /// Classify an opening `{` (about to be pushed): does the matching `}` end at statement position
    /// (a block, control body, or function *declaration* body → a following `/` is a regex) or at
    /// value position (an object literal or function *expression* body → division)?
    fn brace_ends_statement(&mut self) -> bool {
        let prev_is_paren = matches!(self.out.last().map(|t| &t.kind), Some(Tok::Punct(")")));
        if prev_is_paren {
            // A body after `)` — either a function body (classified by `pending_fn`) or a control
            // block / method (statement position).
            return match self.pending_fn.pop() {
                Some(is_expr) => !is_expr,
                None => true,
            };
        }
        match self.out.last().map(|t| &t.kind) {
            None => true,
            Some(Tok::Punct(p)) => matches!(*p, ";" | "{" | "}" | "=>"),
            Some(Tok::Keyword(k)) => matches!(*k, "else" | "do" | "try" | "finally"),
            _ => false,
        }
    }

    /// Whether a `function` keyword (about to be pushed) sits in statement position (a declaration)
    /// rather than expression position.
    fn function_is_declaration(&self) -> bool {
        match self.out.last().map(|t| &t.kind) {
            None => true,
            Some(Tok::Punct(p)) => match *p {
                ";" | "{" => true,
                "}" => self.last_close_block,
                _ => false,
            },
            Some(Tok::Keyword(k)) => {
                matches!(*k, "else" | "do" | "try" | "finally" | "export" | "default")
            }
            _ => false,
        }
    }

    fn push(&mut self, kind: Tok) {
        match &kind {
            Tok::Keyword(k) if *k == "function" => {
                let is_expr = !self.function_is_declaration();
                self.pending_fn.push(is_expr);
            }
            Tok::Punct(p) if *p == "{" => {
                let ends_stmt = self.brace_ends_statement();
                self.brace_stack.push(ends_stmt);
            }
            Tok::Punct(p) if *p == "}" => {
                self.last_close_block = self.brace_stack.pop().unwrap_or(false);
            }
            _ => {}
        }
        let nl = self.nl_pending;
        self.nl_pending = false;
        self.out.push(Token {
            kind,
            line: self.line,
            nl_before: nl,
            legacy_octal: false,
            escaped: false,
            lone_surrogate: false,
        });
    }
    /// Flag the most recently pushed token as having contained a `\u` escape.
    fn mark_escaped(&mut self) {
        if let Some(t) = self.out.last_mut() {
            t.escaped = true;
        }
    }
    /// Flag the most recently pushed token as containing a lone surrogate.
    fn mark_lone_surrogate(&mut self) {
        if let Some(t) = self.out.last_mut() {
            t.lone_surrogate = true;
        }
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
            } else if c == '<'
                && self.peek_at(1) == Some('!')
                && self.peek_at(2) == Some('-')
                && self.peek_at(3) == Some('-')
            {
                // Annex B HTML-like comment: `<!--` opens a single-line comment.
                self.skip_line_comment();
            } else if c == '-'
                && self.peek_at(1) == Some('-')
                && self.peek_at(2) == Some('>')
                && (self.nl_pending || self.out.is_empty())
            {
                // Annex B: `-->` at the start of a line (or of the source) is a comment to EOL.
                self.skip_line_comment();
            } else if c == '/' && self.regex_allowed() {
                self.read_regex()?;
            } else if c == '"' || c == '\'' {
                self.read_string(c)?;
            } else if c == '`' {
                self.read_template()?;
            } else if c.is_ascii_digit()
                || (c == '.' && self.peek2().is_some_and(|d| d.is_ascii_digit()))
            {
                self.read_number()?;
            } else if is_ident_start(c) || c == '#' || (c == '\\' && self.peek2() == Some('u')) {
                self.read_ident()?;
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

    fn read_ident(&mut self) -> Result<(), LexError> {
        let mut s = String::new();
        // A leading `#` (private name) is part of the identifier but not an ident-continue char.
        if self.peek() == Some('#') {
            s.push('#');
            self.bump();
        }
        // `\uXXXX` / `\u{...}` escapes may appear in an identifier; track that so an escaped reserved
        // word stays an Identifier (a keyword written with an escape is not the keyword).
        let mut had_escape = false;
        // The first code point must be IdentifierStart; the rest IdentifierPart. This holds for an
        // escaped code point too — so `#x` (escaped `#`) and a leading combining mark are errors.
        let mut first = true;
        loop {
            match self.peek() {
                Some('\\') if self.peek2() == Some('u') => {
                    self.bump();
                    self.bump();
                    match self.read_unicode_escape_char() {
                        Some(ch) => {
                            let ok = if first {
                                is_ident_start(ch)
                            } else {
                                is_ident_part(ch)
                            };
                            if !ok {
                                return Err(self.err("invalid character in escaped identifier"));
                            }
                            had_escape = true;
                            s.push(ch);
                            first = false;
                        }
                        None => return Err(self.err("invalid unicode escape in identifier")),
                    }
                }
                Some(c) if (first && is_ident_start(c)) || (!first && is_ident_part(c)) => {
                    s.push(c);
                    self.bump();
                    first = false;
                }
                _ => break,
            }
        }
        // A reserved word is always a keyword — even spelled with a `\u` escape. An escaped reserved
        // word can't be an Identifier (the parser rejects a keyword there), but it still works as a
        // property name (keywords are accepted in those positions).
        if let Some(kw) = KEYWORDS.iter().find(|k| **k == s) {
            self.push(Tok::Keyword(kw));
            if had_escape {
                self.mark_escaped();
            }
            return Ok(());
        }
        self.push(Tok::Ident(s));
        if had_escape {
            self.mark_escaped();
        }
        Ok(())
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
        self.pending_lone_surrogate = false;
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
        if self.pending_lone_surrogate {
            self.mark_lone_surrogate();
            self.pending_lone_surrogate = false;
        }
        Ok(())
    }

    fn read_template(&mut self) -> Result<(), LexError> {
        self.bump(); // opening backtick
        let mut parts: Vec<TplPart> = Vec::new();
        let mut cooked = String::new();
        let mut raw_start = self.pos; // raw source of the current chunk starts here
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated template literal")),
                Some('`') => {
                    let raw: String = self.chars[raw_start..self.pos].iter().collect();
                    self.bump();
                    parts.push(TplPart::Str {
                        cooked: std::mem::take(&mut cooked),
                        raw,
                    });
                    break;
                }
                Some('$') if self.chars.get(self.pos + 1) == Some(&'{') => {
                    let raw: String = self.chars[raw_start..self.pos].iter().collect();
                    parts.push(TplPart::Str {
                        cooked: std::mem::take(&mut cooked),
                        raw,
                    });
                    self.bump(); // '$'
                    self.bump(); // '{'
                    parts.push(TplPart::Sub(self.read_template_sub()?));
                    raw_start = self.pos;
                }
                Some('\\') => {
                    // Octal / `\8` / `\9` escapes are never allowed in template literals.
                    self.pending_legacy = false;
                    self.bump(); // consume the backslash
                    self.read_escape(&mut cooked)?;
                    if self.pending_legacy {
                        return Err(
                            self.err("octal escape sequences are not allowed in template literals")
                        );
                    }
                }
                Some(c) => {
                    self.bump();
                    cooked.push(c);
                }
            }
        }
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
        if let Some(c) = char::from_u32(n) {
            out.push(c);
            return Ok(());
        }
        // `n` is a surrogate code point. A high surrogate followed by a `\u` low surrogate combines
        // into a single astral code point; anything else is a lone surrogate (representable as a JS
        // string but flagged so it can be rejected as a ModuleExportName).
        if (0xD800..=0xDBFF).contains(&n) && self.peek() == Some('\\') && self.peek2() == Some('u')
        {
            let save = self.pos;
            self.bump(); // '\'
            self.bump(); // 'u'
            let mut lo = String::new();
            let mut ok = true;
            if self.peek() == Some('{') {
                self.bump();
                while let Some(c) = self.peek() {
                    if c == '}' {
                        break;
                    }
                    lo.push(c);
                    self.bump();
                }
                if self.bump() != Some('}') {
                    ok = false;
                }
            } else {
                for _ in 0..4 {
                    match self.peek() {
                        Some(c) if c.is_ascii_hexdigit() => {
                            lo.push(c);
                            self.bump();
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            let low = if ok {
                u32::from_str_radix(&lo, 16).ok()
            } else {
                None
            };
            if let Some(low) = low.filter(|l| (0xDC00..=0xDFFF).contains(l)) {
                let cp = 0x10000 + ((n - 0xD800) << 10) + (low - 0xDC00);
                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                return Ok(());
            }
            // Not a valid low surrogate: rewind and treat the high surrogate as lone.
            self.pos = save;
        }
        self.pending_lone_surrogate = true;
        out.push('\u{FFFD}');
        Ok(())
    }

    /// A numeric separator `_` is only legal immediately between two digits of the given radix.
    fn validate_seps(&self, lo: usize, hi: usize, radix: u32) -> Result<(), LexError> {
        let s = &self.chars[lo..hi];
        for (i, &c) in s.iter().enumerate() {
            if c == '_' {
                let prev = i.checked_sub(1).and_then(|j| s.get(j));
                let next = s.get(i + 1);
                let ok = prev.is_some_and(|p| p.is_digit(radix))
                    && next.is_some_and(|n| n.is_digit(radix));
                if !ok {
                    return Err(self.err("invalid use of numeric separator"));
                }
            }
        }
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
            self.validate_seps(digits_start, self.pos, radix)?;
            let digits: String = self.chars[digits_start..self.pos]
                .iter()
                .filter(|c| **c != '_')
                .collect();
            if self.peek() == Some('n') {
                self.bump();
                // BigInt is stored as i128; a literal larger than that wraps (mod 2^128) rather than
                // erroring, so a file merely *using* such a value (e.g. as a distinct collection key)
                // still parses. Arbitrary-precision arithmetic is a separate, unimplemented feature.
                let n = parse_bigint_wrapping(&digits, radix);
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
            // A leading-zero integer (legacy octal / non-octal decimal) admits no separators.
            if self.chars[start..self.pos].contains(&'_') {
                return Err(self.err("numeric separator not allowed in legacy literal"));
            }
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
            self.validate_seps(start, self.pos, 10)?;
            let text: String = self.chars[start..self.pos]
                .iter()
                .filter(|c| **c != '_')
                .collect();
            self.bump(); // n
            let n = parse_bigint_wrapping(&text, 10);
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
            while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '_') {
                self.bump();
            }
        }
        self.validate_seps(start, self.pos, 10)?;
        let text: String = self.chars[start..self.pos]
            .iter()
            .filter(|c| **c != '_')
            .collect();
        let n: f64 = text
            .parse()
            .map_err(|_| self.err("invalid numeric literal"))?;
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
                    // A backslash sequence can't contain a line terminator either.
                    match self.bump() {
                        Some(c) if is_line_terminator(c) => {
                            return Err(self.err("unterminated regular expression"))
                        }
                        Some(c) => body.push(c),
                        None => return Err(self.err("unterminated regular expression")),
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
        let rest: String = self.chars[self.pos..(self.pos + 4).min(self.chars.len())]
            .iter()
            .collect();
        // `?.` followed by a digit is `?` then `.` (a conditional like `x ? .5 : .3`), not optional
        // chaining.
        if rest.starts_with("?.")
            && self
                .chars
                .get(self.pos + 2)
                .is_some_and(|c| c.is_ascii_digit())
        {
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
        Err(self.err(format!(
            "unexpected character {:?}",
            self.peek().unwrap_or('\0')
        )))
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
/// Whether code point `c` is in the Unicode property `name` (an ASCII-free path; the bundled UCD
/// tables give the exact ID_Start/ID_Continue sets).
fn prop_has(name: &str, c: char) -> bool {
    let u = c as u32;
    crate::unicode_props::lookup(name, None)
        .map(|r| {
            r.binary_search_by(|&(lo, hi)| {
                if u < lo {
                    std::cmp::Ordering::Greater
                } else if u > hi {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
        })
        .unwrap_or(false)
}
fn is_ident_start(c: char) -> bool {
    // IdentifierStart = ID_Start ∪ {$, _} (plus `\u` escapes, handled by the caller).
    if c.is_ascii() {
        return c == '_' || c == '$' || c.is_ascii_alphabetic();
    }
    prop_has("ID_Start", c)
}
fn is_ident_part(c: char) -> bool {
    // IdentifierPart = ID_Continue ∪ {$, _, ZWNJ, ZWJ}.
    if c.is_ascii() {
        return c == '_' || c == '$' || c.is_ascii_alphanumeric();
    }
    c == '\u{200C}' || c == '\u{200D}' || prop_has("ID_Continue", c)
}

/// Parse BigInt `digits` (already separator-free) in `radix` into the engine's i128 representation,
/// wrapping on overflow (mod 2^128). The same input always yields the same value, so a literal and
/// `BigInt("<same digits>")` compare equal even when both exceed i128.
fn parse_bigint_wrapping(digits: &str, radix: u32) -> i128 {
    let mut acc: i128 = 0;
    for c in digits.chars() {
        if let Some(d) = c.to_digit(radix) {
            acc = acc.wrapping_mul(radix as i128).wrapping_add(d as i128);
        }
    }
    acc
}
