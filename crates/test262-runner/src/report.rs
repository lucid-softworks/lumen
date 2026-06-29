//! Writes `test262-report/summary.json` — a small, dependency-free JSON dump so the score is
//! trackable run-over-run (the JS-language analogue of the project's WPT report).

use crate::Tally;
use std::collections::BTreeMap;
use std::io::Write;

pub fn write(
    by_cat: &BTreeMap<String, Tally>,
    total: Tally,
    targets: &[String],
) -> std::io::Result<()> {
    std::fs::create_dir_all("test262-report")?;
    let mut f = std::fs::File::create("test262-report/summary.json")?;

    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"targets\": [{}],\n", json_str_array(targets)));
    out.push_str(&format!(
        "  \"total\": {{ \"pass\": {}, \"fail\": {}, \"skip\": {} }},\n",
        total.pass, total.fail, total.skip
    ));
    let ran = total.pass + total.fail;
    let pct = if ran > 0 {
        100.0 * total.pass as f64 / ran as f64
    } else {
        0.0
    };
    out.push_str(&format!("  \"pass_rate\": {pct:.1},\n"));
    out.push_str("  \"categories\": {\n");
    let mut first = true;
    for (cat, t) in by_cat {
        if !first {
            out.push_str(",\n");
        }
        first = false;
        out.push_str(&format!(
            "    {}: {{ \"pass\": {}, \"fail\": {}, \"skip\": {} }}",
            json_string(cat),
            t.pass,
            t.fail,
            t.skip
        ));
    }
    out.push_str("\n  }\n}\n");

    f.write_all(out.as_bytes())?;
    println!("\nwrote test262-report/summary.json");
    Ok(())
}

fn json_str_array(items: &[String]) -> String {
    items
        .iter()
        .map(|s| json_string(s))
        .collect::<Vec<_>>()
        .join(", ")
}

fn json_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}
