//! Token kinds produced by the [`crate::lexer`].

/// One lexical token plus the source bookkeeping the parser needs: the 1-based line (for error
/// messages) and whether a line terminator appeared before this token (for Automatic Semicolon
/// Insertion and the handful of "[no LineTerminator here]" grammar rules).
#[derive(Debug, Clone)]
pub struct Token<'a> {
    pub kind: Tok<'a>,
    pub line: u32,
    /// UTF-8 byte offsets of this token in the source (for function `toString` source slices).
    pub start: u32,
    pub end: u32,
    pub nl_before: bool,
    /// A legacy-octal number (`010`) or a string with a legacy octal/`\8`/`\9` escape — a
    /// SyntaxError in strict mode.
    pub legacy_octal: bool,
    /// The identifier contained a `\u` escape — so it can't be recognized as a contextual keyword
    /// (`async`/`get`/`set`/`of`/`static`/…).
    pub escaped: bool,
    /// A string literal that contains a lone (unpaired) surrogate code point — well-formed enough to
    /// be a JS string, but not a valid ModuleExportName.
    pub lone_surrogate: bool,
}

#[derive(Debug, Clone)]
pub enum Tok<'a> {
    Num(f64),
    /// A BigInt literal (`123n`).
    BigInt(crate::bigint::JsBigInt),
    Str(crate::lstr::LStr),
    /// A template literal, split into cooked string chunks and raw `${...}` substitution sources.
    /// `` `a${x}b` `` lexes to `[Str("a"), Sub("x"), Str("b")]`. The parser desugars it to a string
    /// concatenation, sub-parsing each `Sub` source.
    Template(Box<[TplPart]>),
    /// Unescaped ASCII spellings borrow source text until an AST name needs ownership.
    Ident(&'a str),
    /// Decoded identifiers and tokens re-lexed from temporary source own their text.
    OwnedIdent(Box<str>),
    /// A reserved word. The text is interned to a `&'static str` so the parser can match by value.
    Keyword(&'static str),
    /// A punctuator. Interned to `&'static str` (e.g. `"=>"`, `"==="`, `"+="`).
    Punct(&'static str),
    /// A regular-expression literal: `/body/flags`.
    // Keep this uncommon two-string payload out of line, so it does not determine
    // the size of every token in a source file.
    Regex(Box<RegexToken>),
    Eof,
}

impl<'a> Tok<'a> {
    pub(crate) fn ident(&self) -> Option<&str> {
        match self {
            Self::Ident(text) => Some(text),
            Self::OwnedIdent(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn into_ident(self) -> Option<std::borrow::Cow<'a, str>> {
        match self {
            Self::Ident(text) => Some(std::borrow::Cow::Borrowed(text)),
            Self::OwnedIdent(text) => Some(std::borrow::Cow::Owned(text.into_string())),
            _ => None,
        }
    }
}

impl PartialEq for Tok<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Num(a), Self::Num(b)) => a == b,
            (Self::BigInt(a), Self::BigInt(b)) => a == b,
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::Template(a), Self::Template(b)) => a == b,
            (Self::Keyword(a), Self::Keyword(b)) | (Self::Punct(a), Self::Punct(b)) => a == b,
            (Self::Regex(a), Self::Regex(b)) => a == b,
            (Self::Eof, Self::Eof) => true,
            _ => self.ident().is_some() && self.ident() == other.ident(),
        }
    }
}

