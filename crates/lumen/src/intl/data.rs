//! Embedded CLDR-derived formatting data (a deliberate subset). Grown as the intl402 score climbs.

/// The four list patterns (two, start, middle, end) for a (language, type, style). Falls back to
/// English when the language is unknown. `{0}`/`{1}` are the placeholders.
pub fn list_patterns(lang: &str, kind: &str, style: &str) -> [&'static str; 4] {
    const COMMA: [&str; 4] = ["{0}, {1}", "{0}, {1}", "{0}, {1}", "{0}, {1}"];
    const SPACE: [&str; 4] = ["{0} {1}", "{0} {1}", "{0} {1}", "{0} {1}"];
    // The `unit` type: narrow is space-joined everywhere; short/long is comma-joined, except a few
    // languages (Spanish) whose `unit-long` uses the conjunction form.
    if kind == "unit" {
        return match (lang, style) {
            (_, "narrow") => SPACE,
            ("es", "long") => ["{0} y {1}", "{0}, {1}", "{0}, {1}", "{0} y {1}"],
            _ => COMMA,
        };
    }
    if lang == "es" {
        return match (kind, style) {
            ("conjunction", "long") => ["{0} y {1}", "{0}, {1}", "{0}, {1}", "{0} y {1}"],
            ("conjunction", _) => ["{0} y {1}", "{0}, {1}", "{0}, {1}", "{0}, {1}"],
            ("disjunction", "long") => ["{0} o {1}", "{0}, {1}", "{0}, {1}", "{0} o {1}"],
            ("disjunction", _) => ["{0} o {1}", "{0}, {1}", "{0}, {1}", "{0}, {1}"],
            (_, _) => COMMA,
        };
    }
    // English (and the default for any language we do not yet ship list data for).
    match (kind, style) {
        ("conjunction", "long") => ["{0} and {1}", "{0}, {1}", "{0}, {1}", "{0}, and {1}"],
        ("conjunction", "short") => ["{0} & {1}", "{0}, {1}", "{0}, {1}", "{0}, & {1}"],
        ("conjunction", "narrow") => SPACE,
        ("disjunction", "long") => ["{0} or {1}", "{0}, {1}", "{0}, {1}", "{0}, or {1}"],
        ("disjunction", "short") => ["{0} or {1}", "{0}, {1}", "{0}, {1}", "{0}, or {1}"],
        ("disjunction", "narrow") => ["{0} or {1}", "{0}, {1}", "{0}, {1}", "{0}, or {1}"],
        (_, _) => COMMA,
    }
}

/// Cardinal plural category of `n` (an already-formatted non-negative number, given as its integer
/// value `i` plus whether it had a fractional part) for a language. Returns one of
/// "zero"/"one"/"two"/"few"/"many"/"other". A small subset of the CLDR plural rules.
pub fn plural_cardinal(lang: &str, i: u64, has_fraction: bool) -> &'static str {
    match lang {
        // English-like: one iff i==1 and no fraction.
        "en" | "de" | "nl" | "it" | "es" | "pt" | "sv" | "da" | "et" | "fi" => {
            if i == 1 && !has_fraction {
                "one"
            } else {
                "other"
            }
        }
        // French: one iff i==0 or i==1.
        "fr" => {
            if (i == 0 || i == 1) && !has_fraction {
                "one"
            } else {
                "other"
            }
        }
        // Japanese/Chinese/Korean: always other.
        "ja" | "zh" | "ko" | "th" | "id" | "vi" => "other",
        _ => {
            if i == 1 && !has_fraction {
                "one"
            } else {
                "other"
            }
        }
    }
}

/// The plural categories a language distinguishes (for PluralRules.resolvedOptions().pluralCategories).
pub fn plural_categories(lang: &str) -> &'static [&'static str] {
    match lang {
        "ja" | "zh" | "ko" | "th" | "id" | "vi" => &["other"],
        "ar" => &["zero", "one", "two", "few", "many", "other"],
        "ru" | "uk" | "pl" => &["one", "few", "many", "other"],
        "cs" | "sk" => &["one", "few", "many", "other"],
        _ => &["one", "other"],
    }
}
