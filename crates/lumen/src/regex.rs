//! A from-scratch regular-expression engine (no dependencies).
//!
//! Pipeline: [`parse`] turns a pattern string into a [`Node`] AST, `compile` lowers it to a flat
//! [`Inst`] program, and [`Regex::exec_at`] runs a recursive backtracking matcher over it. Supports
//! the commonly-used syntax: literals, `.`, character classes (`[...]`, `\d\w\s` and negations),
//! anchors (`^ $ \b \B`), quantifiers (`* + ? {n} {n,} {n,m}`, greedy + lazy), groups (capturing,
//! `(?:)`), alternation, backreferences, and lookahead (`(?= )` / `(?! )`), with the `g i m s y`
//! flags. Backtracking is bounded by a step budget so pathological patterns fail instead of hanging.

use std::rc::Rc;

const MAX_REPEAT: usize = 1000;
const STEP_LIMIT: u64 = 2_000_000;

/// A compiled regular expression.
pub struct Regex {
    prog: Vec<Inst>,
    nmarks: usize,
    pub unicode: bool,
    pub ngroups: usize,
    pub source: String,
    pub flags: String,
    pub global: bool,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dotall: bool,
    pub sticky: bool,
    /// `(?<name>…)` group names paired with their capture index.
    pub names: Vec<(String, usize)>,
}

