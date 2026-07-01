//! CLDR alias tables used by tag canonicalization. A deliberately-embedded subset (no external
//! data files); grown as the intl402 conformance score climbs.

/// languageAlias: deprecated language identifier → preferred replacement. Keys are lowercase and
/// may be multi-subtag (`sgn-gr`); values are canonical language identifiers.
pub fn language_alias(key: &str) -> Option<&'static str> {
    Some(match key {
        // Simple deprecated two/three-letter codes.
        "in" => "id",
        "iw" => "he",
        "ji" => "yi",
        "jw" => "jv",
        "mo" => "ro",
        "aam" => "aas",
        "adp" => "dz",
        "aue" => "ktz",
        "ayx" => "nun",
        "bgm" => "bcg",
        "bjd" => "drl",
        "ccq" => "rki",
        "cjr" => "mom",
        "cka" => "cmr",
        "cmk" => "xch",
        "coy" => "pij",
        "cqu" => "quh",
        "drh" => "khk",
        "drw" => "prs",
        "gav" => "dev",
        "gfx" => "vaj",
        "ggn" => "gvr",
        "gti" => "nyc",
        "guv" => "duz",
        "hrr" => "jal",
        "ibi" => "opa",
        "ilw" => "gal",
        "jeg" => "oyb",
        "kgc" => "tdf",
        "kgh" => "kml",
        "koj" => "kwv",
        "krm" => "bmf",
        "ktr" => "dtp",
        "kvs" => "gdj",
        "kwq" => "yam",
        "kxe" => "tvd",
        "kzj" => "dtp",
        "kzt" => "dtp",
        "lii" => "raq",
        "lmm" => "rmx",
        "meg" => "cir",
        "mst" => "mry",
        "mwj" => "vaj",
        "myt" => "mry",
        "nad" => "xny",
        "ncp" => "kdz",
        "nnx" => "ngv",
        "nts" => "pij",
        "oun" => "vaj",
        "pcr" => "adx",
        "pmc" => "huw",
        "pmu" => "phr",
        "ppa" => "bfy",
        "ppr" => "lcq",
        "pry" => "prt",
        "puz" => "pub",
        "sca" => "hle",
        "skk" => "oyb",
        "tdu" => "dtp",
        "thc" => "tpo",
        "thx" => "oyb",
        "tie" => "ras",
        "tkk" => "twm",
        "tlw" => "weo",
        "tmp" => "tyj",
        "tne" => "kak",
        "tnf" => "prs",
        "tsf" => "taj",
        "uok" => "ema",
        "xba" => "cax",
        "xia" => "acn",
        "xkh" => "waw",
        "xsj" => "suj",
        "ybd" => "rki",
        "yma" => "lrr",
        "yos" => "zom",
        "yuu" => "yug",
        // Chinese extlang collapses and macrolanguage preferences.
        "cmn" => "zh",
        // Complex language aliases that carry a script/region.
        "sh" => "sr-Latn",
        "aar" => "aa",
        "tl" => "fil",
        "swc" => "sw-CD",
        "prs" => "fa-AF",
        "zsm" => "ms",
        "arb" => "ar",
        "bh" => "bho",
        // sign-language regional aliases.
        "sgn-br" => "bzs",
        "sgn-co" => "csn",
        "sgn-de" => "gsg",
        "sgn-dk" => "dsl",
        "sgn-es" => "ssp",
        "sgn-fr" => "fsl",
        "sgn-gb" => "bfi",
        "sgn-gr" => "gss",
        "sgn-ie" => "isg",
        "sgn-it" => "ise",
        "sgn-jp" => "jsl",
        "sgn-mx" => "mfs",
        "sgn-ni" => "ncs",
        "sgn-nl" => "dse",
        "sgn-no" => "nsl",
        "sgn-pt" => "psr",
        "sgn-se" => "swl",
        "sgn-us" => "ase",
        "sgn-za" => "sfs",
        _ => return None,
    })
}

