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
    Char(char),
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
    Look { negate: bool, prog: Rc<Vec<Inst>> },
}

#[derive(Default)]
struct CharClass {
    negate: bool,
    ranges: Vec<(char, char)>,
    /// Builtin sub-classes by letter: 'd','w','s' (and uppercase negated forms expanded inline).
    builtins: Vec<char>,
}

impl CharClass {
    fn matches(&self, c: char, icase: bool) -> bool {
        let mut hit = self.matches_raw(c);
        if !hit && icase {
            // Try the opposite case for case-insensitive matching.
            for alt in c.to_lowercase().chain(c.to_uppercase()) {
                if alt != c && self.matches_raw(alt) {
                    hit = true;
                    break;
                }
            }
        }
        hit ^ self.negate
    }
    fn matches_raw(&self, c: char) -> bool {
        for &(lo, hi) in &self.ranges {
            if c >= lo && c <= hi {
                return true;
            }
        }
        for &b in &self.builtins {
            if builtin_matches(b, c) {
                return true;
            }
        }
        false
    }
}

fn builtin_matches(b: char, c: char) -> bool {
    match b {
        'd' => c.is_ascii_digit(),
        'D' => !c.is_ascii_digit(),
        'w' => c.is_ascii_alphanumeric() || c == '_',
        'W' => !(c.is_ascii_alphanumeric() || c == '_'),
        's' => c.is_whitespace(),
        'S' => !c.is_whitespace(),
        _ => false,
    }
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

// ---------------------------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------------------------

enum Node {
    Empty,
    Char(char),
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
    Look(bool, Box<Node>),
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    ngroups: usize,
    names: Vec<(String, usize)>,
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
        let mut p = Parser { chars: pattern.chars().collect(), pos: 0, ngroups: 0, names: Vec::new() };
        let ast = p.parse_alt()?;
        if p.pos != p.chars.len() {
            return Err("unexpected character in pattern".into());
        }
        // Wrap the whole match in group-0 saves.
        let mut prog = vec![Inst::Save(0)];
        compile(&ast, &mut prog)?;
        prog.push(Inst::Save(1));
        prog.push(Inst::Match);
        // The `flags` accessor returns flags in canonical order.
        let canonical: String = "dgimsuvy".chars().filter(|c| flags.contains(*c)).collect();
        Ok(Regex {
            prog,
            ngroups: p.ngroups,
            source: if pattern.is_empty() { "(?:)".into() } else { pattern.to_string() },
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
    pub fn exec_at(&self, input: &[char], start: usize) -> Option<Vec<Option<(usize, usize)>>> {
        let mut from = start;
        loop {
            if from > input.len() {
                return None;
            }
            let mut m = Matcher {
                re: self,
                input,
                caps: vec![None; 2 * (self.ngroups + 1)],
                steps: 0,
                depth: 0,
            };
            if m.run(&self.prog, 0, from) {
                let mut out = Vec::with_capacity(self.ngroups + 1);
                for g in 0..=self.ngroups {
                    out.push(match (m.caps[2 * g], m.caps[2 * g + 1]) {
                        (Some(a), Some(b)) => Some((a, b)),
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
            Some(c) => Ok(Node::Char(c)),
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
                        Some('=') | Some('!') => {
                            self.bump();
                            let inner = self.parse_alt()?;
                            self.expect(')')?;
                            Ok(Node::Group(None, Box::new(inner)))
                        }
                        _ => {
                            let mut name = String::new();
                            while let Some(c) = self.peek() {
                                self.bump();
                                if c == '>' {
                                    break;
                                }
                                name.push(c);
                            }
                            self.ngroups += 1;
                            let idx = self.ngroups;
                            if self.names.iter().any(|(n, _)| *n == name) {
                                return Err(format!("duplicate group name {name}"));
                            }
                            self.names.push((name, idx));
                            let inner = self.parse_alt()?;
                            self.expect(')')?;
                            Ok(Node::Group(Some(idx), Box::new(inner)))
                        }
                    }
                }
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

    fn parse_class(&mut self) -> Result<Node, String> {
        let mut cc = CharClass::default();
        if self.peek() == Some('^') {
            self.bump();
            cc.negate = true;
        }
        // A leading ']' is a literal.
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err("unterminated character class".into()),
                Some(']') if !first => {
                    self.bump();
                    break;
                }
                _ => {}
            }
            first = false;
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
                        push_class_atom(&mut cc, a);
                        cc.ranges.push(('-', '-'));
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
                Some('n') => Ok(ClassAtom::Char('\n')),
                Some('t') => Ok(ClassAtom::Char('\t')),
                Some('r') => Ok(ClassAtom::Char('\r')),
                Some('f') => Ok(ClassAtom::Char('\u{000C}')),
                Some('v') => Ok(ClassAtom::Char('\u{000B}')),
                Some('0') => Ok(ClassAtom::Char('\0')),
                Some('b') => Ok(ClassAtom::Char('\u{0008}')),
                Some('x') => Ok(ClassAtom::Char(self.hex(2))),
                Some('u') => Ok(ClassAtom::Char(self.unicode_escape())),
                Some(c) => Ok(ClassAtom::Char(c)),
            },
            Some(c) => Ok(ClassAtom::Char(c)),
        }
    }

    fn parse_escape(&mut self) -> Result<Node, String> {
        match self.bump() {
            None => Err("trailing backslash".into()),
            Some(c @ ('d' | 'D' | 'w' | 'W' | 's' | 'S')) => {
                Ok(Node::Class(CharClass { builtins: vec![c], ..Default::default() }))
            }
            Some('b') => Ok(Node::WordB(true)),
            Some('B') => Ok(Node::WordB(false)),
            Some('n') => Ok(Node::Char('\n')),
            Some('t') => Ok(Node::Char('\t')),
            Some('r') => Ok(Node::Char('\r')),
            Some('f') => Ok(Node::Char('\u{000C}')),
            Some('v') => Ok(Node::Char('\u{000B}')),
            Some('0') => Ok(Node::Char('\0')),
            Some('x') => Ok(Node::Char(self.hex(2))),
            Some('u') => Ok(Node::Char(self.unicode_escape())),
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
            Some(c) => Ok(Node::Char(c)),
        }
    }

    fn hex(&mut self, n: usize) -> char {
        let mut s = String::new();
        for _ in 0..n {
            if let Some(c) = self.peek() {
                if c.is_ascii_hexdigit() {
                    s.push(c);
                    self.bump();
                }
            }
        }
        u32::from_str_radix(&s, 16).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}')
    }

    fn unicode_escape(&mut self) -> char {
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
            u32::from_str_radix(&s, 16).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}')
        } else {
            self.hex(4)
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
    Char(char),
    Builtin(char),
}

fn push_class_atom(cc: &mut CharClass, a: ClassAtom) {
    match a {
        ClassAtom::Char(c) => cc.ranges.push((c, c)),
        ClassAtom::Builtin(b) => cc.builtins.push(b),
    }
}

// ---------------------------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------------------------

fn compile(node: &Node, prog: &mut Vec<Inst>) -> Result<(), String> {
    match node {
        Node::Empty => {}
        Node::Char(c) => prog.push(Inst::Char(*c)),
        Node::Any => prog.push(Inst::Any),
        Node::Class(cc) => prog.push(Inst::Class(Rc::new(clone_class(cc)))),
        Node::Start => prog.push(Inst::AssertStart),
        Node::End => prog.push(Inst::AssertEnd),
        Node::WordB(b) => prog.push(Inst::WordBoundary(*b)),
        Node::Backref(n) => prog.push(Inst::Backref(*n)),
        Node::Concat(v) => {
            for n in v {
                compile(n, prog)?;
            }
        }
        Node::Alt(v) => {
            let mut jmp_ends = Vec::new();
            for (i, alt) in v.iter().enumerate() {
                if i < v.len() - 1 {
                    let sp = prog.len();
                    prog.push(Inst::Split(0, 0));
                    let a_start = prog.len();
                    compile(alt, prog)?;
                    jmp_ends.push(prog.len());
                    prog.push(Inst::Jmp(0));
                    let next = prog.len();
                    prog[sp] = Inst::Split(a_start, next);
                } else {
                    compile(alt, prog)?;
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
            compile(inner, prog)?;
            if let Some(i) = idx {
                prog.push(Inst::Save(2 * i + 1));
            }
        }
        Node::Look(negate, inner) => {
            let mut sub = Vec::new();
            compile(inner, &mut sub)?;
            sub.push(Inst::Match);
            prog.push(Inst::Look { negate: *negate, prog: Rc::new(sub) });
        }
        Node::Repeat(inner, min, max, greedy) => compile_repeat(inner, *min, *max, *greedy, prog)?,
    }
    Ok(())
}

fn compile_repeat(
    inner: &Node,
    min: usize,
    max: Option<usize>,
    greedy: bool,
    prog: &mut Vec<Inst>,
) -> Result<(), String> {
    if min > MAX_REPEAT || max.map(|m| m > MAX_REPEAT).unwrap_or(false) {
        return Err("repetition count too large".into());
    }
    for _ in 0..min {
        compile(inner, prog)?;
    }
    match max {
        None => {
            // Greedy: L1: Split(body, end); body; Jmp(L1); end.
            let l1 = prog.len();
            let sp = prog.len();
            prog.push(Inst::Split(0, 0));
            let body = prog.len();
            compile(inner, prog)?;
            prog.push(Inst::Jmp(l1));
            let end = prog.len();
            prog[sp] = if greedy { Inst::Split(body, end) } else { Inst::Split(end, body) };
        }
        Some(m) => {
            let extra = m.saturating_sub(min);
            let mut splits = Vec::new();
            for _ in 0..extra {
                let sp = prog.len();
                prog.push(Inst::Split(0, 0));
                let body = prog.len();
                splits.push((sp, body));
                compile(inner, prog)?;
            }
            let end = prog.len();
            for (sp, body) in splits {
                prog[sp] = if greedy { Inst::Split(body, end) } else { Inst::Split(end, body) };
            }
        }
    }
    Ok(())
}

fn clone_class(cc: &CharClass) -> CharClass {
    CharClass { negate: cc.negate, ranges: cc.ranges.clone(), builtins: cc.builtins.clone() }
}

// ---------------------------------------------------------------------------------------------
// Backtracking matcher
// ---------------------------------------------------------------------------------------------

/// Recursion-depth ceiling for the backtracking matcher (separate from the step budget): a long
/// input against a greedy quantifier recurses once per consumed char, which would overflow the
/// native stack on big inputs.
const MAX_MATCH_DEPTH: u32 = 3000;

struct Matcher<'a> {
    re: &'a Regex,
    input: &'a [char],
    caps: Vec<Option<usize>>,
    steps: u64,
    depth: u32,
}

impl Matcher<'_> {
    fn eqc(&self, a: char, b: char) -> bool {
        if a == b {
            return true;
        }
        if self.re.ignore_case {
            return a.to_lowercase().eq(b.to_lowercase());
        }
        false
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
            Inst::Char(c) => {
                if pos < self.input.len() && self.eqc(self.input[pos], *c) {
                    self.run(prog, pc + 1, pos + 1)
                } else {
                    false
                }
            }
            Inst::Any => {
                if pos < self.input.len() && (self.re.dotall || self.input[pos] != '\n') {
                    self.run(prog, pc + 1, pos + 1)
                } else {
                    false
                }
            }
            Inst::Class(cc) => {
                if pos < self.input.len() && cc.matches(self.input[pos], self.re.ignore_case) {
                    self.run(prog, pc + 1, pos + 1)
                } else {
                    false
                }
            }
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
            Inst::Jmp(t) => self.run(prog, *t, pos),
            Inst::AssertStart => {
                let ok = pos == 0 || (self.re.multiline && self.input[pos - 1] == '\n');
                ok && self.run(prog, pc + 1, pos)
            }
            Inst::AssertEnd => {
                let ok = pos == self.input.len() || (self.re.multiline && self.input[pos] == '\n');
                ok && self.run(prog, pc + 1, pos)
            }
            Inst::WordBoundary(want) => {
                let before = pos > 0 && is_word(self.input[pos - 1]);
                let after = pos < self.input.len() && is_word(self.input[pos]);
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
                        let text: Vec<char> = self.input[a..b].to_vec();
                        if pos + text.len() <= self.input.len()
                            && (0..text.len()).all(|i| self.eqc(self.input[pos + i], text[i]))
                        {
                            self.run(prog, pc + 1, pos + text.len())
                        } else {
                            false
                        }
                    }
                    _ => self.run(prog, pc + 1, pos), // unset group matches empty
                }
            }
            Inst::Look { negate, prog: sub } => {
                let negate = *negate;
                let sub = sub.clone();
                let saved = self.caps.clone();
                let matched = self.run(&sub, 0, pos);
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
        }
    }
}