#[derive(Clone)]
enum Inst {
    Char(u32),
    Any,
    Class(Rc<CharClass>),
    Save(usize),
    Split(usize, usize),
    Jmp(usize),
    Match,
    AssertStart,
    AssertEnd,
    WordBoundary(bool),
    Backref(usize),
    /// `\k<name>` where the name is shared by several groups: matches via whichever captured.
    BackrefAlt(Rc<Vec<usize>>),
    /// Reset capture slots for groups `lo..=hi` at the start of a quantifier iteration.
    ClearCaps(usize, usize),
    Look {
        negate: bool,
        prog: Rc<Vec<Inst>>,
    },
    /// `(?<=…)` / `(?<!…)`: the body must match text ending at the current position.
    LookBehind {
        negate: bool,
        prog: Rc<Vec<Inst>>,
    },
    /// A repeated single-character matcher (`a*`, `\w+`, `.{2,5}`, `\p{L}+`). Consumed iteratively so
    /// a long run doesn't recurse once per character (which overflows the backtracking depth limit).
    Many {
        rep: Rep,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
    /// `(?ims-ims:…)` inline modifiers: push a new `(icase, multiline, dotall)` flag set for the
    /// group body (`Some` = add/remove, `None` = inherit), then `PopFlags` restores it.
    PushFlags(Option<bool>, Option<bool>, Option<bool>),
    PopFlags,
    /// RepeatMatcher's empty-iteration rule: `SetMark` records the position entering an optional
    /// quantifier iteration; `CheckProgress` FAILS (forcing backtracking into the body or out of
    /// the loop) when the iteration consumed nothing.
    SetMark(usize),
    CheckProgress(usize),
}

/// A single-codepoint matcher, for the `Inst::Many` fast path.
#[derive(Clone)]
enum Rep {
    Char(u32),
    Any,
    Class(Rc<CharClass>),
}

#[derive(Default, Clone)]
struct CharClass {
    negate: bool,
    ranges: Vec<(u32, u32)>,
    /// Builtin sub-classes by letter: 'd','w','s' (and uppercase negated forms expanded inline).
    builtins: Vec<char>,
    /// Unicode property escapes `\p{…}` / `\P{…}`: `(negated, sorted codepoint ranges)`.
    props: Vec<(bool, &'static [(u32, u32)])>,
}

impl CharClass {
    fn matches(&self, u: u32, icase: bool, unicode: bool) -> bool {
        let mut hit = self.matches_raw2(u, icase, unicode);
        let c = char::from_u32(u);
        if !hit && icase {
            if let Some(c) = c {
                if unicode {
                    // Try every member of the character's case-fold orbit.
                    for alt in fold_orbit(u) {
                        if alt != u && self.matches_raw2(alt, icase, unicode) {
                            hit = true;
                            break;
                        }
                    }
                } else {
                    // Legacy Canonicalize: compare via simple uppercase, never folding a
                    // non-ASCII character onto an ASCII one.
                    let cu = canonicalize_legacy(c);
                    if cu != c && self.matches_raw2(cu as u32, icase, unicode) {
                        hit = true;
                    }
                    // A member whose canonical form equals cu also matches (/[k]/i vs 'K').
                    if !hit {
                        for alt in c.to_lowercase().chain(c.to_uppercase()) {
                            if alt != c
                                && canonicalize_legacy(alt) == cu
                                && self.matches_raw2(alt as u32, icase, unicode)
                            {
                                hit = true;
                                break;
                            }
                        }
                    }
                }
            }
        }
        hit ^ self.negate
    }
    fn matches_raw2(&self, u: u32, icase: bool, unicode: bool) -> bool {
        // Class membership is decided in true code-point space: smuggled surrogate atoms in the
        // class's own ranges decode to their surrogate values.
        for &(lo, hi) in &self.ranges {
            if u >= lo && u <= hi {
                return true;
            }
        }
        for &b in &self.builtins {
            if builtin_matches_ic(b, u, icase, unicode) {
                return true;
            }
        }
        for &(neg, ranges) in &self.props {
            // Ranges are sorted and disjoint: binary-search for the one that could contain `u`.
            let in_range = ranges
                .binary_search_by(|&(lo, hi)| {
                    if u < lo {
                        std::cmp::Ordering::Greater
                    } else if u > hi {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .is_ok();
            if in_range ^ neg {
                return true;
            }
        }
        false
    }
}

fn builtin_matches_ic(b: char, u: u32, icase: bool, unicode: bool) -> bool {
    let c = char::from_u32(u);
    match b {
        'd' => c.map(|c| c.is_ascii_digit()).unwrap_or(false),
        'D' => !c.map(|c| c.is_ascii_digit()).unwrap_or(false),
        'w' => is_word_ic(u, icase, unicode),
        'W' => !is_word_ic(u, icase, unicode),
        's' => c.map(js_whitespace).unwrap_or(false),
        'S' => !c.map(js_whitespace).unwrap_or(false),
        _ => false,
    }
}

/// A JS LineTerminator code point.
fn is_line_terminator_u32(c: u32) -> bool {
    matches!(c, 0x0A | 0x0D | 0x2028 | 0x2029)
}

fn is_word(c: u32) -> bool {
    char::from_u32(c)
        .map(|c| c.is_ascii_alphanumeric() || c == '_')
        .unwrap_or(false)
}

/// GetWordCharacters: under unicode case-insensitive matching, characters whose case fold lands
/// in [A-Za-z0-9_] (ſ, K) are word characters too.
fn is_word_ic(c: u32, icase: bool, unicode: bool) -> bool {
    if is_word(c) {
        return true;
    }
    if !(icase && unicode) {
        return false;
    }
    fold_orbit(c).any(is_word)
}

/// The canonical full case-folding representative of a code point (identity outside any orbit).
fn fold_canon(u: u32) -> u32 {
    match crate::regex_fold::FOLD_CANON.binary_search_by_key(&u, |&(m, _)| m) {
        Ok(k) => crate::regex_fold::FOLD_CANON[k].1,
        Err(_) => u,
    }
}

/// Every member of `u`'s case-fold orbit (just `u` when it has none).
fn fold_orbit(u: u32) -> impl Iterator<Item = u32> {
    let canon = fold_canon(u);
    let t = crate::regex_fold::FOLD_ORBITS;
    let lo = t.partition_point(|&(c, _)| c < canon);
    let hi = t.partition_point(|&(c, _)| c <= canon);
    let mut own = if lo == hi { Some(u) } else { None };
    t[lo..hi]
        .iter()
        .map(|&(_, m)| m)
        .chain(std::iter::from_fn(move || own.take()))
}

/// The JS WhiteSpace + LineTerminator set: includes U+FEFF and NBSP, but NOT U+0085 (NEL) or
/// other control characters Rust's `is_whitespace` accepts.
fn js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn uprop_has(name: &str, c: char) -> bool {
    let u = c as u32;
    crate::unicode_props::lookup(name, None).is_some_and(|r| {
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
}
/// IdentifierStart for a RegExp capture-group name (ID_Start ∪ {$, _}).
/// The legacy (non-Unicode) Canonicalize: the simple uppercase mapping, except that a non-ASCII
/// character never canonicalizes onto an ASCII one (so /\u212a/i does not match 'K' without /u).
fn canonicalize_legacy(c: char) -> char {
    let mut up = c.to_uppercase();
    let (first, rest) = (up.next(), up.next());
    match (first, rest) {
        (Some(u), None) => {
            if (c as u32) >= 128 && (u as u32) < 128 {
                c
            } else {
                u
            }
        }
        _ => c,
    }
}

/// A regular-expression SyntaxCharacter (the only chars an identity escape may name in /u mode).
fn is_regex_syntax_char(c: char) -> bool {
    matches!(
        c,
        '^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
    )
}

fn regex_ident_start(c: char) -> bool {
    if c.is_ascii() {
        return c == '$' || c == '_' || c.is_ascii_alphabetic();
    }
    uprop_has("ID_Start", c)
}
/// IdentifierPart for a capture-group name (ID_Continue ∪ {$, _, ZWNJ, ZWJ}).
fn regex_ident_part(c: char) -> bool {
    if c.is_ascii() {
        return c == '$' || c == '_' || c.is_ascii_alphanumeric();
    }
    c == '\u{200C}' || c == '\u{200D}' || uprop_has("ID_Continue", c)
}

// ---------------------------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
enum Node {
    Empty,
    Char(u32),
    Any,
    Class(CharClass),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Group(Option<usize>, Box<Node>),
    Repeat(Box<Node>, usize, Option<usize>, bool),
    Start,
    End,
    WordB(bool),
    Backref(usize),
    /// `\k<name>` — resolved to a group index after the whole pattern is parsed.
    NamedBackref(String),
    /// `\k<name>` naming several duplicate groups — matches via whichever of them captured.
    BackrefAlt(Vec<usize>),
    Look(bool, Box<Node>),
    /// `(?<=…)` / `(?<!…)` lookbehind: assert the body matches text *ending* at the current position.
    LookBehind(bool, Box<Node>),
    /// `(?ims-ims:…)` inline-modifier group: `(add, remove)` flag deltas over `(i, m, s)`.
    Modifier {
        add: (bool, bool, bool),
        remove: (bool, bool, bool),
        inner: Box<Node>,
    },
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    ngroups: usize,
    names: Vec<(String, usize)>,
    /// `u` or `v` flag: enables Unicode mode (notably `\p{…}` property escapes).
    unicode: bool,
    /// Whether `\k` is a named back-reference here: true in Unicode mode, or when the pattern
    /// contains a named group (`(?<name>…)`). Otherwise `\k` is the literal character `k` (Annex B).
    /// The `v` flag: classes are ClassSetExpressions (nested classes, `&&`, `--`, `\q{}`).
    unicode_sets: bool,
    named_mode: bool,
    /// `\k<name>` references collected during parsing, validated against `names` afterwards.
    name_refs: Vec<String>,
}

/// The element sequence regular expressions operate over. In unicode (`u`/`v`) mode an element
/// is a code point; otherwise it is a UTF-16 code unit. Surrogate units/code points are carried
/// as their jstr-smuggled plane-16 scalars so every element is a valid `char` — an astral
/// character in a non-unicode pattern or subject is therefore TWO elements (its two halves).
pub fn pattern_elements(unicode: bool, s: &str) -> Vec<char> {
    if unicode {
        crate::jstr::code_points(s)
            .into_iter()
            .map(elem_of_cp)
            .collect()
    } else {
        crate::jstr::units(s)
            .into_iter()
            .map(|u| {
                if (0xD800..0xE000).contains(&(u as u32)) {
                    crate::jstr::smuggle(u)
                } else {
                    char::from_u32(u as u32).unwrap()
                }
            })
            .collect()
    }
}

/// The true code-point value of a pattern/subject element (smuggled surrogates decode).
fn cp_of_elem(c: char) -> u32 {
    match crate::jstr::smuggled(c) {
        Some(u) => u as u32,
        None => c as u32,
    }
}

fn elem_of_cp(cp: u32) -> char {
    if (0xD800..0xE000).contains(&cp) {
        crate::jstr::smuggle(cp as u16)
    } else {
        char::from_u32(cp).unwrap()
    }
}

/// A subject string prepared for matching: its elements plus each element's unit offset
/// (`unit_of.len() == elems.len() + 1`; the last entry is the total unit length). JS-visible
/// indices (lastIndex, match.index) are always unit offsets.
pub struct ReText {
    pub elems: Vec<u32>,
    pub unit_of: Vec<usize>,
    unicode: bool,
}

impl ReText {
    pub fn new(unicode: bool, s: &str) -> ReText {
        if unicode {
            let cps = crate::jstr::code_points(s);
            let mut unit_of = Vec::with_capacity(cps.len() + 1);
            let mut u = 0usize;
            for &cp in &cps {
                unit_of.push(u);
                u += if cp >= 0x10000 { 2 } else { 1 };
            }
            unit_of.push(u);
            ReText {
                elems: cps,
                unit_of,
                unicode,
            }
        } else {
            let units = crate::jstr::units(s);
            let elems: Vec<u32> = units.iter().map(|&u| u as u32).collect();
            let unit_of = (0..=elems.len()).collect();
            ReText {
                elems,
                unit_of,
                unicode,
            }
        }
    }

    /// The element index containing unit offset `u` (== len when `u` is at/past the end).
    pub fn elem_at_unit(&self, u: usize) -> usize {
        match self.unit_of.binary_search(&u) {
            Ok(k) => k.min(self.elems.len()),
            Err(k) => k - 1,
        }
    }

    /// The unit offset of element `e`.
    pub fn unit_index(&self, e: usize) -> usize {
        self.unit_of[e.min(self.elems.len())]
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.elems.len()
    }

    /// The canonical string for elements `a..b` (surrogate halves recombine).
    pub fn slice(&self, a: usize, b: usize) -> String {
        if self.unicode {
            crate::jstr::from_code_points(&self.elems[a..b])
        } else {
            let units: Vec<u16> = self.elems[a..b].iter().map(|&e| e as u16).collect();
            crate::jstr::from_units(&units)
        }
    }
}

/// Whether `pattern` contains a named capture group `(?<name>…)` (not a lookbehind `(?<=`/`(?<!`).
fn has_named_group(pattern: &str) -> bool {
    let b: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == '(' && b[i + 1] == '?' && b[i + 2] == '<' {
            let after = b.get(i + 3).copied();
            if after != Some('=') && after != Some('!') {
                return true;
            }
        }
        i += 1;
    }
    false
}

impl Regex {
    pub fn new(pattern: &str, flags: &str) -> Result<Regex, String> {
        let mut seen = String::new();
        for f in flags.chars() {
            if !"dgimsuvy".contains(f) {
                return Err(format!("invalid regular expression flag {f}"));
            }
            if seen.contains(f) {
                return Err(format!("duplicate regular expression flag {f}"));
            }
            seen.push(f);
        }
        if flags.contains('u') && flags.contains('v') {
            return Err("the u and v regular expression flags are mutually exclusive".into());
        }
        let unicode = flags.contains('u') || flags.contains('v');
        let unicode_sets = flags.contains('v');
        let named_mode = unicode || has_named_group(pattern);
        let mut p = Parser {
            chars: pattern_elements(unicode, pattern),
            pos: 0,
            ngroups: 0,
            names: Vec::new(),
            unicode,
            unicode_sets,
            named_mode,
            name_refs: Vec::new(),
        };
        let mut ast = p.parse_alt()?;
        if p.pos != p.chars.len() {
            return Err("unexpected character in pattern".into());
        }
        // Resolve `\k<name>` references now that every group name is known.
        for name in &p.name_refs {
            if !p.names.iter().any(|(n, _)| n == name) {
                return Err(format!("invalid named back reference <{name}>"));
            }
        }
        // Duplicate group names are allowed only across distinct alternation branches.
        validate_group_names(&ast, &p.names)?;
        // In Unicode mode a decimal escape must name an existing capture group.
        if unicode {
            let mut max_ref = 0usize;
            max_backref(&ast, &mut max_ref);
            if max_ref > p.ngroups {
                return Err(format!(
                    "back reference \\{max_ref} exceeds the number of capture groups"
                ));
            }
        }
        resolve_named_backrefs(&mut ast, &p.names);
        // Wrap the whole match in group-0 saves.
        let mut prog = vec![Inst::Save(0)];
        let mut nmarks = 0usize;
        compile(&ast, &mut prog, &mut nmarks)?;
        prog.push(Inst::Save(1));
        prog.push(Inst::Match);
        // The `flags` accessor returns flags in canonical order.
        let canonical: String = "dgimsuvy".chars().filter(|c| flags.contains(*c)).collect();
        Ok(Regex {
            unicode,
            nmarks,
            prog,
            ngroups: p.ngroups,
            source: if pattern.is_empty() {
                "(?:)".into()
            } else {
                pattern.to_string()
            },
            flags: canonical,
            global: flags.contains('g'),
            ignore_case: flags.contains('i'),
            multiline: flags.contains('m'),
            dotall: flags.contains('s'),
            sticky: flags.contains('y'),
            names: p.names,
        })
    }

    /// Try to match, scanning forward from `start` (unless sticky/`y`, which requires a match at
    /// exactly `start`). Returns capture spans: index 0 is the whole match, then one per group.
    pub fn exec_at(&self, input: &[u32], start: usize) -> Option<Vec<Option<(usize, usize)>>> {
        let mut from = start;
        loop {
            if from > input.len() {
                return None;
            }
            let mut m = Matcher {
                input,
                caps: vec![None; 2 * (self.ngroups + 1)],
                marks: vec![None; self.nmarks],
                steps: 0,
                depth: 0,
                back: false,
                flags: vec![(self.ignore_case, self.multiline, self.dotall)],
                unicode: self.flags.contains('u') || self.flags.contains('v'),
            };
            if m.run(&self.prog, 0, from) {
                let mut out = Vec::with_capacity(self.ngroups + 1);
                for g in 0..=self.ngroups {
                    out.push(match (m.caps[2 * g], m.caps[2 * g + 1]) {
                        // A group inside a lookbehind captured right-to-left: normalize the span.
                        (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
                        _ => None,
                    });
                }
                return Some(out);
            }
            if self.sticky {
                return None;
            }
            from += 1;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------------------------

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn parse_alt(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.bump();
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn parse_concat(&mut self) -> Result<Node, String> {
        let mut seq = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            seq.push(self.parse_quantified()?);
        }
        match seq.len() {
            0 => Ok(Node::Empty),
            1 => Ok(seq.pop().unwrap()),
            _ => Ok(Node::Concat(seq)),
        }
    }

    fn parse_quantified(&mut self) -> Result<Node, String> {
        // A quantifier at the start of a term (after `(`, `|`, or `^`) has nothing to repeat.
        if matches!(self.peek(), Some('*' | '+' | '?')) {
            return Err("nothing to repeat".into());
        }
        // A *braced* quantifier at term start too (`/{2}/`); a non-quantifier `{` stays a
        // literal (Annex B) and is handled by parse_atom.
        if self.peek() == Some('{') && self.try_parse_brace()?.is_some() {
            return Err("nothing to repeat".into());
        }
        let atom = self.parse_atom()?;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.bump();
                (0, None)
            }
            Some('+') => {
                self.bump();
                (1, None)
            }
            Some('?') => {
                self.bump();
                (0, Some(1))
            }
            Some('{') => match self.try_parse_brace()? {
                Some(mm) => mm,
                None => return Ok(atom),
            },
            _ => return Ok(atom),
        };
        // A lookbehind can never be quantified; a lookahead only outside Unicode mode
        // (the Annex B QuantifiableAssertion carve-out).
        if matches!(atom, Node::LookBehind(..)) || (self.unicode && matches!(atom, Node::Look(..)))
        {
            return Err("quantifier on an assertion".into());
        }
        let greedy = if self.peek() == Some('?') {
            self.bump();
            false
        } else {
            true
        };
        // A quantifier cannot itself be quantified (`a**`, `a+?` is lazy and already consumed).
        if matches!(self.peek(), Some('*' | '+' | '?')) {
            return Err("nothing to repeat".into());
        }
        Ok(Node::Repeat(Box::new(atom), min, max, greedy))
    }

    /// `{n}` / `{n,}` / `{n,m}`. Returns `None` (and leaves position) if it is not a valid quantifier
    /// (a literal `{`).
    fn try_parse_brace(&mut self) -> Result<Option<(usize, Option<usize>)>, String> {
        let save = self.pos;
        self.bump(); // {
        let mut digits = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                digits.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if digits.is_empty() {
            self.pos = save;
            return Ok(None);
        }
        let min: usize = digits.parse().unwrap_or(0);
        let max = if self.peek() == Some(',') {
            self.bump();
            let mut d2 = String::new();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    d2.push(c);
                    self.bump();
                } else {
                    break;
                }
            }
            if d2.is_empty() {
                None
            } else {
                Some(d2.parse().unwrap_or(min))
            }
        } else {
            Some(min)
        };
        if self.peek() != Some('}') {
            self.pos = save;
            return Ok(None);
        }
        self.bump(); // }
        if let Some(mx) = max {
            if min > mx {
                return Err("numbers out of order in {} quantifier".into());
            }
        }
        Ok(Some((min, max)))
    }

    fn parse_atom(&mut self) -> Result<Node, String> {
        match self.bump() {
            None => Ok(Node::Empty),
            Some('.') => Ok(Node::Any),
            Some('^') => Ok(Node::Start),
            Some('$') => Ok(Node::End),
            Some('(') => self.parse_group(),
            Some('[') => self.parse_class(),
            Some('\\') => self.parse_escape(),
            // In Unicode mode a PatternCharacter excludes the remaining SyntaxCharacters.
            Some(c @ ('{' | '}' | ']')) if self.unicode => {
                Err(format!("lone '{c}' is not valid in a unicode pattern"))
            }
            Some(c) => Ok(Node::Char(cp_of_elem(c))),
        }
    }

    fn parse_group(&mut self) -> Result<Node, String> {
        // Detect (?:...), (?=...), (?!...), (?<name>...), and lookbehind (?<= / (?<! .
        if self.peek() == Some('?') {
            self.bump();
            match self.peek() {
                Some(':') => {
                    self.bump();
                    let inner = self.parse_alt()?;
                    self.expect(')')?;
                    Ok(Node::Group(None, Box::new(inner)))
                }
                Some('=') => {
                    self.bump();
                    let inner = self.parse_alt()?;
                    self.expect(')')?;
                    Ok(Node::Look(false, Box::new(inner)))
                }
                Some('!') => {
                    self.bump();
                    let inner = self.parse_alt()?;
                    self.expect(')')?;
                    Ok(Node::Look(true, Box::new(inner)))
                }
                Some('<') => {
                    self.bump();
                    // Named group (?<name>...) -> treat as a normal capturing group; lookbehind
                    // (?<= / (?<! is approximated as a non-capturing group (best effort).
                    match self.peek() {
                        Some(c @ ('=' | '!')) => {
                            self.bump();
                            let inner = self.parse_alt()?;
                            self.expect(')')?;
                            Ok(Node::LookBehind(c == '!', Box::new(inner)))
                        }
                        _ => {
                            let name = self.parse_group_name()?;
                            self.ngroups += 1;
                            let idx = self.ngroups;
                            // Duplicate names are allowed (ES2025) — they're distinct capture groups
                            // in different alternatives; the `groups` object reports whichever matched.
                            self.names.push((name, idx));
                            let inner = self.parse_alt()?;
                            self.expect(')')?;
                            Ok(Node::Group(Some(idx), Box::new(inner)))
                        }
                    }
                }
                Some('i' | 'm' | 's' | '-') => self.parse_modifier_group(),
                _ => Err("unsupported group".into()),
            }
        } else {
            self.ngroups += 1;
            let idx = self.ngroups;
            let inner = self.parse_alt()?;
            self.expect(')')?;
            Ok(Node::Group(Some(idx), Box::new(inner)))
        }
    }

    /// Parse `(?ims-ims:body)` after the `(?`. Flags before `-` are added, after `-` removed.
    fn parse_modifier_group(&mut self) -> Result<Node, String> {
        let mut add = (false, false, false);
        let mut remove = (false, false, false);
        let mut neg = false;
        let mut seen_any = false;
        loop {
            match self.peek() {
                Some('-') if !neg => {
                    self.bump();
                    neg = true;
                }
                Some(c @ ('i' | 'm' | 's')) => {
                    self.bump();
                    seen_any = true;
                    let slot = if neg { &mut remove } else { &mut add };
                    let f = match c {
                        'i' => &mut slot.0,
                        'm' => &mut slot.1,
                        _ => &mut slot.2,
                    };
                    if *f {
                        return Err("duplicate inline modifier flag".into());
                    }
                    *f = true;
                }
                Some(':') => break,
                _ => return Err("invalid inline modifier".into()),
            }
        }
        self.bump(); // ':'
        let _ = seen_any;
        // Only a wholly-empty modifier list (`(?:` is handled elsewhere; `(?-:` reaches here) is
        // invalid — `(?s-:…)` (add some, remove none) is fine.
        if add == (false, false, false) && remove == (false, false, false) {
            return Err("empty inline modifier".into());
        }
        // A flag may not be both added and removed.
        if (add.0 && remove.0) || (add.1 && remove.1) || (add.2 && remove.2) {
            return Err("inline modifier flag added and removed".into());
        }
        let inner = self.parse_alt()?;
        self.expect(')')?;
        Ok(Node::Modifier {
            add,
            remove,
            inner: Box::new(inner),
        })
    }

    /// `v`-mode `[...]`: parse a ClassSetExpression, computing the concrete set, and compile it
    /// to a match node (an alternation of its strings — longest first — plus a range class).
    fn parse_class_set(&mut self) -> Result<Node, String> {
        let negate = if self.peek() == Some('^') {
            self.bump();
            true
        } else {
            false
        };
        let mut set = self.parse_class_set_expression()?;
        self.expect(']')?;
        if negate {
            set = set.complement()?;
        }
        Ok(class_set_to_node(set))
    }

    fn parse_class_set_expression(&mut self) -> Result<ClassSet, String> {
        // Empty class.
        if self.peek() == Some(']') {
            return Ok(ClassSet::default());
        }
        let first = self.parse_class_set_operand()?;
        // Decide the expression kind from the following operator.
        if self.peek() == Some('&') && self.chars.get(self.pos + 1) == Some(&'&') {
            let mut acc = first;
            while self.peek() == Some('&') && self.chars.get(self.pos + 1) == Some(&'&') {
                self.bump();
                self.bump();
                if self.peek() == Some('&') {
                    return Err("unexpected '&&&' in class set".into());
                }
                let rhs = self.parse_class_set_operand()?;
                acc = acc.intersect(rhs);
            }
            return Ok(acc);
        }
        if self.peek() == Some('-') && self.chars.get(self.pos + 1) == Some(&'-') {
            let mut acc = first;
            while self.peek() == Some('-') && self.chars.get(self.pos + 1) == Some(&'-') {
                self.bump();
                self.bump();
                let rhs = self.parse_class_set_operand()?;
                acc = acc.subtract(rhs);
            }
            return Ok(acc);
        }
        // Union (with a-z ranges).
        let mut acc = self.maybe_class_set_range(first)?;
        while self.peek() != Some(']') && self.peek().is_some() {
            if self.peek() == Some('&') && self.chars.get(self.pos + 1) == Some(&'&') {
                return Err("cannot mix '&&' with a union in a class set".into());
            }
            if self.peek() == Some('-') && self.chars.get(self.pos + 1) == Some(&'-') {
                return Err("cannot mix '--' with a union in a class set".into());
            }
            let next = self.parse_class_set_operand()?;
            let next = self.maybe_class_set_range(next)?;
            acc = acc.union(next);
        }
        Ok(acc)
    }

    /// After a single-character operand, `-x` extends it to a range.
    fn maybe_class_set_range(&mut self, operand: ClassSet) -> Result<ClassSet, String> {
        let single = operand.strings.is_empty()
            && operand.ranges.len() == 1
            && operand.ranges[0].0 == operand.ranges[0].1;
        if single
            && self.peek() == Some('-')
            && self.chars.get(self.pos + 1) != Some(&'-')
            && self.chars.get(self.pos + 1) != Some(&']')
        {
            self.bump(); // '-'
            let hi = self.parse_class_set_operand()?;
            let hi_single =
                hi.strings.is_empty() && hi.ranges.len() == 1 && hi.ranges[0].0 == hi.ranges[0].1;
            if !hi_single {
                return Err("invalid character class range".into());
            }
            let (a, b) = (operand.ranges[0].0, hi.ranges[0].0);
            if a > b {
                return Err("range out of order in character class".into());
            }
            return Ok(ClassSet {
                ranges: vec![(a, b)],
                strings: Vec::new(),
            });
        }
        Ok(operand)
    }

    fn parse_class_set_operand(&mut self) -> Result<ClassSet, String> {
        match self.peek() {
            None => Err("unterminated character class".into()),
            Some('[') => {
                self.bump();
                let negate = if self.peek() == Some('^') {
                    self.bump();
                    true
                } else {
                    false
                };
                let mut set = self.parse_class_set_expression()?;
                self.expect(']')?;
                if negate {
                    set = set.complement()?;
                }
                Ok(set)
            }
            Some('\\') => {
                self.bump();
                match self.peek() {
                    Some('q') => {
                        self.bump();
                        if self.bump() != Some('{') {
                            return Err("expected '{' after \\q".into());
                        }
                        let mut set = ClassSet::default();
                        let mut cur: Vec<char> = Vec::new();
                        loop {
                            match self.peek() {
                                None => return Err("unterminated \\q{...}".into()),
                                Some('}') => {
                                    self.bump();
                                    push_q_alternative(&mut set, std::mem::take(&mut cur));
                                    break;
                                }
                                Some('|') => {
                                    self.bump();
                                    push_q_alternative(&mut set, std::mem::take(&mut cur));
                                }
                                Some('\\') => {
                                    self.bump();
                                    let v = self.class_set_escape_char()?;
                                    cur.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                                }
                                Some(c) => {
                                    self.bump();
                                    cur.push(c);
                                }
                            }
                        }
                        set.normalize();
                        Ok(set)
                    }
                    Some(b @ ('d' | 'D' | 'w' | 'W' | 's' | 'S')) => {
                        self.bump();
                        Ok(builtin_class_set(b))
                    }
                    Some(pc @ ('p' | 'P')) => {
                        self.bump();
                        self.parse_class_set_property(pc == 'P')
                    }
                    _ => Ok(ClassSet::from_cp(self.class_set_escape_char()?)),
                }
            }
            // ClassSetSyntaxCharacters may not appear literally.
            Some(c @ ('(' | ')' | '{' | '}' | '/' | '|' | '-')) => {
                Err(format!("'{c}' must be escaped in a v-mode class"))
            }
            Some(c) => {
                // Doubled punctuators are reserved.
                if "&!#$%*+,.:;<=>?@^`~\"'".contains(c) && self.chars.get(self.pos + 1) == Some(&c)
                {
                    return Err(format!("reserved doubled punctuator '{c}{c}' in class set"));
                }
                self.bump();
                Ok(ClassSet::from_cp(cp_of_elem(c)))
            }
        }
    }

    /// A single-character escape inside a v-mode class (`\n`, `\u{...}`, `\-`, identity escapes).
    fn class_set_escape_char(&mut self) -> Result<u32, String> {
        match self.bump() {
            None => Err("trailing backslash in class".into()),
            Some('n') => Ok('\n' as u32),
            Some('t') => Ok('\t' as u32),
            Some('r') => Ok('\r' as u32),
            Some('f') => Ok(0x0C),
            Some('v') => Ok(0x0B),
            Some('b') => Ok(0x08),
            Some('0') => Ok(0),
            Some('x') => self.hex_strict(2),
            Some('u') => self.unicode_escape_strict(),
            Some('c') => match self.peek() {
                Some(l) if l.is_ascii_alphabetic() => {
                    self.bump();
                    Ok((l as u8 % 32) as u32)
                }
                _ => Err("invalid \\c escape in class set".into()),
            },
            Some(c) if is_regex_syntax_char(c) || "/-&!#%,:;<=>@`~\"'".contains(c) => Ok(c as u32),
            Some(c) => Err(format!("invalid identity escape \\{c} in v-mode class")),
        }
    }

    fn parse_class_set_property(&mut self, negate: bool) -> Result<ClassSet, String> {
        if self.bump() != Some('{') {
            return Err("invalid property escape: expected '{'".into());
        }
        let mut body = String::new();
        loop {
            match self.bump() {
                Some('}') => break,
                Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '=' => body.push(c),
                Some(_) => return Err("invalid character in property escape".into()),
                None => return Err("unterminated property escape".into()),
            }
        }
        let (name, value) = match body.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (body.as_str(), None),
        };
        if value.is_none() {
            if let Some(set) = property_of_strings(name) {
                if negate {
                    return Err("\\P of a property of strings is invalid".into());
                }
                return Ok(set);
            }
        }
        match crate::unicode_props::lookup_strict(name, value) {
            Some((complement, ranges)) => {
                let set = ClassSet {
                    ranges: ranges.to_vec(),
                    strings: Vec::new(),
                };
                if negate != complement {
                    set.complement()
                } else {
                    Ok(set)
                }
            }
            None => Err(format!("invalid unicode property {body}")),
        }
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        if self.unicode_sets {
            return self.parse_class_set();
        }
        let mut cc = CharClass::default();
        if self.peek() == Some('^') {
            self.bump();
            cc.negate = true;
        }
        // `]` always closes — `[]` is the empty class (matches nothing), `[^]` matches anything.
        loop {
            match self.peek() {
                None => return Err("unterminated character class".into()),
                Some(']') => {
                    self.bump();
                    break;
                }
                _ => {}
            }
            let lo = self.class_atom()?;
            // Range a-z (but `-` at end or before `]` is literal).
            if self.peek() == Some('-') && self.chars.get(self.pos + 1) != Some(&']') {
                self.bump();
                let hi = self.class_atom()?;
                match (lo, hi) {
                    (ClassAtom::Char(a), ClassAtom::Char(b)) => {
                        if a > b {
                            return Err("range out of order in character class".into());
                        }
                        cc.ranges.push((a, b));
                    }
                    (a, b) => {
                        // In Unicode mode a class escape (`\d`, `\p{…}`) can't be a range bound.
                        if self.unicode {
                            return Err("invalid character class range".into());
                        }
                        push_class_atom(&mut cc, a);
                        cc.ranges.push(('-' as u32, '-' as u32));
                        push_class_atom(&mut cc, b);
                    }
                }
            } else {
                push_class_atom(&mut cc, lo);
            }
        }
        Ok(Node::Class(cc))
    }

    fn class_atom(&mut self) -> Result<ClassAtom, String> {
        match self.bump() {
            None => Err("unterminated character class".into()),
            Some('\\') => match self.bump() {
                None => Err("bad escape in class".into()),
                Some(c @ ('d' | 'D' | 'w' | 'W' | 's' | 'S')) => Ok(ClassAtom::Builtin(c)),
                Some(c @ ('p' | 'P')) if self.unicode => {
                    let prop = self.parse_prop_escape(c == 'P')?;
                    Ok(ClassAtom::Prop(prop))
                }
                Some('n') => Ok(ClassAtom::Char('\n' as u32)),
                Some('t') => Ok(ClassAtom::Char('\t' as u32)),
                Some('r') => Ok(ClassAtom::Char('\r' as u32)),
                Some('f') => Ok(ClassAtom::Char(0x0C)),
                Some('v') => Ok(ClassAtom::Char(0x0B)),
                Some('0') => {
                    if self.unicode && self.peek().is_some_and(|d| d.is_ascii_digit()) {
                        return Err("legacy octal escape in unicode pattern".into());
                    }
                    Ok(ClassAtom::Char(0))
                }
                Some('b') => Ok(ClassAtom::Char(0x08)),
                Some('c') => match self.peek() {
                    Some(l) if l.is_ascii_alphabetic() => {
                        self.bump();
                        Ok(ClassAtom::Char((l as u8 % 32) as u32))
                    }
                    _ if self.unicode => Err("invalid \\c escape in unicode pattern".into()),
                    _ => Ok(ClassAtom::Char('c' as u32)),
                },
                Some('x') => {
                    if self.unicode {
                        Ok(ClassAtom::Char(self.hex_strict(2)?))
                    } else {
                        Ok(ClassAtom::Char(self.hex(2, 'x')))
                    }
                }
                Some('u') => {
                    if self.unicode {
                        Ok(ClassAtom::Char(self.unicode_escape_strict()?))
                    } else {
                        Ok(ClassAtom::Char(self.unicode_escape()))
                    }
                }
                Some(c) if self.unicode && !is_regex_syntax_char(c) && c != '/' && c != '-' => {
                    Err(format!("invalid identity escape \\{c} in unicode class"))
                }
                Some(c) => Ok(ClassAtom::Char(cp_of_elem(c))),
            },
            Some(c) => Ok(ClassAtom::Char(cp_of_elem(c))),
        }
    }

    fn parse_escape(&mut self) -> Result<Node, String> {
        match self.bump() {
            None => Err("trailing backslash".into()),
            Some(c @ ('d' | 'D' | 'w' | 'W' | 's' | 'S')) => Ok(Node::Class(CharClass {
                builtins: vec![c],
                ..Default::default()
            })),
            Some(c @ ('p' | 'P')) if self.unicode => {
                // In v-mode a property escape may be a property of *strings* (a computed set).
                if self.unicode_sets {
                    let set = self.parse_class_set_property(c == 'P')?;
                    return Ok(class_set_to_node(set));
                }
                let prop = self.parse_prop_escape(c == 'P')?;
                Ok(Node::Class(CharClass {
                    props: vec![prop],
                    ..Default::default()
                }))
            }
            Some('b') => Ok(Node::WordB(true)),
            Some('B') => Ok(Node::WordB(false)),
            Some('k') if self.named_mode => {
                // `\k<name>` — a named back-reference (resolved after the full parse).
                if self.peek() != Some('<') {
                    return Err("expected '<' in named back reference".into());
                }
                self.bump();
                let name = self.parse_group_name()?;
                self.name_refs.push(name.clone());
                Ok(Node::NamedBackref(name))
            }
            Some('n') => Ok(Node::Char('\n' as u32)),
            Some('t') => Ok(Node::Char('\t' as u32)),
            Some('r') => Ok(Node::Char('\r' as u32)),
            Some('f') => Ok(Node::Char(0x0C)),
            Some('v') => Ok(Node::Char(0x0B)),
            Some('0') => {
                // `\0` may not be followed by a digit in Unicode mode (a legacy octal escape).
                if self.unicode && self.peek().is_some_and(|d| d.is_ascii_digit()) {
                    return Err("legacy octal escape in unicode pattern".into());
                }
                Ok(Node::Char(0))
            }
            Some('c') => {
                // `\cX` (a letter) is a control escape; anything else is only tolerated outside
                // Unicode mode.
                match self.peek() {
                    Some(l) if l.is_ascii_alphabetic() => {
                        self.bump();
                        Ok(Node::Char((l as u8 % 32) as u32))
                    }
                    _ if self.unicode => Err("invalid \\c escape in unicode pattern".into()),
                    _ => Ok(Node::Char('c' as u32)),
                }
            }
            Some('x') => {
                if self.unicode {
                    Ok(Node::Char(self.hex_strict(2)?))
                } else {
                    Ok(Node::Char(self.hex(2, 'x')))
                }
            }
            Some('u') => {
                if self.unicode {
                    Ok(Node::Char(self.unicode_escape_strict()?))
                } else {
                    Ok(Node::Char(self.unicode_escape()))
                }
            }
            Some(c) if c.is_ascii_digit() => {
                let mut num = c.to_digit(10).unwrap() as usize;
                while let Some(d) = self.peek() {
                    if d.is_ascii_digit() {
                        num = num * 10 + d.to_digit(10).unwrap() as usize;
                        self.bump();
                    } else {
                        break;
                    }
                }
                Ok(Node::Backref(num))
            }
            // IdentityEscape in Unicode mode is a SyntaxCharacter or '/' only.
            Some(c) if self.unicode && !is_regex_syntax_char(c) && c != '/' => {
                Err(format!("invalid identity escape \\{c} in unicode pattern"))
            }
            Some(c) => Ok(Node::Char(cp_of_elem(c))),
        }
    }

    /// Parse a `\p{Name}` / `\p{Name=Value}` body (the `\p`/`\P` already consumed). `negate` is true
    /// for `\P`. Returns `(negated, ranges)`. Only valid in Unicode mode; an unknown property errors.
    fn parse_prop_escape(&mut self, negate: bool) -> Result<(bool, &'static [(u32, u32)]), String> {
        if self.bump() != Some('{') {
            return Err("invalid property escape: expected '{'".into());
        }
        let mut body = String::new();
        loop {
            match self.bump() {
                Some('}') => break,
                // The grammar is `[A-Za-z0-9_]` names, optionally `name=value` — no spaces or other
                // characters (so `\p{ Gc=L }` with spaces is a SyntaxError, not loose-matched).
                Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '=' => body.push(c),
                Some(_) => return Err("invalid character in property escape".into()),
                None => return Err("unterminated property escape".into()),
            }
        }
        let (name, value) = match body.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (body.as_str(), None),
        };
        // Exact spellings only — `\p{…}` does not do UAX44 loose matching.
        match crate::unicode_props::lookup_strict(name, value) {
            Some((complement, ranges)) => Ok((negate != complement, ranges)),
            None => Err(format!("invalid unicode property {body}")),
        }
    }

    /// Read a `(?<name>` capture-group name (the `>` is consumed). A name is a `RegExpIdentifierName`:
    /// an IdentifierName, optionally using `\u` escapes, validated against ID_Start / ID_Continue.
    fn parse_group_name(&mut self) -> Result<String, String> {
        let mut name = String::new();
        loop {
            match self.peek() {
                Some('>') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.bump();
                    if self.peek() == Some('u') {
                        self.bump();
                        let mut cp = self.unicode_escape_u32();
                        // A `\uD8xx\uDCxx` lead/trail escape pair combines into one code point.
                        if (0xD800..=0xDBFF).contains(&cp)
                            && self.peek() == Some('\\')
                            && self.chars.get(self.pos + 1) == Some(&'u')
                        {
                            let save = self.pos;
                            self.bump();
                            self.bump();
                            let trail = self.unicode_escape_u32();
                            if (0xDC00..=0xDFFF).contains(&trail) {
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (trail - 0xDC00);
                            } else {
                                self.pos = save;
                            }
                        }
                        name.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                    } else {
                        return Err("invalid escape in capture group name".into());
                    }
                }
                Some(c) => {
                    self.bump();
                    // In non-unicode mode the elements are code units: recombine a smuggled
                    // surrogate pair into the character it encodes.
                    if let Some(&next) = self.chars.get(self.pos) {
                        if let Some(real) = crate::jstr::paired_char(c, next) {
                            self.bump();
                            name.push(real);
                            continue;
                        }
                    }
                    match crate::jstr::smuggled(c) {
                        // A truly lone surrogate can never be part of an identifier.
                        Some(_) => return Err("invalid capture group name".into()),
                        None => name.push(c),
                    }
                }
                None => return Err("unterminated capture group name".into()),
            }
        }
        let mut chars = name.chars();
        let valid =
            matches!(chars.next(), Some(c) if regex_ident_start(c)) && chars.all(regex_ident_part);
        if !valid {
            return Err(format!("invalid capture group name <{name}>"));
        }
        Ok(name)
    }

