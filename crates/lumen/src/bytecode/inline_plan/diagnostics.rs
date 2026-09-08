//! Opt-in admission records; identities are printed only, never retained as roots.
use super::{Chunk, Function, Op};
use std::rc::Rc;
pub(super) struct Record<'a> {
    pub caller: &'a Function,
    pub chunk: &'a Chunk,
    pub callee: &'a Function,
    pub callee_chunk: &'a Chunk,
    pub depth: u32,
    pub site: usize,
    pub way: usize,
    pub callee_object: usize,
    pub caller_env: usize,
    pub callee_env: usize,
    pub budget: usize,
    pub free_names: &'a [Rc<str>],
}
pub(super) fn rejected(enabled: bool, record: Record<'_>) {
    if !enabled {
        return;
    }
    eprintln!(
        "[inline-admission] {}",
        json(&record, "distinct-nonglobal-env")
    );
}
fn strings<'a>(values: impl Iterator<Item = &'a str>) -> String {
    format!("[{}]", values.map(quoted).collect::<Vec<_>>().join(","))
}
pub(super) fn source(function: &Function) -> String {
    quoted(function.source.as_deref().unwrap_or(""))
}
fn json(r: &Record<'_>, reason: &str) -> String {
    let uses_this = r.callee_chunk.uses_this();
    let sites = r
        .chunk
        .ops
        .iter()
        .enumerate()
        .filter_map(|(pc, op)| call_site(pc, *op, r.site, uses_this))
        .collect::<Vec<_>>()
        .join(",");
    let free = strings(r.free_names.iter().map(|s| &**s));
    let slots = strings(r.chunk.slot_names.iter().map(|s| &**s));
    let overlap = strings(
        r.free_names
            .iter()
            .filter(|s| r.chunk.slot_names.contains(s))
            .map(|s| &**s),
    );
    let fields = [
        format!("\"reason\":{}", quoted(reason)),
        format!(
            "\"caller_function\":{}",
            r.caller as *const Function as usize
        ),
        format!("\"caller_chunk\":{}", r.chunk as *const Chunk as usize),
        format!("\"caller_source\":{}", source(r.caller)),
        format!(
            "\"callee_function\":{}",
            r.callee as *const Function as usize
        ),
        format!(
            "\"callee_chunk\":{}",
            r.callee_chunk as *const Chunk as usize
        ),
        format!("\"callee_source\":{}", source(r.callee)),
        format!("\"callee_object\":{}", r.callee_object),
        format!("\"caller_env\":{}", r.caller_env),
        format!("\"callee_env\":{}", r.callee_env),
        format!(
            "\"depth\":{},\"site\":{},\"way\":{}",
            r.depth, r.site, r.way
        ),
        format!("\"calls\":[{sites}]"),
        format!(
            "\"callee_ops\":{},\"callee_slots\":{}",
            r.callee_chunk.ops.len(),
            r.callee_chunk.n_slots
        ),
        format!("\"free_names\":{free}"),
        format!(
            "\"budget_remaining\":{},\"inline_cost\":{}",
            r.budget,
            r.callee_chunk.ops.len()
        ),
        format!(
            "\"budget_fits_now\":{}",
            r.callee_chunk.ops.len() <= r.budget
        ),
        format!("\"uses_this\":{uses_this}"),
        format!("\"caller_slot_names\":{slots},\"conservative_slot_overlap\":{overlap}"),
        "\"limits\":\"admission event, not runtime hotness or complete splice eligibility\""
            .to_owned(),
    ];
    format!("{{{}}}", fields.join(","))
}
fn call_site(pc: usize, op: Op, wanted: usize, uses_this: bool) -> Option<String> {
    let (argc, site, receiver) = match op {
        Op::Call(argc, site) => (argc, site, false),
        Op::CallWithThis(argc, site) => (argc, site, true),
        _ => return None,
    };
    if site as usize != wanted {
        return None;
    }
    Some(format!(
        "{{\"pc\":{pc},\"op\":{},\"argc\":{argc},\"has_receiver\":{receiver},\"argc_fits\":{},\"receiver_fits\":{}}}",
        quoted(if receiver{"CallWithThis"}else{"Call"}),argc<=8,receiver||!uses_this,
    ))
}

pub(super) fn quoted(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1f}' => {
                write!(out, "\\u{:04x}", c as u32).expect("String write");
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
impl Record<'_> {
    pub(super) fn accepted(self, enabled: bool, global: bool, shared: bool, after: usize) {
        let record = self;
        if !enabled {
            return;
        }
        let base = json(&record, "accepted-way");
        let body = base.strip_suffix('}').expect("JSON object");
        eprintln!("[inline-admission] {body},\"global_closure\":{global},\"shared_closure\":{shared},\"budget_after_nested\":{after},\"budget_after_direct\":{},\"expected_env\":{}}}",record.budget-record.callee_chunk.ops.len(),if shared{record.callee_env}else{0});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_and_binding_names_are_unambiguous_json_strings() {
        assert_eq!(
            quoted("a\"\\\n\r\t\0\u{1f}😀"),
            "\"a\\\"\\\\\\n\\r\\t\\u0000\\u001f😀\""
        );
        assert_eq!(strings(["x", "y\n"].into_iter()), "[\"x\",\"y\\n\"]");
    }
}
