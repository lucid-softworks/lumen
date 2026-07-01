//! Embedded CLDR-derived formatting data (a deliberate subset). Grown as the intl402 score climbs.

/// The (decimal, group) separators for a locale's `latn` number symbols, and the grouping sizes
/// (primary, secondary) — e.g. Indian `en-IN` groups as 3;2.
pub fn number_symbols(lang: &str, region: &str) -> (&'static str, &'static str, (usize, usize)) {
    // (decimal, group, (primary_group, secondary_group))
    match (lang, region) {
        ("en", "IN") | ("hi", _) | ("bn", "IN") | ("ta", "IN") => (".", ",", (3, 2)),
        ("de", _) | ("es", "ES") | ("it", _) | ("nl", _) | ("pt", "PT") | ("da", _)
        | ("id", _) | ("tr", _) => (",", ".", (3, 3)),
        ("fr", _) | ("ru", _) | ("pl", _) | ("cs", _) | ("hu", _) | ("fi", _) | ("sv", _) => {
            (",", "\u{202f}", (3, 3))
        }
        ("es", _) => (",", ".", (3, 3)),
        ("pt", _) => (",", ".", (3, 3)),
        // English, Japanese, Chinese, Korean, and the default: "." decimal, "," group, 3;3.
        _ => (".", ",", (3, 3)),
    }
}

/// The localized "NaN" symbol (most locales use "NaN"; Traditional Chinese differs).
pub fn nan_symbol(lang: &str, region: &str) -> &'static str {
    match (lang, region) {
        ("zh", "TW") | ("zh", "HK") | ("zh", "MO") | ("yue", _) => "非數值",
        _ => "NaN",
    }
}

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