    /// Annex B ExtendedHexEscapeSequence: `\x` needs exactly `n` hex digits, otherwise the whole
    /// escape is an IdentityEscape for `esc` (consuming nothing past it).
    fn hex(&mut self, n: usize, esc: char) -> u32 {
        let save = self.pos;
        let mut s = String::new();
        for _ in 0..n {
            match self.peek() {
                Some(c) if c.is_ascii_hexdigit() => {
                    s.push(c);
                    self.bump();
                }
                _ => {
                    self.pos = save;
                    return esc as u32;
                }
            }
        }
        u32::from_str_radix(&s, 16).unwrap_or(0xFFFD)
    }

    /// Four hex digits as a raw value (surrogate halves pass through).
    fn hex4_u32(&mut self) -> u32 {
        let mut s = String::new();
        for _ in 0..4 {
            if let Some(c) = self.peek() {
                if c.is_ascii_hexdigit() {
                    s.push(c);
                    self.bump();
                }
            }
        }
        u32::from_str_radix(&s, 16).unwrap_or(0xFFFD)
    }

    fn unicode_escape(&mut self) -> u32 {
        self.unicode_escape_u32()
    }

    /// Exactly `n` hex digits, or a SyntaxError (Unicode mode).
    fn hex_strict(&mut self, n: usize) -> Result<u32, String> {
        let mut v: u32 = 0;
        for _ in 0..n {
            match self.peek().and_then(|c| c.to_digit(16)) {
                Some(d) => {
                    v = v * 16 + d;
                    self.bump();
                }
                None => return Err("invalid hexadecimal escape".into()),
            }
        }
        Ok(v)
    }

