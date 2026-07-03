//! A minimal parser for the test262 YAML frontmatter block (`/*--- ... ---*/`). Hand-rolled (std
//! only) — it understands just the fields the runner needs: `negative`, `flags`, `includes`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Parse,
    Early,
    Resolution,
    Runtime,
}

#[derive(Debug)]
pub struct Negative {
    pub phase: Phase,
    pub error_type: String,
}

#[derive(Debug, Default)]
pub struct Frontmatter {
    pub negative: Option<Negative>,
    pub flags: Vec<String>,
    pub includes: Vec<String>,
}

impl Frontmatter {
    pub fn has_flag(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }

    pub fn parse(src: &str) -> Frontmatter {
        let mut fm = Frontmatter::default();
        let yaml = match extract(src) {
            Some(y) => y,
            None => return fm,
        };
        // A handful of tests use CR (alone) as their line terminator.
        let yaml = yaml.replace('\r', "\n");
        let lines: Vec<&str> = yaml.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();

            if let Some(rest) = trimmed.strip_prefix("flags:") {
                fm.flags = collect_list(rest, &lines, &mut i, indent);
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("includes:") {
                fm.includes = collect_list(rest, &lines, &mut i, indent);
                continue;
            }
            if trimmed.starts_with("negative:") {
                let (neg, next) = parse_negative(&lines, i, indent);
                fm.negative = neg;
                i = next;
                continue;
            }
            i += 1;
        }
        fm
    }
}

/// Extract the text between `/*---` and `---*/`.
fn extract(src: &str) -> Option<&str> {
    let start = src.find("/*---")? + 5;
    let end = src[start..].find("---*/")? + start;
    Some(&src[start..end])
}

/// Parse a `key: [a, b]` inline list, or a following block of `  - item` lines. Advances `i` past
/// what it consumes.
fn collect_list(rest: &str, lines: &[&str], i: &mut usize, key_indent: usize) -> Vec<String> {
    let rest = rest.trim();
    if rest.starts_with('[') {
        *i += 1;
        return rest
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    // Block list: subsequent lines more-indented than the key, each `- item`.
    *i += 1;
    let mut out = Vec::new();
    while *i < lines.len() {
        let line = lines[*i];
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if t.is_empty() {
            *i += 1;
            continue;
        }
        if indent <= key_indent {
            break;
        }
        if let Some(item) = t.strip_prefix('-') {
            out.push(
                item.trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string(),
            );
            *i += 1;
        } else {
            break;
        }
    }
    out
}

fn parse_negative(lines: &[&str], start: usize, key_indent: usize) -> (Option<Negative>, usize) {
    let mut phase = Phase::Runtime;
    let mut error_type = String::new();
    let mut i = start + 1;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if t.is_empty() {
            i += 1;
            continue;
        }
        if indent <= key_indent {
            break;
        }
        if let Some(v) = t.strip_prefix("phase:") {
            phase = match v.trim() {
                "parse" => Phase::Parse,
                "early" => Phase::Early,
                "resolution" => Phase::Resolution,
                _ => Phase::Runtime,
            };
        } else if let Some(v) = t.strip_prefix("type:") {
            error_type = v.trim().to_string();
        }
        i += 1;
    }
    if error_type.is_empty() {
        (None, i)
    } else {
        (Some(Negative { phase, error_type }), i)
    }
}
