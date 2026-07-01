//! BCP-47 / Unicode BCP-47 (`-u-`/`-t-`) language-tag parsing and canonicalization. The CLDR alias
//! tables live in [`aliases`]; the algorithm here (structural parse → case → alias replacement →
//! variant/extension ordering → keyword canonicalization) is complete.

mod aliases;

/// Whether the tag matches the Unicode locale grammar, or is one of the grammar-valid grandfathered
/// tags (IsStructurallyValidLanguageTag).
pub fn is_structurally_valid_tag(tag: &str) -> bool {
    aliases::grandfathered(&tag.to_ascii_lowercase()).is_some() || parse(tag).is_some()
}

/// Canonicalize a `ca` (calendar) keyword type, applying the CLDR type alias if any.
pub fn canonical_ca(ty: &str) -> Option<String> {
    aliases::unicode_type_alias("ca", ty).map(|s| s.to_string())
}

fn is_alpha(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic())
}
fn is_digit(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}
fn is_alnum(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn is_language(s: &str) -> bool {
    // unicode_language_subtag = alpha{2,3} | alpha{5,8}  (no 4-alpha, no extlang form).
    ((s.len() >= 2 && s.len() <= 3) || (s.len() >= 5 && s.len() <= 8)) && is_alpha(s)
}
fn is_script(s: &str) -> bool {
    s.len() == 4 && is_alpha(s)
}
fn is_region(s: &str) -> bool {
    (s.len() == 2 && is_alpha(s)) || (s.len() == 3 && is_digit(s))
}
fn is_variant(s: &str) -> bool {
    (s.len() >= 5 && s.len() <= 8 && is_alnum(s))
        || (s.len() == 4 && s.as_bytes()[0].is_ascii_digit() && is_alnum(s))
}
fn is_singleton(s: &str) -> bool {
    s.len() == 1 && is_alnum(s) && !s.eq_ignore_ascii_case("x")
}

/// A parsed unicode language identifier + extensions.
#[derive(Default, Clone)]
pub struct LangTag {
    pub language: String,
    pub extlangs: Vec<String>,
    pub script: String,
    pub region: String,
    pub variants: Vec<String>,
    /// Non-`u`/`t`/`x` extensions: (singleton, subtags).
    pub other_ext: Vec<(char, Vec<String>)>,
    /// The `-t-` (transform) extension: an optional tlang plus sorted key/value fields.
    pub transform: Option<(Option<Box<LangTag>>, Vec<(String, Vec<String>)>)>,
    /// The `-u-` (unicode) extension: attributes plus key/value keywords.
    pub unicode: Option<(Vec<String>, Vec<(String, Vec<String>)>)>,
    pub private: Vec<String>,
}

/// Parse a tag into its structure, or `None` if it is not structurally valid.
pub fn parse(tag: &str) -> Option<LangTag> {
    if tag.is_empty() || tag.len() > 255 {
        return None;
    }
    let parts: Vec<&str> = tag.split('-').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let mut idx = 0;
    let mut t = LangTag::default();

    // A private-use-only tag ("x-...") has no language subtag, so it is not a valid Unicode locale
    // identifier and must be rejected (RangeError), even though BCP-47 permits it.

    // language
    if idx >= parts.len() || !is_language(parts[idx]) {
        return None;
    }
    t.language = parts[idx].to_string();
    idx += 1;
    // (ECMA-402's Unicode grammar has no extlang subtags.)
    // script
    if idx < parts.len() && is_script(parts[idx]) {
        t.script = parts[idx].to_string();
        idx += 1;
    }
    // region
    if idx < parts.len() && is_region(parts[idx]) {
        t.region = parts[idx].to_string();
        idx += 1;
    }
    // variants (must be unique)
    while idx < parts.len() && is_variant(parts[idx]) {
        let v = parts[idx].to_lowercase();
        if t.variants.iter().any(|e| e.eq_ignore_ascii_case(&v)) {
            return None;
        }
        t.variants.push(v);
        idx += 1;
    }
    // extensions + private use (singletons must be unique)
    let mut seen_singletons: Vec<char> = Vec::new();
    while idx < parts.len() {
        let s = parts[idx];
        if s.eq_ignore_ascii_case("x") {
            let p = parse_privateuse(&parts, idx)?;
            t.private = p;
            return Some(t);
        }
        if !is_singleton(s) {
            return None;
        }
        let singleton = s.to_ascii_lowercase().chars().next().unwrap();
        if seen_singletons.contains(&singleton) {
            return None;
        }
        seen_singletons.push(singleton);
        idx += 1;
        // collect subtags until the next singleton / x / end
        let start = idx;
        let mut subs: Vec<&str> = Vec::new();
        while idx < parts.len() && parts[idx].len() >= 2 {
            subs.push(parts[idx]);
            idx += 1;
        }
        if subs.is_empty() {
            return None; // an extension must have at least one subtag
        }
        // subtag length rules: u/t/other allow 2-8 alnum
        if !subs.iter().all(|p| p.len() >= 2 && p.len() <= 8 && is_alnum(p)) {
            return None;
        }
        let _ = start;
        match singleton {
            'u' => t.unicode = Some(parse_unicode(&subs)?),
            't' => t.transform = Some(parse_transform(&subs)?),
            _ => t
                .other_ext
                .push((singleton, subs.iter().map(|s| s.to_lowercase()).collect())),
        }
    }
    Some(t)
}

fn parse_privateuse(parts: &[&str], start: usize) -> Option<Vec<String>> {
    // "x" then 1..* subtags of 1..8 alnum.
    let mut out = vec!["x".to_string()];
    let mut idx = start + 1;
    if idx >= parts.len() {
        return None;
    }
    while idx < parts.len() {
        let p = parts[idx];
        if p.is_empty() || p.len() > 8 || !is_alnum(p) {
            return None;
        }
        out.push(p.to_lowercase());
        idx += 1;
    }
    Some(out)
}

/// Parse the `-u-` extension body into (attributes, keywords). Keywords are (key, [type…]).
fn parse_unicode(subs: &[&str]) -> Option<(Vec<String>, Vec<(String, Vec<String>)>)> {
    let mut attributes: Vec<String> = Vec::new();
    let mut keywords: Vec<(String, Vec<String>)> = Vec::new();
    let mut i = 0;
    // leading attributes (length != 2)
    while i < subs.len() && subs[i].len() != 2 {
        attributes.push(subs[i].to_lowercase());
        i += 1;
    }
    while i < subs.len() {
        // key = alphanum alpha (exactly 2 chars, second must be a letter).
        if subs[i].len() != 2 || !subs[i].as_bytes()[1].is_ascii_alphabetic() {
            return None;
        }
        let key = subs[i].to_lowercase();
        i += 1;
        let mut types: Vec<String> = Vec::new();
        while i < subs.len() && subs[i].len() != 2 {
            types.push(subs[i].to_lowercase());
            i += 1;
        }
        keywords.push((key, types));
    }
    Some((attributes, keywords))
}

/// Parse the `-t-` extension body into (optional tlang, fields). Fields are (key, [value…]).
fn parse_transform(
    subs: &[&str],
) -> Option<(Option<Box<LangTag>>, Vec<(String, Vec<String>)>)> {
    let mut i = 0;
    // optional tlang: starts with a language subtag (2-3 or 5-8 alpha) — not a tfield key (2 alnum
    // with a digit second char).
    let mut tlang: Option<Box<LangTag>> = None;
    if i < subs.len() && is_language(subs[i]) {
        // collect the language-identifier portion (until a tfield key: exactly 2 chars, second a digit)
        let mut lang_parts: Vec<&str> = Vec::new();
        while i < subs.len() {
            let s = subs[i];
            if s.len() == 2 && s.as_bytes()[1].is_ascii_digit() {
                break; // start of tfields
            }
            lang_parts.push(s);
            i += 1;
        }
        let joined = lang_parts.join("-");
        tlang = Some(Box::new(parse(&joined)?));
    }
    let mut fields: Vec<(String, Vec<String>)> = Vec::new();
    while i < subs.len() {
        // tfield key: 2 chars, second is a digit
        let key = subs[i];
        if key.len() != 2 || !key.as_bytes()[1].is_ascii_digit() {
            return None;
        }
        let key = key.to_lowercase();
        i += 1;
        let mut vals: Vec<String> = Vec::new();
        while i < subs.len() {
            let s = subs[i];
            if s.len() == 2 && s.as_bytes()[1].is_ascii_digit() {
                break;
            }
            if s.len() < 3 || s.len() > 8 {
                return None;
            }
            vals.push(s.to_lowercase());
            i += 1;
        }
        if vals.is_empty() {
            return None;
        }
        fields.push((key, vals));
    }
    Some((tlang, fields))
}

fn titlecase(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i == 0 {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// CanonicalizeUnicodeLocaleId: parse, apply case + alias replacement + ordering, and render.
pub fn canonicalize_language_tag(tag: &str) -> Option<String> {
    // A grammar-valid grandfathered/redundant tag canonicalizes wholesale first.
    if let Some(repl) = aliases::grandfathered(&tag.to_ascii_lowercase()) {
        return canonicalize_language_tag(repl);
    }
    let mut t = parse(tag)?;
    canonicalize_struct(&mut t);
    Some(render(&t))
}

fn canonicalize_struct(t: &mut LangTag) {
    // Private-use-only tag.
    if t.language.is_empty() && !t.private.is_empty() {
        return;
    }
    // Case normalization.
    t.language = t.language.to_lowercase();
    t.script = titlecase(&t.script);
    t.region = t.region.to_uppercase();
    for e in &mut t.extlangs {
        *e = e.to_lowercase();
    }
    for v in &mut t.variants {
        *v = v.to_lowercase();
    }

    // Language alias (may expand to language[-script][-region][-variant…]).
    apply_language_alias(t);
    // Region alias (simple + territory-split handled minimally).
    if let Some(r) = aliases::region_alias(&t.region) {
        t.region = r.to_string();
    }
    // Script alias.
    if let Some(s) = aliases::script_alias(&t.script) {
        t.script = s.to_string();
    }
    // Variant aliases.
    for v in &mut t.variants {
        if let Some(rep) = aliases::variant_alias(v) {
            *v = rep.to_string();
        }
    }
    t.variants.sort();
    t.variants.dedup();

    // Extension canonicalization.
    if let Some((attrs, keywords)) = &mut t.unicode {
        canonicalize_unicode(attrs, keywords);
    }
    if let Some((tlang, fields)) = &mut t.transform {
        if let Some(l) = tlang {
            canonicalize_struct(l);
        }
        for (_k, v) in fields.iter_mut() {
            for e in v.iter_mut() {
                *e = e.to_lowercase();
            }
        }
        fields.sort_by(|a, b| a.0.cmp(&b.0));
    }
    for (_s, subs) in &mut t.other_ext {
        for e in subs.iter_mut() {
            *e = e.to_lowercase();
        }
    }
    t.other_ext.sort_by_key(|(s, _)| *s);
}

fn apply_language_alias(t: &mut LangTag) {
    // CLDR language aliases can key on language, language-region, language-script, or
    // language-script-region; the matched subtags are *consumed* by the replacement. Try the most
    // specific key first.
    let lang = t.language.clone();
    // (key, consumes_script, consumes_region)
    let candidates: [(String, bool, bool); 4] = [
        (format!("{}-{}-{}", lang, t.script, t.region), true, true),
        (format!("{}-{}", lang, t.region), false, true),
        (format!("{}-{}", lang, t.script), true, false),
        (lang.clone(), false, false),
    ];
    for (c, consumes_script, consumes_region) in candidates {
        if c.contains("--") || c.ends_with('-') || c.starts_with('-') {
            continue;
        }
        if consumes_script && t.script.is_empty() {
            continue;
        }
        if consumes_region && t.region.is_empty() {
            continue;
        }
        if let Some(rep) = aliases::language_alias(&c.to_lowercase()) {
            let rt = match parse(rep) {
                Some(mut r) => {
                    r.language = r.language.to_lowercase();
                    r.script = titlecase(&r.script);
                    r.region = r.region.to_uppercase();
                    r
                }
                None => continue,
            };
            t.language = rt.language;
            if consumes_script {
                t.script.clear();
            }
            if consumes_region {
                t.region.clear();
            }
            // The replacement supplies subtags only if the target field is now empty.
            if !rt.script.is_empty() && t.script.is_empty() {
                t.script = rt.script;
            }
            if !rt.region.is_empty() && t.region.is_empty() {
                t.region = rt.region;
            }
            for v in rt.variants {
                if !t.variants.contains(&v) {
                    t.variants.push(v);
                }
            }
            return;
        }
    }
}

fn canonicalize_unicode(attrs: &mut Vec<String>, keywords: &mut Vec<(String, Vec<String>)>) {
    for a in attrs.iter_mut() {
        *a = a.to_lowercase();
    }
    attrs.sort();
    attrs.dedup();
    for (key, types) in keywords.iter_mut() {
        *key = key.to_lowercase();
        for ty in types.iter_mut() {
            *ty = ty.to_lowercase();
        }
        // Apply key/type aliases (e.g. ca=islamicc → ca=islamic-civil handled as a whole type).
        let joined = types.join("-");
        if let Some(rep) = aliases::unicode_type_alias(key, &joined) {
            *types = rep.split('-').map(|s| s.to_string()).collect();
        }
        // A single "true" type is dropped (e.g. "kn-true" → "kn").
        if types.len() == 1 && types[0] == "true" {
            types.clear();
        }
    }
    // Sort keywords by key; dedup keeping the first occurrence.
    keywords.sort_by(|a, b| a.0.cmp(&b.0));
    let mut seen: Vec<String> = Vec::new();
    keywords.retain(|(k, _)| {
        if seen.contains(k) {
            false
        } else {
            seen.push(k.clone());
            true
        }
    });
}

/// Render the canonical string form.
pub fn render(t: &LangTag) -> String {
    if t.language.is_empty() && !t.private.is_empty() {
        return t.private.join("-");
    }
    let mut out = String::new();
    out.push_str(&t.language);
    for e in &t.extlangs {
        out.push('-');
        out.push_str(e);
    }
    if !t.script.is_empty() {
        out.push('-');
        out.push_str(&t.script);
    }
    if !t.region.is_empty() {
        out.push('-');
        out.push_str(&t.region);
    }
    for v in &t.variants {
        out.push('-');
        out.push_str(v);
    }
    // Extensions are emitted in singleton order: other singletons (sorted), then t, then u, then x.
    // Per spec they are sorted by singleton; 't' < 'u' alphabetically and both before private-use.
    let mut singletons: Vec<(char, String)> = Vec::new();
    for (s, subs) in &t.other_ext {
        singletons.push((*s, format!("{}-{}", s, subs.join("-"))));
    }
    if let Some((tlang, fields)) = &t.transform {
        let mut s = String::from("t");
        if let Some(l) = tlang {
            // The tlang inside a `-t-` extension is rendered all-lowercase (unlike a top-level tag).
            s.push('-');
            s.push_str(&render(l).to_lowercase());
        }
        for (k, v) in fields {
            s.push('-');
            s.push_str(k);
            s.push('-');
            s.push_str(&v.join("-"));
        }
        singletons.push(('t', s));
    }
    if let Some((attrs, keywords)) = &t.unicode {
        let mut s = String::from("u");
        for a in attrs {
            s.push('-');
            s.push_str(a);
        }
        for (k, types) in keywords {
            s.push('-');
            s.push_str(k);
            for ty in types {
                s.push('-');
                s.push_str(ty);
            }
        }
        singletons.push(('u', s));
    }
    singletons.sort_by_key(|(c, _)| *c);
    for (_c, s) in singletons {
        out.push('-');
        out.push_str(&s);
    }
    if !t.private.is_empty() {
        out.push('-');
        out.push_str(&t.private.join("-"));
    }
    out
}
