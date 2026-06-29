//! Token kinds produced by the [`crate::lexer`].

/// One lexical token plus the source bookkeeping the parser needs: the 1-based line (for error
/// messages) and whether a line terminator appeared before this token (for Automatic Semicolon
/// Insertion and the handful of "[no LineTerminator here]" grammar rules).
#[derive(Debug, Clone)]
pub struct Token {
    pub kind: Tok,
    pub line: u32,
    pub nl_before: bool,
    /// A legacy-octal number (`010`) or a string with a legacy octal/`\8`/`\9` escape — a
    /// SyntaxError in strict mode.
    pub legacy_octal: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Num(f64),
    /// A BigInt literal (`123n`), stored as `i128`.
    BigInt(i128),
    Str(String),
    /// A template literal, split into cooked string chunks and raw `${...}` substitution sources.
    /// `` `a${x}b` `` lexes to `[Str("a"), Sub("x"), Str("b")]`. The parser desugars it to a string
    /// concatenation, sub-parsing each `Sub` source.
    Template(Vec<TplPart>),
    Ident(String),
    /// A reserved word. The text is interned to a `&'static str` so the parser can match by value.
    Keyword(&'static str),
    /// A punctuator. Interned to `&'static str` (e.g. `"=>"`, `"==="`, `"+="`).
    Punct(&'static str),
    /// A regular-expression literal: `/body/flags`.
    Regex {
        body: String,
        flags: String,
    },
    Eof,
}

/// One piece of a template literal: a literal chunk (with both the cooked value and the raw source,
/// the latter needed for tagged templates' `strings.raw`) or the raw source of a `${...}` hole.
#[derive(Debug, Clone, PartialEq)]
pub enum TplPart {
    Str { cooked: String, raw: String },
    Sub(String),
}

/// The *always-reserved* words. The lexer hands these back as `Keyword` tokens so `var`/`function`/
/// etc. can never be plain identifiers and reserved-word misuse surfaces as a SyntaxError.
///
/// Contextual keywords (`let`, `const` is reserved but `of`/`async`/`get`/`set`/`static`/`yield`/
/// `await`/`as`/`from`) are deliberately NOT here — they are valid identifiers in many positions,
/// so they stay `Ident` and the parser recognises them by text where the grammar calls for them.
pub const KEYWORDS: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
];

/// Multi-char punctuators, longest first so the lexer is maximal-munch.
pub const PUNCTUATORS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=",
    "<=", ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
    "**", "<<", ">>", "{", "}", "(", ")", "[", "]", ".", ";", ",", "<", ">", "+", "-", "*", "/",
    "%", "&", "|", "^", "!", "~", "?", ":", "=", "@",
];
