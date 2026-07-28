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
const INLINE_CAPTURES: usize = 4;

pub(crate) enum Captures {
    Inline {
        len: u8,
        spans: [Option<(usize, usize)>; INLINE_CAPTURES],
    },
    Heap(Box<[Option<(usize, usize)>]>),
}

impl Captures {
    fn from_vec(spans: Vec<Option<(usize, usize)>>) -> Self {
        if spans.len() <= INLINE_CAPTURES {
            let mut inline = [None; INLINE_CAPTURES];
            inline[..spans.len()].copy_from_slice(&spans);
            Captures::Inline {
                len: spans.len() as u8,
                spans: inline,
            }
        } else {
            Captures::Heap(spans.into_boxed_slice())
        }
    }

    fn one(span: (usize, usize)) -> Self {
        let mut spans = [None; INLINE_CAPTURES];
        spans[0] = Some(span);
        Captures::Inline { len: 1, spans }
    }
}

impl std::ops::Deref for Captures {
    type Target = [Option<(usize, usize)>];
    fn deref(&self) -> &Self::Target {
        match self {
            Captures::Inline { len, spans } => &spans[..*len as usize],
            Captures::Heap(spans) => spans,
        }
    }
}

impl AsRef<[Option<(usize, usize)>]> for Captures {
    fn as_ref(&self) -> &[Option<(usize, usize)>] {
        self
    }
}