/// territoryAlias: deprecated region → preferred (single-target aliases only; multi-target
/// splits pick the first, which matches CLDR's rule for a bare tag).
pub fn region_alias(region: &str) -> Option<&'static str> {
    Some(match region {
        "BU" => "MM",
        "CS" => "RS",
        "CT" => "KI",
        "DD" => "DE",
        "DY" => "BJ",
        "FQ" => "AQ",
        "FX" => "FR",
        "HV" => "BF",
        "JT" => "UM",
        "MI" => "UM",
        "NH" => "VU",
        "NQ" => "AQ",
        "PU" => "UM",
        "PZ" => "PA",
        "QU" => "EU",
        "RH" => "ZW",
        "TP" => "TL",
        "UK" => "GB",
        "SU" => "RU",
        "VD" => "VN",
        "WK" => "UM",
        "YD" => "YE",
        "YU" => "RS",
        "ZR" => "CD",
        // numeric → alpha
        "004" => "AF",
        "008" => "AL",
        "756" => "CH",
        "840" => "US",
        _ => return None,
    })
}

pub fn script_alias(script: &str) -> Option<&'static str> {
    Some(match script {
        "Qaai" => "Zinh",
        _ => return None,
    })
}

pub fn variant_alias(variant: &str) -> Option<&'static str> {
    Some(match variant {
        "heploc" => "alalc97",
        "polytoni" => "polyton",
        _ => return None,
    })
}

/// Unicode `-u-` keyword type aliases: (key, joined-type) → replacement joined-type.
pub fn unicode_type_alias(key: &str, ty: &str) -> Option<&'static str> {
    Some(match (key, ty) {
        ("ca", "islamicc") => "islamic-civil",
        ("ca", "ethiopic-amete-alem") => "ethioaa",
        ("ca", "gregorian") => "gregory",
        ("ms", "imperial") => "uksystem",
        ("tz", "cnckg") => "cnsha",
        ("tz", "eire") => "iedub",
        ("tz", "est") => "utcw05",
        ("tz", "gmt0") => "gmt",
        ("tz", "uct") => "utc",
        ("tz", "zulu") => "utc",
        // Collation strength aliases (ks).
        ("ks", "primary") => "level1",
        ("ks", "secondary") => "level2",
        ("ks", "tertiary") => "level3",
        ("ks", "quaternary") => "level4",
        ("ks", "quarternary") => "level4",
        ("ks", "identical") => "identic",
        // Region-override / subdivision alias (rg / sd share subdivision codes).
        ("rg", "no23") => "no50",
        ("sd", "no23") => "no50",
        ("rg", "cn11") => "cnbj",
        ("kb", "yes") => "true",
        ("kc", "yes") => "true",
        ("kh", "yes") => "true",
        ("kk", "yes") => "true",
        ("kn", "yes") => "true",
        _ => return None,
    })
}

/// Grandfathered / redundant whole-tag replacements (matched case-insensitively).
pub fn grandfathered(tag: &str) -> Option<&'static str> {
    let t = tag.to_ascii_lowercase();
    Some(match t.as_str() {
        "art-lojban" => "jbo",
        "cel-gaulish" => "xtg",
        "en-gb-oed" => "en-GB-oxendict",
        "i-ami" => "ami",
        "i-bnn" => "bnn",
        "i-default" => return None, // remains as-is (irregular, no preferred value)
        "i-enochian" => return None,
        "i-hak" => "hak",
        "i-klingon" => "tlh",
        "i-lux" => "lb",
        "i-mingo" => return None,
        "i-navajo" => "nv",
        "i-pwn" => "pwn",
        "i-tao" => "tao",
        "i-tay" => "tay",
        "i-tsu" => "tsu",
        "no-bok" => "nb",
        "no-nyn" => "nn",
        "sgn-be-fr" => "sfb",
        "sgn-be-nl" => "vgt",
        "sgn-ch-de" => "sgg",
        "zh-guoyu" => "zh",
        "zh-hakka" => "hak",
        "zh-min" => return None,
        "zh-min-nan" => "nan",
        "zh-xiang" => "hsn",
        _ => return None,
    })
}