impl Token<'_> {
    /// Re-lexed division text is temporary, so its tokens must own their identifier spellings
    /// before being spliced into the enclosing source stream.
    pub(crate) fn into_owned(self) -> Token<'static> {
        let kind = match self.kind {
            Tok::Num(v) => Tok::Num(v),
            Tok::BigInt(v) => Tok::BigInt(v),
            Tok::Str(v) => Tok::Str(v),
            Tok::Template(v) => Tok::Template(v),
            Tok::Ident(v) => Tok::OwnedIdent(v.into()),
            Tok::OwnedIdent(v) => Tok::OwnedIdent(v),
            Tok::Keyword(v) => Tok::Keyword(v),
            Tok::Punct(v) => Tok::Punct(v),
            Tok::Regex(v) => Tok::Regex(v),
            Tok::Eof => Tok::Eof,
        };
        Token {
            kind,
            line: self.line,
            start: self.start,
            end: self.end,
            nl_before: self.nl_before,
            legacy_octal: self.legacy_octal,
            escaped: self.escaped,
            lone_surrogate: self.lone_surrogate,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegexToken {
    pub body: String,
    pub flags: String,
}

/// One piece of a template literal: a literal chunk (with both the cooked value and the raw source,
/// the latter needed for tagged templates' `strings.raw`) or the raw source of a `${...}` hole.
#[derive(Debug, Clone, PartialEq)]
pub enum TplPart {
    /// A literal chunk. `cooked` is None when the chunk contains an invalid escape sequence —
    /// legal only in a *tagged* template (the cooked value is undefined there).
    Str {
        cooked: Option<String>,
        raw: String,
    },
    Sub(String),
}

/// The *always-reserved* words. The lexer hands these back as `Keyword` tokens so `var`/`function`/
/// etc. can never be plain identifiers and reserved-word misuse surfaces as a SyntaxError.
///
/// Contextual keywords (`let`, `const` is reserved but `of`/`async`/`get`/`set`/`static`/`yield`/
/// `await`/`as`/`from`) are deliberately NOT here — they are valid identifiers in many positions,
/// so they stay `Ident` and the parser recognises them by text where the grammar calls for them.
// Keep the table sorted for snapshot operator lookup. Byte patterns let the lexer
// dispatch by length and shared prefixes without repeated short-string library calls.
macro_rules! keywords {
    ($($word:literal => $bytes:literal),* $(,)?) => {
        pub const KEYWORDS: &[&str] = &[$($word),*];
        pub(crate) fn keyword_ref(text: &str) -> Option<crate::ast::Operator> {
            match text { $($word => Some(&$word),)* _ => None }
        }
        pub(crate) fn keyword(text: &str) -> Option<&'static str> {
            match text.as_bytes() { $($bytes => Some($word),)* _ => None }
        }
    };
}
keywords!(
    "break" => b"break",
    "case" => b"case",
    "catch" => b"catch",
    "class" => b"class",
    "const" => b"const",
    "continue" => b"continue",
    "debugger" => b"debugger",
    "default" => b"default",
    "delete" => b"delete",
    "do" => b"do",
    "else" => b"else",
    "enum" => b"enum",
    "export" => b"export",
    "extends" => b"extends",
    "false" => b"false",
    "finally" => b"finally",
    "for" => b"for",
    "function" => b"function",
    "if" => b"if",
    "import" => b"import",
    "in" => b"in",
    "instanceof" => b"instanceof",
    "new" => b"new",
    "null" => b"null",
    "return" => b"return",
    "super" => b"super",
    "switch" => b"switch",
    "this" => b"this",
    "throw" => b"throw",
    "true" => b"true",
    "try" => b"try",
    "typeof" => b"typeof",
    "var" => b"var",
    "void" => b"void",
    "while" => b"while",
    "with" => b"with",
);

/// Multi-char punctuators, longest first so the lexer is maximal-munch.
macro_rules! punctuator_spellings {
    ($($text:literal),* $(,)?) => {
        pub const PUNCTUATORS: &[&str] = &[$($text),*];
        pub(crate) fn punctuation_ref(text: &str) -> Option<crate::ast::Operator> {
            match text { $($text => Some(&$text),)* _ => None }
        }
    };
}
punctuator_spellings!(
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=",
    "<=", ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
    "**", "<<", ">>", "{", "}", "(", ")", "[", "]", ".", ";", ",", "<", ">", "+", "-", "*", "/",
    "%", "&", "|", "^", "!", "~", "?", ":", "=", "@",
);

// Index the longest-first table by its first ASCII character at compile time. Most
// punctuation tokens have just one candidate; shifts have at most six.
const PUNCTUATOR_PREFIXES: [u64; 128] = {
    assert!(PUNCTUATORS.len() <= 64);
    let mut prefixes = [0; 128];
    let mut n = 0;
    while n < PUNCTUATORS.len() {
        prefixes[PUNCTUATORS[n].as_bytes()[0] as usize] |= 1 << n;
        n += 1;
    }
    prefixes
};

pub(crate) fn punctuator(chars: &[u8]) -> Option<&'static str> {
    let mut candidates = *PUNCTUATOR_PREFIXES.get(*chars.first()? as usize)?;
    while candidates != 0 {
        let p = PUNCTUATORS[candidates.trailing_zeros() as usize];
        candidates &= candidates - 1;
        if p.bytes()
            .enumerate()
            .skip(1)
            .all(|(n, b)| chars.get(n).copied() == Some(b))
        {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn byte_keyword_lookup_matches_exact_reserved_spellings() {
        let check = |text: &str| {
            let expected = super::KEYWORDS.iter().copied().find(|word| *word == text);
            assert_eq!(super::keyword(text), expected, "{text:?}");
        };
        check("");
        for word in super::KEYWORDS {
            check(word);
            for end in 0..word.len() {
                check(&word[..end]);
            }
            for index in 0..word.len() {
                for byte in 0..=127 {
                    let mut altered = word.as_bytes().to_vec();
                    altered[index] = byte;
                    check(std::str::from_utf8(&altered).unwrap());
                }
            }
            for extra in ["x", "0", "_", "$", "é", "🌙", "\\u0061"] {
                check(&format!("{word}{extra}"));
                check(&format!("{extra}{word}"));
            }
        }
        for contextual in [
            "let", "of", "async", "await", "yield", "get", "set", "static",
        ] {
            check(contextual);
        }
    }

    #[test]
    fn keyword_table_is_sorted_for_binary_search() {
        assert!(super::KEYWORDS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn punctuation_dispatch_preserves_longest_match() {
        for first in super::PUNCTUATORS {
            for suffix in super::PUNCTUATORS.iter().copied().chain(["", "é", "🌙"]) {
                let source = format!("{first}{suffix}");
                let expected = super::PUNCTUATORS
                    .iter()
                    .copied()
                    .find(|p| source.starts_with(p));
                assert_eq!(super::punctuator(source.as_bytes()), expected);
            }
        }
        assert_eq!(super::punctuator(&[]), None);
        assert_eq!(super::punctuator("é".as_bytes()), None);
        assert_eq!(super::punctuator(b"a"), None);
    }
}