/// A compiled regular expression.
pub struct Regex {
    prog: Vec<Inst>,
    /// A linear-time byte automaton for the common ASCII-compatible subset. JavaScript regexp
    /// syntax outside that subset (and every non-ASCII subject) stays on the complete matcher
    /// below. Keeping this as an optional acceleration tier means conformance never depends on
    /// the host regexp dialect.
    fast_ascii: Option<regex::bytes::Regex>,
    /// Capture-location storage is sized by the compiled automaton and recycled across matches.
    /// Matching cannot invoke JavaScript, so a regexp cannot re-enter this cell while borrowed.
    fast_ascii_locs: Option<std::cell::RefCell<regex::bytes::CaptureLocations>>,
    /// Backreference-capable ASCII accelerator for a conservative subset whose capture
    /// participation is statically guaranteed. Runtime errors fall back to the complete matcher.
    fast_fancy_ascii: Option<fancy_regex::Regex>,
    /// Unicode-string automaton for legacy patterns whose semantics can be projected exactly.
    /// It matches the smuggled UTF-16 element string in `ReText::wide_src`.
    fast_wide: Option<regex::Regex>,
    fast_wide_locs: Option<std::cell::RefCell<regex::CaptureLocations>>,
    nmarks: usize,
    /// Start-position prescan derived from the program (see [`first_filter`]): lets the scan
    /// skip positions that cannot begin a match instead of running the backtracker at each.
    first: FirstFilter,
    /// [`FirstFilter::Atoms`] baked into a byte-indexed table (elements < 256): the scan loop
    /// becomes one load per position.
    first_lut: Option<Box<[bool; 256]>>,
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

/// What the compiled program says about how a match can begin.
enum FirstFilter {
    /// No usable information — the scan tries every position.
    None,
    /// Every path first asserts `^` in non-multiline mode: a match can only begin at position 0,
    /// so one attempt decides the whole scan.
    Anchored,
    /// Every path begins by consuming one element matching one of these atoms; positions whose
    /// element matches none can be skipped without entering the backtracker. The predicate is a
    /// superset of what the matcher accepts, so a pass is never wrong — only a reject is binding.
    Atoms(Vec<Rep>),
}

/// Compute the [`FirstFilter`] by ε-walking the program from its entry: through saves, jumps,
/// splits, capture clears and marks, collecting the first thing each path does. Anything not
/// modelled (assertions other than a uniform leading `^`, backrefs, lookarounds, inline flags,
/// or an ε-reachable `Match` — an empty-matchable pattern) disables the filter.
fn first_filter(prog: &[Inst], multiline: bool) -> FirstFilter {
    let mut atoms: Vec<Rep> = Vec::new();
    let mut asserts = 0usize;
    let mut stack = vec![0usize];
    let mut seen = vec![false; prog.len()];
    while let Some(pc) = stack.pop() {
        if seen[pc] {
            continue;
        }
        seen[pc] = true;
        match &prog[pc] {
            Inst::Save(_) | Inst::ClearCaps(..) | Inst::SetMark(_) => stack.push(pc + 1),
            Inst::Jmp(t) => stack.push(*t),
            Inst::Split(a, b) => {
                stack.push(*a);
                stack.push(*b);
            }
            Inst::Char(c) => atoms.push(Rep::Char(*c)),
            Inst::Any => atoms.push(Rep::Any),
            Inst::Class(cc) => atoms.push(Rep::Class(cc.clone())),
            Inst::Many { rep, min, .. } => {
                atoms.push(rep.clone());
                if *min == 0 {
                    stack.push(pc + 1); // may consume nothing — the next inst also "begins" a path
                }
            }
            Inst::AssertStart => asserts += 1,
            _ => return FirstFilter::None,
        }
    }
    if asserts > 0 {
        if atoms.is_empty() && !multiline {
            FirstFilter::Anchored
        } else {
            FirstFilter::None
        }
    } else if !atoms.is_empty() {
        FirstFilter::Atoms(atoms)
    } else {
        FirstFilter::None
    }
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
    /// Total capturing groups in the whole pattern (prescanned): Annex B decides decimal escapes
    /// (backreference vs legacy octal) against this count.
    total_groups: usize,
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

/// A subject string prepared for matching: its elements plus each element's unit offset.
/// `unit_of` is `None` when element index == unit offset (always true in non-unicode mode, and in
/// unicode mode for BMP-only subjects); otherwise `unit_of.len() == elems.len() + 1` with the last
/// entry the total unit length. JS-visible indices (lastIndex, match.index) are unit offsets.
pub struct ReText {
    /// Wide elements — EMPTY for an ASCII subject, which matches over `ascii_src`'s bytes
    /// directly (see `Regex::exec_text`) with no per-element materialization at all.
    pub elems: Vec<u32>,
    pub unit_of: Option<Vec<usize>>,
    /// Element count (== `ascii_src` byte length for ASCII, else `elems.len()`).
    n_elems: usize,
    unicode: bool,
    /// The source string when it is pure ASCII (element index == byte index): matching runs
    /// over its bytes and `slice` copies straight out of it.
    ascii_src: Option<crate::lstr::LStr>,
    /// Non-ASCII legacy (non-u/v) subjects encoded as one Rust scalar per UTF-16 code unit.
    wide_src: Option<String>,
    /// Byte offset of each element boundary in `wide_src` (length = n_elems + 1).
    wide_offsets: Vec<usize>,
}

impl ReText {
    /// Prepare `s` for matching, keeping the caller's `Rc` for zero-copy ASCII slicing.
    pub fn new_rc(unicode: bool, s: &crate::lstr::LStr) -> ReText {
        // Engine strings maintain an exact one-way ASCII hint in their allocation header.
        // RegExp workloads commonly stream many distinct ASCII subjects through the tiny
        // identity cache; consulting the hint avoids rescanning every subject just to select
        // the byte matcher.
        if s.ascii_hint() {
            return ReText {
                elems: Vec::new(),
                unit_of: None,
                n_elems: s.len(),
                unicode,
                ascii_src: Some(s.clone()),
                wide_src: None,
                wide_offsets: Vec::new(),
            };
        }
        // Keep the engine string itself: `LStr::clone` is one refcount bump and its immutable
        // bytes can be matched and sliced directly.
        Self::build(unicode, s, Some(s.clone()))
    }

    fn build(unicode: bool, s: &str, src: Option<crate::lstr::LStr>) -> ReText {
        // ASCII: elements are the bytes, and element index == unit offset in both modes.
        if s.is_ascii() {
            return ReText {
                elems: Vec::new(),
                unit_of: None,
                n_elems: s.len(),
                unicode,
                ascii_src: Some(src.unwrap_or_else(|| crate::lstr::LStr::from(s))),
                wide_src: None,
                wide_offsets: Vec::new(),
            };
        }
        if unicode {
            let cps = crate::jstr::code_points(s);
            if cps.iter().all(|&cp| cp < 0x10000) {
                // BMP-only: one unit per element.
                return ReText {
                    n_elems: cps.len(),
                    elems: cps,
                    unit_of: None,
                    unicode,
                    ascii_src: None,
                    wide_src: None,
                    wide_offsets: Vec::new(),
                };
            }
            let mut unit_of = Vec::with_capacity(cps.len() + 1);
            let mut u = 0usize;
            for &cp in &cps {
                unit_of.push(u);
                u += if cp >= 0x10000 { 2 } else { 1 };
            }
            unit_of.push(u);
            ReText {
                n_elems: cps.len(),
                elems: cps,
                unit_of: Some(unit_of),
                unicode,
                ascii_src: None,
                wide_src: None,
                wide_offsets: Vec::new(),
            }
        } else {
            let units = crate::jstr::units(s);
            let mut wide_src = String::with_capacity(s.len());
            let mut wide_offsets = Vec::with_capacity(units.len() + 1);
            for &unit in &units {
                wide_offsets.push(wide_src.len());
                wide_src.push(elem_of_cp(unit as u32));
            }
            wide_offsets.push(wide_src.len());
            ReText {
                n_elems: units.len(),
                elems: units.iter().map(|&u| u as u32).collect(),
                unit_of: None,
                unicode,
                ascii_src: None,
                wide_src: Some(wide_src),
                wide_offsets,
            }
        }
    }

    /// The element index containing unit offset `u` (== len when `u` is at/past the end).
    pub fn elem_at_unit(&self, u: usize) -> usize {
        match &self.unit_of {
            None => u.min(self.n_elems),
            Some(unit_of) => match unit_of.binary_search(&u) {
                Ok(k) => k.min(self.n_elems),
                Err(k) => k - 1,
            },
        }
    }

    /// The unit offset of element `e`.
    pub fn unit_index(&self, e: usize) -> usize {
        match &self.unit_of {
            None => e.min(self.n_elems),
            Some(unit_of) => unit_of[e.min(self.n_elems)],
        }
    }

    fn wide_byte(&self, element: usize) -> usize {
        self.wide_offsets[element.min(self.n_elems)]
    }

    fn wide_element(&self, byte: usize) -> Option<usize> {
        self.wide_offsets.binary_search(&byte).ok()
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.n_elems
    }

    /// The canonical string for elements `a..b` (surrogate halves recombine).
    pub fn slice(&self, a: usize, b: usize) -> String {
        // ASCII subject: element index == byte index — copy straight from the source.
        if let Some(src) = &self.ascii_src {
            return src[a..b].to_string();
        }
        let elems = &self.elems[a..b];
        // ASCII fast path: elements are the bytes.
        if elems.iter().all(|&e| e < 0x80) {
            let bytes: Vec<u8> = elems.iter().map(|&e| e as u8).collect();
            return String::from_utf8(bytes).unwrap();
        }
        if self.unicode {
            crate::jstr::from_code_points(elems)
        } else {
            let units: Vec<u16> = elems.iter().map(|&e| e as u16).collect();
            crate::jstr::from_units(&units)
        }
    }
}

/// Count the capturing groups in a pattern (escapes and classes skipped): `(` not followed by
/// `?`, plus named groups `(?<name>`.
fn count_capture_groups(chars: &[char]) -> usize {
    let mut n = 0;
    let mut i = 0;
    let mut in_class = false;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => {
                let plain = chars.get(i + 1) != Some(&'?');
                let named = chars.get(i + 2) == Some(&'<')
                    && !matches!(chars.get(i + 3), Some('=') | Some('!'));
                if plain || named {
                    n += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    n
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

/// Project legacy `\uXXXX` atoms onto the ASCII alphabet used by the byte matcher.
///
/// This is only consulted for subjects already proven entirely ASCII. Code points above 0x7f
/// can therefore be removed from classes; a range crossing the boundary is capped at 0x7f.
/// Outside a class an above-ASCII literal becomes byte 0xff, which cannot occur in such a
/// subject. Raw `[` inside a JS character class is escaped for Rust's nested-class syntax.
fn project_fast_ascii_pattern(pattern: &str) -> Option<String> {
    fn hex4(bytes: &[u8]) -> Option<u32> {
        if bytes.len() < 4 || !bytes[..4].iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        std::str::from_utf8(&bytes[..4])
            .ok()
            .and_then(|digits| u32::from_str_radix(digits, 16).ok())
    }
    fn push_ascii_escape(out: &mut String, cp: u32) {
        use std::fmt::Write;
        let _ = write!(out, "\\x{:02x}", cp.min(0xff));
    }

    let bytes = pattern.as_bytes();
    let mut out = String::with_capacity(pattern.len());
    let mut in_class = false;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if bytes.get(index + 1) == Some(&b'u') => {
                let cp = hex4(bytes.get(index + 2..index + 6)?)?;
                let after = index + 6;
                if in_class && bytes.get(after) == Some(&b'-') {
                    // Consume a complete `\uXXXX-\uYYYY` range together so dropping a
                    // non-ASCII endpoint can never leave a dangling range operator.
                    if bytes.get(after + 1) != Some(&b'\\') || bytes.get(after + 2) != Some(&b'u') {
                        return None;
                    }
                    let end = hex4(bytes.get(after + 3..after + 7)?)?;
                    if cp <= 0x7f {
                        push_ascii_escape(&mut out, cp);
                        out.push('-');
                        push_ascii_escape(&mut out, end.min(0x7f));
                    }
                    index = after + 7;
                    continue;
                }
                if cp <= 0x7f {
                    push_ascii_escape(&mut out, cp);
                } else if !in_class {
                    out.push_str("\\xff");
                }
                index = after;
                continue;
            }
            b'\\' => {
                index += 1;
                let escaped = *bytes.get(index)?;
                // In legacy (non-u/v) ECMAScript regexps, punctuation that is not regexp
                // syntax may be identity-escaped. Rust's regexp parser intentionally rejects
                // several of those escapes. A slash is syntax only in a JS regexp *literal*
                // delimiter, not in the pattern supplied to RegExp, and a quote is never
                // regexp syntax, so both project to the same literal byte without the slash.
                // (u/v patterns never enter this ASCII tier.)
                if matches!(escaped, b'/' | b'"' | b'\'') {
                    out.push(escaped as char);
                } else {
                    out.push('\\');
                    out.push(escaped as char);
                }
            }
            b'[' if in_class => out.push_str("\\["),
            b'[' => {
                in_class = true;
                out.push('[');
            }
            b']' if in_class => {
                in_class = false;
                out.push(']');
            }
            byte => out.push(byte as char),
        }
        index += 1;
    }
    Some(out)
}

/// Compile the syntax shared exactly enough by ECMAScript and `regex`'s byte automaton.
///
/// This deliberately rejects constructs whose capture or assertion semantics need our full
/// backtracker. Compilation itself is the final syntax filter: harmless JS identity escapes that
/// the byte engine doesn't accept simply fall back as well.
fn compile_fast_ascii(pattern: &str, flags: &str) -> Option<regex::bytes::Regex> {
    if !pattern.is_ascii() || flags.contains('u') || flags.contains('v') {
        return None;
    }

    let bytes = pattern.as_bytes();
    // Rust's parser treats the legacy-JS empty class differently. Its capture state after a
    // repeated group also does not implement ECMAScript's per-iteration clearing rule. Repeating
    // a group with no capture nested inside it is safe: the group's own capture is overwritten
    // on every successful iteration. A nested capture could instead retain a prior iteration's
    // value, so only that structural case stays on the complete matcher.
    {
        let mut escaped = false;
        let mut class_start = None;
        for (index, byte) in bytes.iter().copied().enumerate() {
            if escaped {
                escaped = false;
                continue;
            }
            match byte {
                b'\\' => escaped = true,
                b'[' if class_start.is_none() => class_start = Some(index),
                b']' => {
                    if let Some(start) = class_start.take() {
                        // Legacy `[]` is never-match and `[^]` is match-any; Rust parses these
                        // differently. An escaped or raw `[` inside a non-empty JS class is not
                        // another class opener and must not trigger this check.
                        let inside = &bytes[start + 1..index];
                        if inside.is_empty() || inside == b"^" {
                            return None;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut groups: Vec<bool> = Vec::new();
    let mut escaped = false;
    let mut in_class = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' => escaped = true,
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'(' if !in_class => {
                let capturing = bytes.get(index + 1) != Some(&b'?');
                if capturing {
                    groups.iter_mut().for_each(|nested| *nested = true);
                }
                groups.push(false);
            }
            b')' if !in_class => {
                let has_inner_capture = groups.pop().unwrap_or(false);
                if has_inner_capture && matches!(bytes.get(index + 1), Some(b'*' | b'+' | b'{')) {
                    return None;
                }
            }
            _ => {}
        }
    }
    let mut escaped = false;
    let mut in_class = false;
    for (i, &b) in bytes.iter().enumerate() {
        if escaped {
            // Backreferences, legacy octal escapes and named backreferences require the complete
            // ECMAScript matcher. Ordinary character/class escapes are shared by both engines.
            if b.is_ascii_digit() || b == b'k' || b == b'c' {
                return None;
            }
            escaped = false;
            continue;
        }
        match b {
            b'\\' => escaped = true,
            b'[' => in_class = true,
            b']' => in_class = false,
            b'(' if !in_class && bytes.get(i + 1) == Some(&b'?') => {
                // Non-capturing groups are shared. Lookarounds, named groups and inline flag
                // groups are not admitted here.
                if bytes.get(i + 2) != Some(&b':') {
                    return None;
                }
            }
            _ => {}
        }
    }
    if escaped {
        return None;
    }

    let projected = project_fast_ascii_pattern(pattern)?;
    let mut builder = regex::bytes::RegexBuilder::new(&projected);
    builder
        .unicode(false)
        .case_insensitive(flags.contains('i'))
        .multi_line(flags.contains('m'))
        .dot_matches_new_line(flags.contains('s'));
    builder.build().ok()
}

/// Prove that every backreference is preceded on all paths by its capture and that repeated
/// bodies contain no captures (where host and ECMAScript per-iteration clearing can differ).
/// The proof is deliberately narrow; rejection only selects the complete matcher.
fn fancy_backrefs_safe(
    node: &Node,
    guaranteed: &mut std::collections::BTreeSet<usize>,
    saw_backref: &mut bool,
) -> bool {
    fn contains_capture(node: &Node) -> bool {
        match node {
            Node::Group(Some(_), _) => true,
            Node::Concat(nodes) | Node::Alt(nodes) => nodes.iter().any(contains_capture),
            Node::Group(None, inner)
            | Node::Repeat(inner, ..)
            | Node::Look(_, inner)
            | Node::LookBehind(_, inner)
            | Node::Modifier { inner, .. } => contains_capture(inner),
            _ => false,
        }
    }

    match node {
        Node::Concat(nodes) => nodes
            .iter()
            .all(|node| fancy_backrefs_safe(node, guaranteed, saw_backref)),
        Node::Alt(nodes) => {
            let incoming = guaranteed.clone();
            let mut intersection: Option<std::collections::BTreeSet<usize>> = None;
            for node in nodes {
                let mut branch = incoming.clone();
                if !fancy_backrefs_safe(node, &mut branch, saw_backref) {
                    return false;
                }
                intersection = Some(match intersection {
                    None => branch,
                    Some(current) => current.intersection(&branch).copied().collect(),
                });
            }
            *guaranteed = intersection.unwrap_or(incoming);
            true
        }
        Node::Group(index, inner) => {
            if !fancy_backrefs_safe(inner, guaranteed, saw_backref) {
                return false;
            }
            if let Some(index) = index {
                guaranteed.insert(*index);
            }
            true
        }
        Node::Repeat(inner, min, ..) => {
            if contains_capture(inner) {
                return false;
            }
            let mut after = guaranteed.clone();
            if !fancy_backrefs_safe(inner, &mut after, saw_backref) {
                return false;
            }
            if *min > 0 {
                *guaranteed = after;
            }
            true
        }
        Node::Backref(group) => {
            *saw_backref = true;
            *group != 0 && guaranteed.contains(group)
        }
        Node::Look(_, inner) => {
            if contains_capture(inner) {
                return false;
            }
            *saw_backref = true; // also enables this tier for capture-free lookahead
            let mut inside = guaranteed.clone();
            fancy_backrefs_safe(inner, &mut inside, saw_backref)
        }
        // Duplicate-name alternatives, lookbehind, inline flags, and JS whitespace classes
        // have host-dialect edge cases outside this accelerator's contract.
        Node::BackrefAlt(_)
        | Node::NamedBackref(_)
        | Node::LookBehind(..)
        | Node::Modifier { .. } => false,
        Node::Class(class) => !class.builtins.iter().any(|c| matches!(c, 's' | 'S')),
        _ => true,
    }
}

fn compile_fancy_ascii(pattern: &str, flags: &str, ast: &Node) -> Option<fancy_regex::Regex> {
    if !pattern.is_ascii() || flags.contains('u') || flags.contains('v') {
        return None;
    }
    let mut guaranteed = std::collections::BTreeSet::new();
    let mut saw_backref = false;
    if !fancy_backrefs_safe(ast, &mut guaranteed, &mut saw_backref) || !saw_backref {
        return None;
    }
    let projected = project_fast_ascii_pattern(pattern)?;
    let mut modifiers = String::new();
    if flags.contains('i') {
        modifiers.push('i');
    }
    if flags.contains('m') {
        modifiers.push('m');
    }
    if flags.contains('s') {
        modifiers.push('s');
    }
    let pattern = format!("(?{modifiers}:{projected})");
    fancy_regex::Regex::new(&pattern).ok()
}

fn compile_fast_wide(flags: &str, ast: &Node) -> Option<regex::Regex> {
    if flags.contains('u') || flags.contains('v') || flags.contains('i') {
        return None;
    }
    fn hex(out: &mut String, cp: u32) {
        use std::fmt::Write;
        let _ = write!(out, "\\x{{{cp:x}}}");
    }
    fn class(out: &mut String, cc: &CharClass) -> Option<()> {
        if !cc.props.is_empty() || cc.builtins.iter().any(char::is_ascii_uppercase) {
            return None;
        }
        out.push('[');
        if cc.negate {
            out.push('^');
        }
        for &(lo, hi) in &cc.ranges {
            hex(out, lo);
            if hi != lo {
                out.push('-');
                hex(out, hi);
            }
        }
        for builtin in &cc.builtins {
            match builtin {
                'd' => out.push_str("0-9"),
                'w' => out.push_str("A-Za-z0-9_"),
                's' => {
                    for cp in [
                        0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x20, 0xA0, 0x1680, 0x2000, 0x2001, 0x2002,
                        0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200A, 0x2028,
                        0x2029, 0x202F, 0x205F, 0x3000, 0xFEFF,
                    ] {
                        hex(out, cp);
                    }
                }
                _ => return None,
            }
        }
        out.push(']');
        Some(())
    }
    fn node(out: &mut String, n: &Node) -> Option<()> {
        match n {
            Node::Empty => out.push_str("(?:)"),
            Node::Char(cp) => hex(out, *cp),
            Node::Any => out.push('.'),
            Node::Class(cc) => class(out, cc)?,
            Node::Concat(nodes) => {
                for child in nodes {
                    node(out, child)?;
                }
            }
            Node::Alt(nodes) => {
                out.push_str("(?:");
                for (index, child) in nodes.iter().enumerate() {
                    if index != 0 {
                        out.push('|');
                    }
                    node(out, child)?;
                }
                out.push(')');
            }
            Node::Group(index, inner) => {
                out.push_str(if index.is_some() { "(" } else { "(?:" });
                node(out, inner)?;
                out.push(')');
            }
            Node::Repeat(inner, min, max, greedy) => {
                out.push_str("(?:");
                node(out, inner)?;
                out.push_str("){");
                out.push_str(&min.to_string());
                match max {
                    Some(max) if max == min => {}
                    Some(max) => {
                        out.push(',');
                        out.push_str(&max.to_string());
                    }
                    None => out.push(','),
                }
                out.push('}');
                if !greedy {
                    out.push('?');
                }
            }
            Node::Start => out.push('^'),
            Node::End => out.push('$'),
            // Word boundaries need the legacy ASCII word predicate, while these constructs
            // require the complete ECMAScript matcher or the fancy ASCII tier.
            Node::WordB(_)
            | Node::Backref(_)
            | Node::NamedBackref(_)
            | Node::BackrefAlt(_)
            | Node::Look(..)
            | Node::LookBehind(..)
            | Node::Modifier { .. } => return None,
        }
        Some(())
    }

    let mut pattern = String::new();
    node(&mut pattern, ast)?;
    let mut builder = regex::RegexBuilder::new(&pattern);
    builder
        .multi_line(flags.contains('m'))
        .dot_matches_new_line(flags.contains('s'));
    builder.build().ok()
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
        let elems = pattern_elements(unicode, pattern);
        let total_groups = count_capture_groups(&elems);
        let mut p = Parser {
            chars: elems,
            pos: 0,
            total_groups,
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
        let fast_fancy_ascii = compile_fancy_ascii(pattern, flags, &ast);
        let fast_wide = compile_fast_wide(flags, &ast);
        let fast_wide_locs = fast_wide
            .as_ref()
            .map(|fast| std::cell::RefCell::new(fast.capture_locations()));
        // Wrap the whole match in group-0 saves.
        let mut prog = vec![Inst::Save(0)];
        let mut nmarks = 0usize;
        compile(&ast, &mut prog, &mut nmarks)?;
        prog.push(Inst::Save(1));
        prog.push(Inst::Match);
        // The `flags` accessor returns flags in canonical order.
        let canonical: String = "dgimsuvy".chars().filter(|c| flags.contains(*c)).collect();
        let first = first_filter(&prog, flags.contains('m'));
        let fast_ascii = compile_fast_ascii(pattern, flags);
        let fast_ascii_locs = fast_ascii
            .as_ref()
            .map(|fast| std::cell::RefCell::new(fast.capture_locations()));
        let mut re = Regex {
            fast_ascii,
            fast_ascii_locs,
            fast_fancy_ascii,
            fast_wide,
            fast_wide_locs,
            unicode,
            nmarks,
            first,
            first_lut: None,
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
        };
        let lut = if let FirstFilter::Atoms(atoms) = &re.first {
            let mut lut = Box::new([false; 256]);
            for (c, slot) in lut.iter_mut().enumerate() {
                *slot = re.first_matches(atoms, c as u32);
            }
            Some(lut)
        } else {
            None
        };
        re.first_lut = lut;
        Ok(re)
    }

    /// Whether a match could begin with element `c` — the [`FirstFilter::Atoms`] predicate.
    /// Deliberately a superset of what the matcher accepts (e.g. `Any` only excludes `\n`), so a
    /// pass costs a wasted attempt at worst; only a reject skips work.
    fn first_matches(&self, atoms: &[Rep], c: u32) -> bool {
        let unicode = self.unicode || self.flags.contains('v');
        atoms.iter().any(|rep| match rep {
            Rep::Char(ch) => {
                *ch == c
                    || (self.ignore_case && {
                        match (char::from_u32(c), char::from_u32(*ch)) {
                            (Some(x), Some(y)) => {
                                if unicode {
                                    fold_canon(x as u32) == fold_canon(y as u32)
                                } else {
                                    canonicalize_legacy(x) == canonicalize_legacy(y)
                                }
                            }
                            _ => false,
                        }
                    })
            }
            Rep::Any => self.dotall || c != '\n' as u32,
            Rep::Class(cc) => cc.matches(c, self.ignore_case, unicode),
        })
    }

    /// Match a prepared subject and return shared capture spans.
    pub fn exec_text_shared(&self, text: &ReText, start: usize) -> Option<Captures> {
        match &text.ascii_src {
            Some(s) => {
                if let Some(fast) = &self.fast_ascii {
                    if start > s.len() {
                        None
                    } else {
                        let mut locs = self
                            .fast_ascii_locs
                            .as_ref()
                            .expect("ASCII matcher capture storage")
                            .borrow_mut();
                        let whole = fast.captures_read_at(&mut locs, s.as_bytes(), start)?;
                        if self.sticky && whole.start() != start {
                            None
                        } else {
                            let mut out = Vec::with_capacity(self.ngroups + 1);
                            for group in 0..=self.ngroups {
                                out.push(locs.get(group));
                            }
                            Some(Captures::from_vec(out))
                        }
                    }
                } else if let Some(fancy) = &self.fast_fancy_ascii {
                    match fancy.captures_from_pos(s, start) {
                        Ok(Some(caps)) => {
                            let whole = caps.get(0)?;
                            if self.sticky && whole.start() != start {
                                None
                            } else {
                                let mut out = Vec::with_capacity(self.ngroups + 1);
                                for group in 0..=self.ngroups {
                                    out.push(caps.get(group).map(|m| (m.start(), m.end())));
                                }
                                Some(Captures::from_vec(out))
                            }
                        }
                        Ok(None) => None,
                        // The complete engine retains the authoritative step/depth limits.
                        Err(_) => self.exec_impl(s.as_bytes(), start).map(Captures::from_vec),
                    }
                } else {
                    self.exec_impl(s.as_bytes(), start).map(Captures::from_vec)
                }
            }
            None => {
                if let (Some(subject), Some(fast)) = (&text.wide_src, &self.fast_wide) {
                    if start > text.len() {
                        return None;
                    }
                    let start_byte = text.wide_byte(start);
                    let mut locs = self
                        .fast_wide_locs
                        .as_ref()
                        .expect("wide matcher capture storage")
                        .borrow_mut();
                    let whole = fast.captures_read_at(&mut locs, subject, start_byte)?;
                    let whole_start = text.wide_element(whole.start())?;
                    if self.sticky && whole_start != start {
                        None
                    } else {
                        let mut out = Vec::with_capacity(self.ngroups + 1);
                        for group in 0..=self.ngroups {
                            out.push(locs.get(group).and_then(|(a, b)| {
                                Some((text.wide_element(a)?, text.wide_element(b)?))
                            }));
                        }
                        Some(Captures::from_vec(out))
                    }
                } else {
                    self.exec_impl(&text.elems[..], start)
                        .map(Captures::from_vec)
                }
            }
        }
    }

    /// Match for a caller that only observes matcher side effects. A pattern without capture
    /// groups needs just the whole-match span, so the ASCII tier can use `find_at` instead of
    /// allocating and populating the regex crate's capture-location buffer.
    pub fn exec_text_discard_shared(&self, text: &ReText, start: usize) -> Option<Captures> {
        if self.ngroups == 0 {
            if let (Some(subject), Some(fast)) = (&text.ascii_src, &self.fast_ascii) {
                if start > subject.len() {
                    return None;
                }
                let found = fast.find_at(subject.as_bytes(), start)?;
                if self.sticky && found.start() != start {
                    return None;
                }
                return Some(Captures::one((found.start(), found.end())));
            }
        }
        self.exec_text_shared(text, start)
    }

    /// Whole-match-only search for operations whose JavaScript result is dead. Capture groups
    /// can be recovered lazily if a legacy RegExp static is subsequently observed.
    pub(crate) fn find_text_shared(&self, text: &ReText, start: usize) -> Option<(usize, usize)> {
        if let (Some(subject), Some(fast)) = (&text.ascii_src, &self.fast_ascii) {
            if start > subject.len() {
                return None;
            }
            let found = fast.find_at(subject.as_bytes(), start)?;
            if self.sticky && found.start() != start {
                return None;
            }
            return Some((found.start(), found.end()));
        }
        self.exec_text_shared(text, start)
            .and_then(|captures| captures[0])
    }

    fn exec_impl<I: ReInput>(&self, input: I, start: usize) -> Option<Vec<Option<(usize, usize)>>> {
        if start > input.len() {
            return None;
        }
        // One matcher for the whole scan, its working buffers recycled across `exec` calls via a
        // thread-local (the engine is single-threaded per Interp).
        let mut scratch = MATCH_SCRATCH
            .with(|s| s.borrow_mut().take())
            .unwrap_or_default();
        scratch.caps.clear();
        scratch.caps.resize(2 * (self.ngroups + 1), None);
        scratch.marks.clear();
        scratch.marks.resize(self.nmarks, None);
        scratch.flags.clear();
        scratch
            .flags
            .push((self.ignore_case, self.multiline, self.dotall));
        let mut m = Matcher {
            input,
            caps: scratch.caps,
            marks: scratch.marks,
            steps: 0,
            depth: 0,
            back: false,
            flags: scratch.flags,
            unicode: self.flags.contains('u') || self.flags.contains('v'),
        };
        let mut from = start;
        let result = 'scan: loop {
            if from > input.len() {
                break 'scan None;
            }
            // Prescan: skip positions that cannot begin a match. Sticky regexes get exactly one
            // attempt at `start`, so the filter only ever saves that single attempt for them.
            if !self.sticky {
                match &self.first {
                    FirstFilter::Anchored => {
                        // `^` (non-multiline) can only match at position 0: one attempt at
                        // `from` decides the scan (any later position fails the assert too).
                        if from > 0 {
                            break 'scan None;
                        }
                    }
                    FirstFilter::Atoms(atoms) => {
                        // Every path consumes an element first: find the next viable one. Small
                        // elements go through the precomputed table (one load per position).
                        let len = input.len();
                        loop {
                            if from >= len {
                                break 'scan None;
                            }
                            let c = input.at(from);
                            let viable = match &self.first_lut {
                                Some(lut) if (c as usize) < 256 => lut[c as usize],
                                _ => self.first_matches(atoms, c),
                            };
                            if viable {
                                break;
                            }
                            from += 1;
                        }
                    }
                    FirstFilter::None => {}
                }
            }
            m.caps.fill(None);
            m.marks.fill(None);
            m.flags.truncate(1);
            m.steps = 0;
            m.depth = 0;
            if m.run(&self.prog, 0, from) {
                let mut out = Vec::with_capacity(self.ngroups + 1);
                for g in 0..=self.ngroups {
                    out.push(match (m.caps[2 * g], m.caps[2 * g + 1]) {
                        // A group inside a lookbehind captured right-to-left: normalize the span.
                        (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
                        _ => None,
                    });
                }
                break 'scan Some(out);
            }
            if self.sticky {
                break 'scan None;
            }
            from += 1;
        };
        MATCH_SCRATCH.with(|s| {
            *s.borrow_mut() = Some(MatchScratch {
                caps: m.caps,
                marks: m.marks,
                flags: m.flags,
            });
        });
        result
    }
}

/// Recycled matcher working buffers (see `Regex::exec_at`).
#[derive(Default)]
struct MatchScratch {
    caps: Vec<Option<usize>>,
    marks: Vec<Option<usize>>,
    flags: Vec<(bool, bool, bool)>,
}

thread_local! {
    static MATCH_SCRATCH: std::cell::RefCell<Option<MatchScratch>> =
        const { std::cell::RefCell::new(None) };
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
                    // Annex B: `\0` continues as a LegacyOctalEscapeSequence in a class.
                    let mut v = 0u32;
                    if !self.unicode {
                        for _ in 0..2 {
                            match self.peek() {
                                Some(d @ '0'..='7') => {
                                    v = v * 8 + d.to_digit(8).unwrap();
                                    self.bump();
                                }
                                _ => break,
                            }
                        }
                    }
                    Ok(ClassAtom::Char(v))
                }
                Some(c) if !self.unicode && c.is_ascii_digit() => {
                    // Annex B class octal escape; \8 and \9 are identity digits.
                    if c >= '8' {
                        return Ok(ClassAtom::Char(c as u32));
                    }
                    let mut v = c.to_digit(8).unwrap();
                    let max_more = if c <= '3' { 2 } else { 1 };
                    for _ in 0..max_more {
                        match self.peek() {
                            Some(d @ '0'..='7') => {
                                v = v * 8 + d.to_digit(8).unwrap();
                                self.bump();
                            }
                            _ => break,
                        }
                    }
                    Ok(ClassAtom::Char(v))
                }
                Some('b') => Ok(ClassAtom::Char(0x08)),
                Some('c') => match self.peek() {
                    Some(l) if l.is_ascii_alphabetic() => {
                        self.bump();
                        Ok(ClassAtom::Char((l as u8 % 32) as u32))
                    }
                    // Annex B ClassControlLetter also admits digits and '_'.
                    Some(l) if !self.unicode && (l.is_ascii_digit() || l == '_') => {
                        self.bump();
                        Ok(ClassAtom::Char((l as u8 % 32) as u32))
                    }
                    _ if self.unicode => Err("invalid \\c escape in unicode pattern".into()),
                    _ => {
                        self.pos -= 1; // un-consume the 'c': `\` is a literal backslash member
                        Ok(ClassAtom::Char('\\' as u32))
                    }
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
                // Annex B: `\0` continues as a LegacyOctalEscapeSequence (up to 2 more digits).
                let mut v = 0u32;
                if !self.unicode {
                    for _ in 0..2 {
                        match self.peek() {
                            Some(d @ '0'..='7') => {
                                v = v * 8 + d.to_digit(8).unwrap();
                                self.bump();
                            }
                            _ => break,
                        }
                    }
                }
                Ok(Node::Char(v))
            }
            Some('c') => {
                // `\cX` (a letter) is a control escape; otherwise Annex B treats the `\` as a
                // literal backslash and reparses the `c` as a plain character.
                match self.peek() {
                    Some(l) if l.is_ascii_alphabetic() => {
                        self.bump();
                        Ok(Node::Char((l as u8 % 32) as u32))
                    }
                    _ if self.unicode => Err("invalid \\c escape in unicode pattern".into()),
                    _ => {
                        self.pos -= 1; // un-consume the 'c'
                        Ok(Node::Char('\\' as u32))
                    }
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
                let start = self.pos;
                let mut num = c.to_digit(10).unwrap() as usize;
                while let Some(d) = self.peek() {
                    if d.is_ascii_digit() {
                        num = num.saturating_mul(10) + d.to_digit(10).unwrap() as usize;
                        self.bump();
                    } else {
                        break;
                    }
                }
                if self.unicode || (num >= 1 && num <= self.total_groups) {
                    return Ok(Node::Backref(num));
                }
                // Annex B: a decimal escape naming no capture group is a LegacyOctalEscapeSequence
                // (\8 and \9 are identity escapes); trailing digits reparse as literal atoms.
                self.pos = start;
                if c >= '8' {
                    return Ok(Node::Char(c as u32));
                }
                let mut v = c.to_digit(8).unwrap();
                let max_more = if c <= '3' { 2 } else { 1 };
                for _ in 0..max_more {
                    match self.peek() {
                        Some(d @ '0'..='7') => {
                            v = v * 8 + d.to_digit(8).unwrap();
                            self.bump();
                        }
                        _ => break,
                    }
                }
                Ok(Node::Char(v))
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

    /// A non-strict (Annex B) `\u` escape: exactly four hex digits or `{…}`, otherwise the
    /// whole escape is an IdentityEscape for `u` (consuming nothing).
    fn unicode_escape(&mut self) -> u32 {
        // Annex B (no `u` flag): `\u{` is an identity escape for `u` followed by a quantifier —
        // braced code-point escapes exist only in Unicode mode.
        let save = self.pos;
        let mut v: u32 = 0;
        for _ in 0..4 {
            match self.peek().and_then(|c| c.to_digit(16)) {
                Some(d) => {
                    v = v * 16 + d;
                    self.bump();
                }
                None => {
                    self.pos = save;
                    return 'u' as u32;
                }
            }
        }
        v
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

/// The matcher's view of a subject: element `i` as a code point / code unit. Monomorphized for
/// bytes (an ASCII subject — the common case, matched with no `Vec<u32>` materialization at all)
/// and for wide elements (anything non-ASCII).
pub trait ReInput: Copy {
    fn len(&self) -> usize;
    fn at(&self, i: usize) -> u32;
}

impl ReInput for &[u8] {
    #[inline(always)]
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
    #[inline(always)]
    fn at(&self, i: usize) -> u32 {
        self[i] as u32
    }
}

impl ReInput for &[u32] {
    #[inline(always)]
    fn len(&self) -> usize {
        <[u32]>::len(self)
    }
    #[inline(always)]
    fn at(&self, i: usize) -> u32 {
        self[i]
    }
}

struct Matcher<I: ReInput> {
    input: I,
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

impl<I: ReInput> Matcher<I> {
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
                Some((self.input.at(pos - 1), pos - 1))
            } else {
                None
            }
        } else if pos < self.input.len() {
            Some((self.input.at(pos), pos + 1))
        } else {
            None
        }
    }

    fn rep_matches(&self, rep: &Rep, c: u32) -> bool {
        match rep {
            Rep::Char(ch) => self.eqc_uu(c, *ch),
            Rep::Any => self.dotall() || c != '\n' as u32,
            Rep::Class(cc) => cc.matches(c, self.icase(), self.unicode),
        }
    }

    /// Conservative viability test for the continuation at `pc` and `pos`.
    ///
    /// `Some(false)` proves that its first consuming instruction cannot match here; `Some(true)`
    /// is only a possible match, and `None` means stateful bytecode prevented a proof. Lazy
    /// quantifiers use this to skip impossible retry positions without changing match order.
    fn continuation_viable(
        &self,
        prog: &[Inst],
        mut pc: usize,
        pos: usize,
        mut budget: usize,
        mut tainted_groups: u128,
    ) -> Option<bool> {
        while budget > 0 {
            budget -= 1;
            match &prog[pc] {
                Inst::Char(expected) => {
                    return Some(
                        self.step(pos)
                            .is_some_and(|(found, _)| self.eqc_uu(found, *expected)),
                    );
                }
                Inst::Any => {
                    return Some(self.step(pos).is_some_and(|(found, _)| {
                        self.dotall() || !is_line_terminator_u32(found)
                    }));
                }
                Inst::Class(class) => {
                    return Some(self.step(pos).is_some_and(|(found, _)| {
                        class.matches(found, self.icase(), self.unicode)
                    }));
                }
                Inst::Many { rep, min, .. } => {
                    let matches_here = self
                        .step(pos)
                        .is_some_and(|(found, _)| self.rep_matches(rep, found));
                    if *min > 0 || matches_here {
                        return Some(matches_here);
                    }
                    pc += 1;
                }
                Inst::Backref(group) => {
                    let group = *group;
                    if group >= 128 || tainted_groups & (1u128 << group) != 0 {
                        return None;
                    }
                    if group == 0 || 2 * group + 1 >= self.caps.len() {
                        pc += 1;
                        continue;
                    }
                    match (self.caps[2 * group], self.caps[2 * group + 1]) {
                        (Some(a), Some(b)) if a != b => {
                            let first = self.input.at(a.min(b));
                            return Some(
                                self.step(pos)
                                    .is_some_and(|(found, _)| self.eqc_uu(found, first)),
                            );
                        }
                        _ => {
                            pc += 1; // an empty or unset backreference consumes nothing
                        }
                    }
                }
                Inst::BackrefAlt(groups) => {
                    if groups
                        .iter()
                        .any(|&group| group >= 128 || tainted_groups & (1u128 << group) != 0)
                    {
                        return None;
                    }
                    let captured = groups.iter().copied().find_map(|group| {
                        match (self.caps[2 * group], self.caps[2 * group + 1]) {
                            (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
                            _ => None,
                        }
                    });
                    match captured {
                        Some((a, b)) if a != b => {
                            let first = self.input.at(a);
                            return Some(
                                self.step(pos)
                                    .is_some_and(|(found, _)| self.eqc_uu(found, first)),
                            );
                        }
                        _ => pc += 1,
                    }
                }
                Inst::AssertStart => {
                    if pos != 0
                        && !(self.multiline() && is_line_terminator_u32(self.input.at(pos - 1)))
                    {
                        return Some(false);
                    }
                    pc += 1;
                }
                Inst::AssertEnd => {
                    if pos != self.input.len()
                        && !(self.multiline() && is_line_terminator_u32(self.input.at(pos)))
                    {
                        return Some(false);
                    }
                    pc += 1;
                }
                Inst::WordBoundary(want) => {
                    let before =
                        pos > 0 && is_word_ic(self.input.at(pos - 1), self.icase(), self.unicode);
                    let after = pos < self.input.len()
                        && is_word_ic(self.input.at(pos), self.icase(), self.unicode);
                    if (before != after) != *want {
                        return Some(false);
                    }
                    pc += 1;
                }
                Inst::Jmp(target) => pc = *target,
                Inst::Split(a, b) => {
                    let left = self.continuation_viable(prog, *a, pos, budget, tainted_groups);
                    let right = self.continuation_viable(prog, *b, pos, budget, tainted_groups);
                    return match (left, right) {
                        (Some(false), Some(false)) => Some(false),
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        _ => None,
                    };
                }
                Inst::Match => return Some(true),
                // Capture writes do not themselves consume input. Keep walking, but remember
                // which groups have changed so a following backreference never consults stale
                // state. This is especially valuable for `.*?\k` continuations: the compiler's
                // group-end Save no longer hides the backreference's first-character filter.
                Inst::Save(slot) => {
                    let group = *slot / 2;
                    if group >= 128 {
                        return None;
                    }
                    tainted_groups |= 1u128 << group;
                    pc += 1;
                }
                Inst::ClearCaps(lo, hi) => {
                    if *hi >= 128 {
                        return None;
                    }
                    for group in *lo..=*hi {
                        tainted_groups |= 1u128 << group;
                    }
                    pc += 1;
                }
                Inst::SetMark(_) => pc += 1,
                // These affect the next predicate or can branch on mutable state.
                Inst::Look { .. }
                | Inst::LookBehind { .. }
                | Inst::PushFlags(..)
                | Inst::PopFlags
                | Inst::CheckProgress(_) => return None,
            }
        }
        None
    }

    fn run(&mut self, prog: &[Inst], pc: usize, pos: usize) -> bool {
        if self.depth > MAX_MATCH_DEPTH {
            return false;
        }
        self.depth += 1;
        let r = self.run_inner(prog, pc, pos);
        self.depth -= 1;
        r
    }

    fn run_inner(&mut self, prog: &[Inst], mut pc: usize, mut pos: usize) -> bool {
        // Straight-line regexp bytecode is overwhelmingly common. Execute it iteratively and
        // reserve Rust recursion for genuine backtracking points and state that needs rollback.
        // Besides avoiding a host call per character, this keeps the semantic step budget exact.
        loop {
            self.steps += 1;
            if self.steps > STEP_LIMIT {
                return false;
            }
            match &prog[pc] {
                Inst::Match => return true,
                Inst::Char(c) => match self.step(pos) {
                    Some((e, next)) if self.eqc_uu(e, *c) => {
                        pc += 1;
                        pos = next;
                        continue;
                    }
                    _ => return false,
                },
                Inst::Any => match self.step(pos) {
                    Some((e, next)) if self.dotall() || !is_line_terminator_u32(e) => {
                        pc += 1;
                        pos = next;
                        continue;
                    }
                    _ => return false,
                },
                Inst::Class(cc) => match self.step(pos) {
                    Some((e, next)) if cc.matches(e, self.icase(), self.unicode) => {
                        pc += 1;
                        pos = next;
                        continue;
                    }
                    _ => return false,
                },
                Inst::Save(slot) => {
                    let slot = *slot;
                    let old = self.caps[slot];
                    self.caps[slot] = Some(pos);
                    return if self.run(prog, pc + 1, pos) {
                        true
                    } else {
                        self.caps[slot] = old;
                        false
                    };
                }
                Inst::Split(a, b) => {
                    let (a, b) = (*a, *b);
                    return self.run(prog, a, pos) || self.run(prog, b, pos);
                }
                Inst::SetMark(id) => {
                    let id = *id;
                    let old = self.marks[id];
                    self.marks[id] = Some(pos);
                    return if self.run(prog, pc + 1, pos) {
                        true
                    } else {
                        self.marks[id] = old;
                        false
                    };
                }
                Inst::CheckProgress(id) => {
                    if self.marks[*id] == Some(pos) {
                        return false;
                    } else {
                        pc += 1;
                        continue;
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
                    while avail < cap
                        && avail < room
                        && self.rep_matches(rep, self.input.at(idx(avail)))
                    {
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
                            let candidate = if self.back { pos - n } else { pos + n };
                            if self.continuation_viable(prog, pc + 1, candidate, 16, 0)
                                != Some(false)
                                && cont(self, n)
                            {
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
                    return if self.run(prog, pc + 1, pos) {
                        true
                    } else {
                        self.flags.pop();
                        false
                    };
                }
                Inst::PopFlags => {
                    let popped = self.flags.pop().unwrap();
                    return if self.run(prog, pc + 1, pos) {
                        true
                    } else {
                        self.flags.push(popped);
                        false
                    };
                }
                Inst::Jmp(t) => {
                    pc = *t;
                    continue;
                }
                Inst::AssertStart => {
                    let ok = pos == 0
                        || (self.multiline() && is_line_terminator_u32(self.input.at(pos - 1)));
                    if !ok {
                        return false;
                    }
                    pc += 1;
                    continue;
                }
                Inst::AssertEnd => {
                    let ok = pos == self.input.len()
                        || (self.multiline() && is_line_terminator_u32(self.input.at(pos)));
                    if !ok {
                        return false;
                    }
                    pc += 1;
                    continue;
                }
                Inst::WordBoundary(want) => {
                    let (icase, unicode) = (self.icase(), self.unicode);
                    let before = pos > 0 && is_word_ic(self.input.at(pos - 1), icase, unicode);
                    let after =
                        pos < self.input.len() && is_word_ic(self.input.at(pos), icase, unicode);
                    let boundary = before != after;
                    if boundary != *want {
                        return false;
                    }
                    pc += 1;
                    continue;
                }
                Inst::Backref(g) => {
                    let g = *g;
                    if g == 0 || 2 * g + 1 >= self.caps.len() {
                        pc += 1; // invalid group: matches empty
                        continue;
                    }
                    match (self.caps[2 * g], self.caps[2 * g + 1]) {
                        (Some(a), Some(b)) => {
                            let (a, b) = (a.min(b), a.max(b));
                            let n = b - a;
                            let start = if self.back {
                                if pos < n {
                                    return false;
                                }
                                pos - n
                            } else {
                                if pos + n > self.input.len() {
                                    return false;
                                }
                                pos
                            };
                            if !(0..n).all(|index| {
                                self.eqc_uu(self.input.at(start + index), self.input.at(a + index))
                            }) {
                                return false;
                            }
                            pos = if self.back { pos - n } else { pos + n };
                            pc += 1;
                            continue;
                        }
                        _ => {
                            pc += 1; // unset group matches empty
                            continue;
                        }
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
                        None => {
                            pc += 1; // no group captured: matches empty
                            continue;
                        }
                        Some(g) => {
                            let (a, b) = (self.caps[2 * g].unwrap(), self.caps[2 * g + 1].unwrap());
                            let (a, b) = (a.min(b), a.max(b));
                            let n = b - a;
                            let start = if self.back {
                                if pos < n {
                                    return false;
                                }
                                pos - n
                            } else {
                                if pos + n > self.input.len() {
                                    return false;
                                }
                                pos
                            };
                            if !(0..n).all(|index| {
                                self.eqc_uu(self.input.at(start + index), self.input.at(a + index))
                            }) {
                                return false;
                            }
                            pos = if self.back { pos - n } else { pos + n };
                            pc += 1;
                            continue;
                        }
                    }
                }
                Inst::ClearCaps(lo, hi) => {
                    let (lo, hi) = (*lo, *hi);
                    let saved: Vec<Option<usize>> = self.caps[2 * lo..2 * hi + 2].to_vec();
                    for slot in &mut self.caps[2 * lo..2 * hi + 2] {
                        *slot = None;
                    }
                    return if self.run(prog, pc + 1, pos) {
                        true
                    } else {
                        self.caps[2 * lo..2 * hi + 2].copy_from_slice(&saved);
                        false
                    };
                }
                Inst::Look { negate, prog: sub } => {
                    let negate = *negate;
                    let sub = sub.clone();
                    let saved = self.caps.clone();
                    // A nested lookahead always matches forward, even inside a lookbehind body.
                    let saved_back = std::mem::replace(&mut self.back, false);
                    let matched = self.run(&sub, 0, pos);
                    self.back = saved_back;
                    return if negate {
                        self.caps = saved;
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
                    };
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
                    return if negate {
                        self.caps = saved;
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
                    };
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

#[cfg(test)]
mod fast_ascii_diagnostics {
    #[test]
    fn escaped_open_bracket_is_not_an_empty_class() {
        for pattern in [r"\s*([+>~\s])\s*([a-zA-Z#.*:\[])", r"^[\s[]?shapgvba"] {
            assert!(
                super::compile_fast_ascii(pattern, "g").is_some(),
                "fast ASCII compilation rejected {pattern:?}"
            );
        }
    }

    #[test]
    fn legacy_identity_escaped_punctuation_uses_ascii_tier() {
        assert!(
            super::compile_fast_ascii(r#"(^|[^\\])\"\\\/Qngr\((-?[0-9]+)\)\\\/\""#, "g").is_some()
        );
    }

    #[test]
    fn guaranteed_ascii_backreference_uses_fancy_tier() {
        let re =
            super::Regex::new(r#"^(\[) *@?([\w-]+) *([!*$^~=]*) *('?"?)(.*?)\4 *\]"#, "").unwrap();
        assert!(
            re.fast_fancy_ascii.is_some(),
            "{:?}",
            fancy_regex::Regex::new(&format!(
                "(?:{})",
                super::project_fast_ascii_pattern(
                    r#"^(\[) *@?([\w-]+) *([!*$^~=]*) *('?"?)(.*?)\4 *\]"#
                )
                .unwrap()
            ))
        );
        let input = crate::lstr::LStr::from("[glcr=fhozvg]");
        let text = super::ReText::new_rc(false, &input);
        let caps = re.exec_text_shared(&text, 0).unwrap();
        assert_eq!(caps[0], Some((0, 13)));
    }

    #[test]
    fn capture_free_ascii_lookahead_uses_fancy_tier() {
        let re = super::Regex::new("HF(?=;)", "i").unwrap();
        assert!(re.fast_fancy_ascii.is_some());
        let input = crate::lstr::LStr::from("xhf;y");
        let text = super::ReText::new_rc(false, &input);
        assert_eq!(re.exec_text_shared(&text, 0).unwrap()[0], Some((1, 3)));
    }

    #[test]
    fn legacy_pattern_uses_wide_automaton_with_unit_offsets() {
        let re = super::Regex::new(r"Qngr\((-?[0-9]+)\)", "").unwrap();
        assert!(re.fast_wide.is_some());
        let input = crate::lstr::LStr::from("‰Qngr(-12)");
        let text = super::ReText::new_rc(false, &input);
        let caps = re.exec_text_shared(&text, 0).unwrap();
        assert_eq!(caps[0], Some((1, 10)));
        assert_eq!(caps[1], Some((6, 9)));
    }
}