    /// A Unicode-mode `\u` escape: `{…}` bodies are strictly hex and capped at U+10FFFF, plain
    /// escapes are exactly four hex digits, and a lead/trail surrogate escape pair combines into
    /// one code point.
    fn unicode_escape_strict(&mut self) -> Result<u32, String> {
        if self.peek() == Some('{') {
            self.bump();
            let mut v: u32 = 0;
            let mut any = false;
            loop {
                match self.peek() {
                    Some('}') => {
                        self.bump();
                        break;
                    }
                    Some(c) if c.is_ascii_hexdigit() => {
                        any = true;
                        v = v.saturating_mul(16).saturating_add(c.to_digit(16).unwrap());
                        self.bump();
                    }
                    _ => return Err("invalid code point escape".into()),
                }
            }
            if !any || v > 0x10FFFF {
                return Err("invalid code point escape".into());
            }
            return Ok(v);
        }
        let mut lead: u32 = 0;
        for _ in 0..4 {
            match self.peek().and_then(|c| c.to_digit(16)) {
                Some(d) => {
                    lead = lead * 16 + d;
                    self.bump();
                }
                None => return Err("invalid unicode escape".into()),
            }
        }
        // Combine a surrogate escape pair into a single code point.
        if (0xD800..=0xDBFF).contains(&lead)
            && self.peek() == Some('\\')
            && self.chars.get(self.pos + 1) == Some(&'u')
        {
            let save = self.pos;
            self.bump();
            self.bump();
            let mut trail: u32 = 0;
            let mut ok = true;
            for _ in 0..4 {
                match self.peek().and_then(|c| c.to_digit(16)) {
                    Some(d) => {
                        trail = trail * 16 + d;
                        self.bump();
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && (0xDC00..=0xDFFF).contains(&trail) {
                let cp = 0x10000 + ((lead - 0xD800) << 10) + (trail - 0xDC00);
                return Ok(cp);
            }
            self.pos = save;
        }
        Ok(lead)
    }

    /// The raw code-point value of a `\u` escape body (surrogate values pass through).
    fn unicode_escape_u32(&mut self) -> u32 {
        if self.peek() == Some('{') {
            self.bump();
            let mut s = String::new();
            while let Some(c) = self.peek() {
                if c == '}' {
                    self.bump();
                    break;
                }
                s.push(c);
                self.bump();
            }
            u32::from_str_radix(&s, 16).unwrap_or(0xFFFD)
        } else {
            self.hex4_u32()
        }
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.bump() == Some(c) {
            Ok(())
        } else {
            Err(format!("expected '{c}' in pattern"))
        }
    }
}

enum ClassAtom {
    Char(u32),
    Builtin(char),
    Prop((bool, &'static [(u32, u32)])),
}

fn push_class_atom(cc: &mut CharClass, a: ClassAtom) {
    match a {
        ClassAtom::Char(c) => cc.ranges.push((c, c)),
        ClassAtom::Builtin(b) => cc.builtins.push(b),
        ClassAtom::Prop(p) => cc.props.push(p),
    }
}

// ---------------------------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------------------------

fn compile(node: &Node, prog: &mut Vec<Inst>, nmarks: &mut usize) -> Result<(), String> {
    match node {
        Node::Empty => {}
        Node::Char(c) => prog.push(Inst::Char(*c)),
        Node::Any => prog.push(Inst::Any),
        Node::Class(cc) => prog.push(Inst::Class(Rc::new(clone_class(cc)))),
        Node::Start => prog.push(Inst::AssertStart),
        Node::End => prog.push(Inst::AssertEnd),
        Node::WordB(b) => prog.push(Inst::WordBoundary(*b)),
        Node::Backref(n) => prog.push(Inst::Backref(*n)),
        Node::BackrefAlt(v) => prog.push(Inst::BackrefAlt(Rc::new(v.clone()))),
        // Resolved to `Backref` before compile; treat any stray one as group 0 (never matches).
        Node::NamedBackref(_) => prog.push(Inst::Backref(0)),
        Node::Modifier { add, remove, inner } => {
            let opt = |a: bool, r: bool| {
                if a {
                    Some(true)
                } else if r {
                    Some(false)
                } else {
                    None
                }
            };
            prog.push(Inst::PushFlags(
                opt(add.0, remove.0),
                opt(add.1, remove.1),
                opt(add.2, remove.2),
            ));
            compile(inner, prog, nmarks)?;
            prog.push(Inst::PopFlags);
        }
        Node::Concat(v) => {
            for n in v {
                compile(n, prog, nmarks)?;
            }
        }
        Node::Alt(v) => {
            let mut jmp_ends = Vec::new();
            for (i, alt) in v.iter().enumerate() {
                if i < v.len() - 1 {
                    let sp = prog.len();
                    prog.push(Inst::Split(0, 0));
                    let a_start = prog.len();
                    compile(alt, prog, nmarks)?;
                    jmp_ends.push(prog.len());
                    prog.push(Inst::Jmp(0));
                    let next = prog.len();
                    prog[sp] = Inst::Split(a_start, next);
                } else {
                    compile(alt, prog, nmarks)?;
                }
            }
            let end = prog.len();
            for j in jmp_ends {
                prog[j] = Inst::Jmp(end);
            }
        }
        Node::Group(idx, inner) => {
            if let Some(i) = idx {
                prog.push(Inst::Save(2 * i));
            }
            compile(inner, prog, nmarks)?;
            if let Some(i) = idx {
                prog.push(Inst::Save(2 * i + 1));
            }
        }
        Node::Look(negate, inner) => {
            let mut sub = Vec::new();
            compile(inner, &mut sub, nmarks)?;
            sub.push(Inst::Match);
            prog.push(Inst::Look {
                negate: *negate,
                prog: Rc::new(sub),
            });
        }
        Node::LookBehind(negate, inner) => {
            // The body is compiled from the REVERSED AST and executed right-to-left.
            let mut sub = Vec::new();
            compile(&reverse_node(inner), &mut sub, nmarks)?;
            sub.push(Inst::Match);
            prog.push(Inst::LookBehind {
                negate: *negate,
                prog: Rc::new(sub),
            });
        }
        Node::Repeat(inner, min, max, greedy) => {
            compile_repeat(inner, *min, *max, *greedy, prog, nmarks)?
        }
    }
    Ok(())
}

fn compile_repeat(
    inner: &Node,
    min: usize,
    max: Option<usize>,
    greedy: bool,
    prog: &mut Vec<Inst>,
    nmarks: &mut usize,
) -> Result<(), String> {
    // Fast path: a repeated single-character atom consumes iteratively (no per-character
    // recursion), so arbitrarily large counts (up to 2^53-1) cost nothing to compile.
    if let Some(rep) = single_char_rep(inner) {
        prog.push(Inst::Many {
            rep,
            min,
            max,
            greedy,
        });
        return Ok(());
    }
    // The general path unrolls `min` copies, so bound it to keep compiled programs small.
    if min > MAX_REPEAT || max.map(|m| m > MAX_REPEAT).unwrap_or(false) {
        return Err("repetition count too large".into());
    }
    // RepeatMatcher clears the captures inside the atom at the start of every iteration.
    let span = cap_span(inner);
    let body_with_clear = |prog: &mut Vec<Inst>, nmarks: &mut usize| -> Result<(), String> {
        if let Some((lo, hi)) = span {
            prog.push(Inst::ClearCaps(lo, hi));
        }
        compile(inner, prog, nmarks)
    };
    for _ in 0..min {
        body_with_clear(prog, nmarks)?;
    }
    // Optional iterations enforce the RepeatMatcher empty-iteration rule: an iteration that
    // consumes nothing fails, backtracking into the body's other alternatives or out of the loop.
    // Mark ids are globally unique across the whole pattern (nested sub-programs included).
    fn next_mark(nmarks: &mut usize) -> usize {
        let id = *nmarks;
        *nmarks += 1;
        id
    }
    match max {
        None => {
            // Greedy: L1: Split(body, end); body; Jmp(L1); end.
            let id = next_mark(nmarks);
            let l1 = prog.len();
            let sp = prog.len();
            prog.push(Inst::Split(0, 0));
            let body = prog.len();
            prog.push(Inst::SetMark(id));
            body_with_clear(prog, nmarks)?;
            prog.push(Inst::CheckProgress(id));
            prog.push(Inst::Jmp(l1));
            let end = prog.len();
            prog[sp] = if greedy {
                Inst::Split(body, end)
            } else {
                Inst::Split(end, body)
            };
        }
        Some(m) => {
            let extra = m.saturating_sub(min);
            let mut splits = Vec::new();
            for _ in 0..extra {
                let id = next_mark(nmarks);
                let sp = prog.len();
                prog.push(Inst::Split(0, 0));
                let body = prog.len();
                splits.push((sp, body));
                prog.push(Inst::SetMark(id));
                body_with_clear(prog, nmarks)?;
                prog.push(Inst::CheckProgress(id));
            }
            let end = prog.len();
            for (sp, body) in splits {
                prog[sp] = if greedy {
                    Inst::Split(body, end)
                } else {
                    Inst::Split(end, body)
                };
            }
        }
    }
    Ok(())
}

/// The AST with every concatenation reversed, so a forward compile of the result consumed
/// right-to-left implements backwards matching. Alternative ORDER is preserved; nested
/// lookarounds keep their own orientation (their compile handles direction independently).
fn reverse_node(node: &Node) -> Node {
    match node {
        Node::Concat(v) => Node::Concat(v.iter().rev().map(reverse_node).collect()),
        Node::Alt(v) => Node::Alt(v.iter().map(reverse_node).collect()),
        Node::Group(idx, inner) => Node::Group(*idx, Box::new(reverse_node(inner))),
        Node::Repeat(inner, min, max, greedy) => {
            Node::Repeat(Box::new(reverse_node(inner)), *min, *max, *greedy)
        }
        Node::Modifier { add, remove, inner } => Node::Modifier {
            add: *add,
            remove: *remove,
            inner: Box::new(reverse_node(inner)),
        },
        other => other.clone(),
    }
}

/// The min/max capture-group indices inside `node`, if any (for per-iteration capture resets).
fn cap_span(node: &Node) -> Option<(usize, usize)> {
    let merge = |a: Option<(usize, usize)>, b: Option<(usize, usize)>| match (a, b) {
        (Some((l1, h1)), Some((l2, h2))) => Some((l1.min(l2), h1.max(h2))),
        (x, None) | (None, x) => x,
    };
    match node {
        Node::Group(idx, inner) => merge(idx.map(|i| (i, i)), cap_span(inner)),
        Node::Concat(v) | Node::Alt(v) => v.iter().fold(None, |acc, n| merge(acc, cap_span(n))),
        Node::Repeat(inner, ..)
        | Node::Look(_, inner)
        | Node::LookBehind(_, inner)
        | Node::Modifier { inner, .. } => cap_span(inner),
        _ => None,
    }
}

/// The largest numeric back reference in the pattern (0 when there are none).
fn max_backref(node: &Node, out: &mut usize) {
    match node {
        Node::Backref(n) => *out = (*out).max(*n),
        Node::Concat(items) | Node::Alt(items) => {
            for n in items {
                max_backref(n, out);
            }
        }
        Node::Group(_, inner)
        | Node::Repeat(inner, _, _, _)
        | Node::Look(_, inner)
        | Node::LookBehind(_, inner)
        | Node::Modifier { inner, .. } => max_backref(inner, out),
        _ => {}
    }
}

/// Replace each `\k<name>` (`Node::NamedBackref`) with the numeric `Backref` of its group. Names are
/// validated before this runs, so an unknown name resolves to group 0 (never matches), harmlessly.
/// Reject same-name capture groups that could both match (i.e. live in the same concatenation);
/// duplicates spread across different alternation branches are allowed (ES2025).
fn validate_group_names(node: &Node, names: &[(String, usize)]) -> Result<(), String> {
    collect_group_names(node, names)?;
    Ok(())
}

fn collect_group_names(
    node: &Node,
    names: &[(String, usize)],
) -> Result<std::collections::HashSet<String>, String> {
    use std::collections::HashSet;
    match node {
        Node::Group(idx, inner) => {
            let mut s = collect_group_names(inner, names)?;
            if let Some(idx) = idx {
                if let Some((name, _)) = names.iter().find(|(_, i)| i == idx) {
                    if !s.insert(name.clone()) {
                        return Err(format!("duplicate group name {name}"));
                    }
                }
            }
            Ok(s)
        }
        Node::Look(_, inner) | Node::LookBehind(_, inner) | Node::Repeat(inner, _, _, _) => {
            collect_group_names(inner, names)
        }
        Node::Modifier { inner, .. } => collect_group_names(inner, names),
        Node::Concat(children) => {
            let mut all = HashSet::new();
            for c in children {
                for n in collect_group_names(c, names)? {
                    if !all.insert(n.clone()) {
                        return Err(format!("duplicate group name {n}"));
                    }
                }
            }
            Ok(all)
        }
        Node::Alt(branches) => {
            let mut union = HashSet::new();
            for b in branches {
                union.extend(collect_group_names(b, names)?);
            }
            Ok(union)
        }
        _ => Ok(std::collections::HashSet::new()),
    }
}

fn resolve_named_backrefs(node: &mut Node, names: &[(String, usize)]) {
    match node {
        Node::NamedBackref(name) => {
            let idxs: Vec<usize> = names
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, i)| *i)
                .collect();
            *node = match idxs.len() {
                0 => Node::Backref(0),
                1 => Node::Backref(idxs[0]),
                _ => Node::BackrefAlt(idxs),
            };
        }
        Node::Concat(v) | Node::Alt(v) => {
            v.iter_mut().for_each(|n| resolve_named_backrefs(n, names))
        }
        Node::Group(_, inner)
        | Node::Repeat(inner, ..)
        | Node::Look(_, inner)
        | Node::LookBehind(_, inner)
        | Node::Modifier { inner, .. } => resolve_named_backrefs(inner, names),
        _ => {}
    }
}

/// If `node` matches exactly one code point, return it as a `Rep` (for the `Inst::Many` fast path).
fn single_char_rep(node: &Node) -> Option<Rep> {
    match node {
        Node::Char(c) => Some(Rep::Char(*c)),
        Node::Any => Some(Rep::Any),
        Node::Class(cc) => Some(Rep::Class(Rc::new(clone_class(cc)))),
        _ => None,
    }
}

fn clone_class(cc: &CharClass) -> CharClass {
    CharClass {
        negate: cc.negate,
        ranges: cc.ranges.clone(),
        builtins: cc.builtins.clone(),
        props: cc.props.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// Backtracking matcher
// ---------------------------------------------------------------------------------------------

/// Recursion-depth ceiling for the backtracking matcher (separate from the step budget): a long
/// input against a greedy quantifier recurses once per consumed char, which would overflow the
/// native stack on big inputs.
const MAX_MATCH_DEPTH: u32 = 3000;

struct Matcher<'a> {
    input: &'a [u32],
    caps: Vec<Option<usize>>,
    marks: Vec<Option<usize>>,
    steps: u64,
    depth: u32,
    /// Matching direction: a lookbehind body (compiled from the reversed AST) consumes leftward.
    back: bool,
    /// `(icase, multiline, dotall)` stack — the base flags, plus an entry per active `(?ims-ims:…)`
    /// inline-modifier group. Reads use the top; the group's Push/Pop instructions undo on backtrack.
    flags: Vec<(bool, bool, bool)>,
    /// Unicode mode (`u`/`v`): case-insensitive matching uses full case folding instead of the
    /// legacy Canonicalize (simple uppercase, never folding non-ASCII to ASCII).
    unicode: bool,
}

impl Matcher<'_> {
    fn icase(&self) -> bool {
        self.flags.last().unwrap().0
    }
    fn multiline(&self) -> bool {
        self.flags.last().unwrap().1
    }
    fn dotall(&self) -> bool {
        self.flags.last().unwrap().2
    }
    /// Compare two subject/pattern code points under the active case rules.
    fn eqc_uu(&self, a: u32, b: u32) -> bool {
        if a == b {
            return true;
        }
        if self.icase() {
            let (ca, cb) = match (char::from_u32(a), char::from_u32(b)) {
                (Some(x), Some(y)) => (x, y),
                _ => return false, // lone surrogates have no case
            };
            if self.unicode {
                // Full case folding via the generated orbit table (ſ≡s, ΐ≡ΐ, K≡k, ...).
                return fold_canon(ca as u32) == fold_canon(cb as u32);
            }
            return canonicalize_legacy(ca) == canonicalize_legacy(cb);
        }
        false
    }

    /// The next element to consume and the position after it, honouring the match direction.
    fn step(&self, pos: usize) -> Option<(u32, usize)> {
        if self.back {
            if pos > 0 {
                Some((self.input[pos - 1], pos - 1))
            } else {
                None
            }
        } else if pos < self.input.len() {
            Some((self.input[pos], pos + 1))
        } else {
            None
        }
    }

    /// Consume a backreference's captured text (element order preserved in both directions).
    fn backref_step(&mut self, prog: &[Inst], pc: usize, pos: usize, text: &[u32]) -> bool {
        let n = text.len();
        if self.back {
            if pos >= n && (0..n).all(|i| self.eqc_uu(self.input[pos - n + i], text[i])) {
                self.run(prog, pc + 1, pos - n)
            } else {
                false
            }
        } else if pos + n <= self.input.len()
            && (0..n).all(|i| self.eqc_uu(self.input[pos + i], text[i]))
        {
            self.run(prog, pc + 1, pos + n)
        } else {
            false
        }
    }

    fn rep_matches(&self, rep: &Rep, c: u32) -> bool {
        match rep {
            Rep::Char(ch) => self.eqc_uu(c, *ch),
            Rep::Any => self.dotall() || c != '\n' as u32,
            Rep::Class(cc) => cc.matches(c, self.icase(), self.unicode),
        }
    }

    fn run(&mut self, prog: &[Inst], pc: usize, pos: usize) -> bool {
        self.steps += 1;
        if self.steps > STEP_LIMIT || self.depth > MAX_MATCH_DEPTH {
            return false;
        }
        self.depth += 1;
        let r = self.run_inner(prog, pc, pos);
        self.depth -= 1;
        r
    }

    fn run_inner(&mut self, prog: &[Inst], pc: usize, pos: usize) -> bool {
        match &prog[pc] {
            Inst::Match => true,
            Inst::Char(c) => match self.step(pos) {
                Some((e, next)) if self.eqc_uu(e, *c) => self.run(prog, pc + 1, next),
                _ => false,
            },
            Inst::Any => match self.step(pos) {
                Some((e, next)) if self.dotall() || !is_line_terminator_u32(e) => {
                    self.run(prog, pc + 1, next)
                }
                _ => false,
            },
            Inst::Class(cc) => match self.step(pos) {
                Some((e, next)) if cc.matches(e, self.icase(), self.unicode) => {
                    self.run(prog, pc + 1, next)
                }
                _ => false,
            },
            Inst::Save(slot) => {
                let slot = *slot;
                let old = self.caps[slot];
                self.caps[slot] = Some(pos);
                if self.run(prog, pc + 1, pos) {
                    true
                } else {
                    self.caps[slot] = old;
                    false
                }
            }
            Inst::Split(a, b) => {
                let (a, b) = (*a, *b);
                self.run(prog, a, pos) || self.run(prog, b, pos)
            }
            Inst::SetMark(id) => {
                let id = *id;
                let old = self.marks[id];
                self.marks[id] = Some(pos);
                if self.run(prog, pc + 1, pos) {
                    true
                } else {
                    self.marks[id] = old;
                    false
                }
            }
            Inst::CheckProgress(id) => {
                if self.marks[*id] == Some(pos) {
                    false
                } else {
                    self.run(prog, pc + 1, pos)
                }
            }
            Inst::Many {
                rep,
                min,
                max,
                greedy,
            } => {
                let (min, max, greedy) = (*min, *max, *greedy);
                // Consume as many as the input allows (up to `max`), iteratively.
                let cap = max.unwrap_or(usize::MAX);
                let room = if self.back {
                    pos
                } else {
                    self.input.len() - pos
                };
                let idx = |k: usize| if self.back { pos - 1 - k } else { pos + k };
                let mut avail = 0;
                while avail < cap && avail < room && self.rep_matches(rep, self.input[idx(avail)]) {
                    avail += 1;
                }
                if avail < min {
                    return false;
                }
                // Backtrack the count (greedy: high→min; lazy: min→high), recursing only on the
                // continuation, so a run of N characters costs O(N) here plus one match per attempt.
                let cont = |m: &mut Self, n: usize| {
                    let p = if m.back { pos - n } else { pos + n };
                    m.run(prog, pc + 1, p)
                };
                if greedy {
                    let mut n = avail;
                    loop {
                        if cont(self, n) {
                            return true;
                        }
                        if n == min {
                            return false;
                        }
                        n -= 1;
                    }
                } else {
                    let mut n = min;
                    loop {
                        if cont(self, n) {
                            return true;
                        }
                        if n == avail {
                            return false;
                        }
                        n += 1;
                    }
                }
            }
            Inst::PushFlags(i, m, s) => {
                let cur = *self.flags.last().unwrap();
                let new = (i.unwrap_or(cur.0), m.unwrap_or(cur.1), s.unwrap_or(cur.2));
                self.flags.push(new);
                if self.run(prog, pc + 1, pos) {
                    true
                } else {
                    self.flags.pop(); // undo on backtrack
                    false
                }
            }
            Inst::PopFlags => {
                let popped = self.flags.pop().unwrap();
                if self.run(prog, pc + 1, pos) {
                    true
                } else {
                    self.flags.push(popped); // undo on backtrack
                    false
                }
            }
            Inst::Jmp(t) => self.run(prog, *t, pos),
            Inst::AssertStart => {
                let ok =
                    pos == 0 || (self.multiline() && is_line_terminator_u32(self.input[pos - 1]));
                ok && self.run(prog, pc + 1, pos)
            }
            Inst::AssertEnd => {
                let ok = pos == self.input.len()
                    || (self.multiline() && is_line_terminator_u32(self.input[pos]));
                ok && self.run(prog, pc + 1, pos)
            }
            Inst::WordBoundary(want) => {
                let (icase, unicode) = (self.icase(), self.unicode);
                let before = pos > 0 && is_word_ic(self.input[pos - 1], icase, unicode);
                let after = pos < self.input.len() && is_word_ic(self.input[pos], icase, unicode);
                let boundary = before != after;
                (boundary == *want) && self.run(prog, pc + 1, pos)
            }
            Inst::Backref(g) => {
                let g = *g;
                if g == 0 || 2 * g + 1 >= self.caps.len() {
                    return self.run(prog, pc + 1, pos); // invalid group: matches empty
                }
                match (self.caps[2 * g], self.caps[2 * g + 1]) {
                    (Some(a), Some(b)) => {
                        let (a, b) = (a.min(b), a.max(b));
                        let text: Vec<u32> = self.input[a..b].to_vec();
                        self.backref_step(prog, pc, pos, &text)
                    }
                    _ => self.run(prog, pc + 1, pos), // unset group matches empty
                }
            }
            Inst::BackrefAlt(idxs) => {
                // At most one same-named group can have captured; match through that one.
                let g = idxs.iter().copied().find(|&g| {
                    2 * g + 1 < self.caps.len()
                        && self.caps[2 * g].is_some()
                        && self.caps[2 * g + 1].is_some()
                });
                match g {
                    None => self.run(prog, pc + 1, pos), // no group captured: matches empty
                    Some(g) => {
                        let (a, b) = (self.caps[2 * g].unwrap(), self.caps[2 * g + 1].unwrap());
                        let (a, b) = (a.min(b), a.max(b));
                        let text: Vec<u32> = self.input[a..b].to_vec();
                        self.backref_step(prog, pc, pos, &text)
                    }
                }
            }
            Inst::ClearCaps(lo, hi) => {
                let (lo, hi) = (*lo, *hi);
                let saved: Vec<Option<usize>> = self.caps[2 * lo..2 * hi + 2].to_vec();
                for s in &mut self.caps[2 * lo..2 * hi + 2] {
                    *s = None;
                }
                if self.run(prog, pc + 1, pos) {
                    true
                } else {
                    self.caps[2 * lo..2 * hi + 2].copy_from_slice(&saved);
                    false
                }
            }
            Inst::Look { negate, prog: sub } => {
                let negate = *negate;
                let sub = sub.clone();
                let saved = self.caps.clone();
                // A nested lookahead always matches forward, even inside a lookbehind body.
                let saved_back = std::mem::replace(&mut self.back, false);
                let matched = self.run(&sub, 0, pos);
                self.back = saved_back;
                if negate {
                    self.caps = saved; // negative lookahead: discard captures
                    if matched {
                        false
                    } else {
                        self.run(prog, pc + 1, pos)
                    }
                } else if matched {
                    self.run(prog, pc + 1, pos)
                } else {
                    self.caps = saved;
                    false
                }
            }
            Inst::LookBehind { negate, prog: sub } => {
                let negate = *negate;
                let sub = sub.clone();
                let saved = self.caps.clone();
                // The body (compiled from the reversed AST) matches RIGHT-TO-LEFT from `pos`, so
                // alternative order, greed, and captures follow the spec's backwards semantics.
                let saved_back = std::mem::replace(&mut self.back, true);
                let matched = self.run(&sub, 0, pos);
                self.back = saved_back;
                if negate {
                    self.caps = saved; // negative lookbehind: discard captures
                    if matched {
                        false
                    } else {
                        self.run(prog, pc + 1, pos)
                    }
                } else if matched {
                    self.run(prog, pc + 1, pos)
                } else {
                    self.caps = saved;
                    false
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// `v`-flag (unicodeSets) character classes: ClassSetExpressions are evaluated at parse time into
// a concrete set of code-point ranges plus a set of multi-code-point strings.
// ---------------------------------------------------------------------------------------------

/// A `v`-mode class set: sorted, disjoint code-point ranges plus multi-code-point strings.
#[derive(Default, Clone)]
struct ClassSet {
    ranges: Vec<(u32, u32)>,
    strings: Vec<Vec<char>>,
}

impl ClassSet {
    fn normalize(&mut self) {
        self.ranges.sort_unstable();
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(self.ranges.len());
        for &(lo, hi) in &self.ranges {
            if let Some(last) = out.last_mut() {
                if lo <= last.1.saturating_add(1) {
                    last.1 = last.1.max(hi);
                    continue;
                }
            }
            out.push((lo, hi));
        }
        self.ranges = out;
        self.strings.sort();
        self.strings.dedup();
    }

    fn union(mut self, other: ClassSet) -> ClassSet {
        self.ranges.extend(other.ranges);
        self.strings.extend(other.strings);
        self.normalize();
        self
    }

    fn intersect(mut self, other: ClassSet) -> ClassSet {
        let mut ranges = Vec::new();
        for &(a, b) in &self.ranges {
            for &(c, d) in &other.ranges {
                let lo = a.max(c);
                let hi = b.min(d);
                if lo <= hi {
                    ranges.push((lo, hi));
                }
            }
        }
        self.strings.retain(|s| other.strings.contains(s));
        self.ranges = ranges;
        self.normalize();
        self
    }

    fn subtract(mut self, other: ClassSet) -> ClassSet {
        let mut ranges = self.ranges.clone();
        for &(c, d) in &other.ranges {
            let mut next = Vec::with_capacity(ranges.len() + 1);
            for &(a, b) in &ranges {
                if d < a || c > b {
                    next.push((a, b));
                    continue;
                }
                if a < c {
                    next.push((a, c - 1));
                }
                if b > d {
                    next.push((d + 1, b));
                }
            }
            ranges = next;
        }
        self.strings.retain(|s| !other.strings.contains(s));
        self.ranges = ranges;
        self.normalize();
        self
    }

    /// Complement over the full code-point space. A set containing strings may not be negated.
    fn complement(mut self) -> Result<ClassSet, String> {
        if !self.strings.is_empty() {
            return Err("cannot negate a class set containing strings".into());
        }
        self.normalize();
        let mut out = Vec::new();
        let mut next = 0u32;
        for &(lo, hi) in &self.ranges {
            if lo > next {
                out.push((next, lo - 1));
            }
            next = hi.saturating_add(1);
        }
        if next <= 0x10FFFF {
            out.push((next, 0x10FFFF));
        }
        self.ranges = out;
        Ok(self)
    }

    fn from_cp(c: u32) -> ClassSet {
        ClassSet {
            ranges: vec![(c, c)],
            strings: Vec::new(),
        }
    }
}

/// The concrete ranges of a `\d`/`\w`/`\s` class escape (for `v`-mode set arithmetic).
fn builtin_class_set(b: char) -> ClassSet {
    let base = match b.to_ascii_lowercase() {
        'd' => vec![(0x30, 0x39)],
        'w' => vec![(0x30, 0x39), (0x41, 0x5A), (0x5F, 0x5F), (0x61, 0x7A)],
        's' => {
            let mut r = vec![
                (0x09, 0x0D),
                (0x20, 0x20),
                (0x85, 0x85),
                (0xA0, 0xA0),
                (0x1680, 0x1680),
                (0x2000, 0x200A),
                (0x2028, 0x2029),
                (0x202F, 0x202F),
                (0x205F, 0x205F),
                (0x3000, 0x3000),
                (0xFEFF, 0xFEFF),
            ];
            r.sort_unstable();
            r
        }
        _ => Vec::new(),
    };
    let mut set = ClassSet {
        ranges: base,
        strings: Vec::new(),
    };
    if b.is_ascii_uppercase() {
        set = set.complement().unwrap();
    }
    set
}

/// The derivable Unicode "properties of strings" (UTS #51 definitions built from the bundled
/// emoji binary-property tables). The RGI_* curated lists are not derivable and stay unsupported.
fn property_of_strings(name: &str) -> Option<ClassSet> {
    let ranges_of = |prop: &str| -> Vec<(u32, u32)> {
        crate::unicode_props::lookup(prop, None)
            .map(|r| r.to_vec())
            .unwrap_or_default()
    };
    match name {
        "Basic_Emoji" => {
            // Emoji_Presentation singletons, plus (Emoji minus Emoji_Presentation) + FE0F.
            let ep = ClassSet {
                ranges: ranges_of("Emoji_Presentation"),
                strings: Vec::new(),
            };
            let emoji = ClassSet {
                ranges: ranges_of("Emoji"),
                strings: Vec::new(),
            };
            let text_only = emoji.subtract(ep.clone());
            let mut strings = Vec::new();
            for &(lo, hi) in &text_only.ranges {
                for u in lo..=hi {
                    if let Some(c) = char::from_u32(u) {
                        strings.push(vec![c, '\u{FE0F}']);
                    }
                }
            }
            let mut set = ep;
            set.strings = strings;
            set.normalize();
            Some(set)
        }
        "Emoji_Keycap_Sequence" => {
            let mut strings = Vec::new();
            for c in "#*0123456789".chars() {
                strings.push(vec![c, '\u{FE0F}', '\u{20E3}']);
            }
            Some(ClassSet {
                ranges: Vec::new(),
                strings,
            })
        }
        "RGI_Emoji_Modifier_Sequence" => {
            let bases = ranges_of("Emoji_Modifier_Base");
            let mut strings = Vec::new();
            for &(lo, hi) in &bases {
                for u in lo..=hi {
                    if let Some(c) = char::from_u32(u) {
                        for m in 0x1F3FB..=0x1F3FF {
                            strings.push(vec![c, char::from_u32(m).unwrap()]);
                        }
                    }
                }
            }
            Some(ClassSet {
                ranges: Vec::new(),
                strings,
            })
        }
        "RGI_Emoji_Flag_Sequence" => Some(ClassSet {
            ranges: Vec::new(),
            strings: crate::regex_emoji::RGI_FLAG_SEQUENCES
                .iter()
                .map(|s| s.chars().collect())
                .collect(),
        }),
        "RGI_Emoji_ZWJ_Sequence" => Some(ClassSet {
            ranges: Vec::new(),
            strings: crate::regex_emoji::RGI_ZWJ_SEQUENCES
                .iter()
                .map(|s| s.chars().collect())
                .collect(),
        }),
        "RGI_Emoji" => {
            // The union table: single code points join the ranges, sequences the strings.
            let mut set = ClassSet {
                ranges: Vec::new(),
                strings: Vec::new(),
            };
            for s in crate::regex_emoji::RGI_EMOJI_ALL {
                let cs: Vec<char> = s.chars().collect();
                if cs.len() == 1 {
                    set.ranges.push((cs[0] as u32, cs[0] as u32));
                } else {
                    set.strings.push(cs);
                }
            }
            set.normalize();
            Some(set)
        }
        "RGI_Emoji_Tag_Sequence" => {
            // The three RGI tag sequences: england, scotland, wales.
            let mk = |tags: &str| {
                let mut v = vec!['\u{1F3F4}'];
                for c in tags.chars() {
                    v.push(char::from_u32(0xE0000 + c as u32).unwrap());
                }
                v.push('\u{E007F}');
                v
            };
            Some(ClassSet {
                ranges: Vec::new(),
                strings: vec![mk("gbeng"), mk("gbsct"), mk("gbwls")],
            })
        }
        _ => None,
    }
}

/// A `\q{...}` alternative: a single char joins the ranges; longer sequences join the strings.
fn push_q_alternative(set: &mut ClassSet, alt: Vec<char>) {
    match alt.len() {
        0 => set.strings.push(Vec::new()),
        1 => set.ranges.push((alt[0] as u32, alt[0] as u32)),
        _ => set.strings.push(alt),
    }
}

/// Compile a computed class set: an alternation of its strings (longest first, so the greedy
/// match prefers the longest sequence) plus a plain range class. Lone-surrogate ranges are
/// dropped (input is scalar values).
fn class_set_to_node(mut set: ClassSet) -> Node {
    set.normalize();
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for &(lo, hi) in &set.ranges {
        let mut push = |a: u32, b: u32| {
            if a <= b {
                ranges.push((a, b));
            }
        };
        if lo <= 0xD7FF && hi >= 0xE000 {
            push(lo, 0xD7FF);
            push(0xE000, hi);
        } else if !(0xD800..=0xDFFF).contains(&lo) || !(0xD800..=0xDFFF).contains(&hi) {
            push(lo.clamp(0, 0x10FFFF), hi.min(0x10FFFF));
        }
    }
    let class = Node::Class(CharClass {
        negate: false,
        ranges,
        builtins: Vec::new(),
        props: Vec::new(),
    });
    if set.strings.is_empty() {
        return class;
    }
    let mut strings = set.strings;
    strings.sort_by_key(|b| std::cmp::Reverse(b.len()));
    let mut alts: Vec<Node> = strings
        .into_iter()
        .map(|cs| {
            if cs.is_empty() {
                Node::Empty
            } else {
                Node::Concat(cs.into_iter().map(|c| Node::Char(c as u32)).collect())
            }
        })
        .collect();
    alts.push(class);
    Node::Group(None, Box::new(Node::Alt(alts)))
}
