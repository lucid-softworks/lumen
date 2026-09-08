//! Final targets that survived inline AST emission and optimized compilation.
use super::{diagnostics, Chunk, Function, Op};
use crate::value::Callable;
use std::rc::Rc;
pub(crate) fn record(function: &Function, chunk: &Chunk) {
    if std::env::var_os("LUMEN_INLINE_ADMISSION").is_none() {
        return;
    }
    for (index, target) in chunk.inline_targets.iter().enumerate() {
        let Some(object) = target.pin.upgrade() else {
            continue;
        };
        let borrowed = object.borrow();
        let Callable::User(user) = &borrowed.call else {
            continue;
        };
        let pcs = chunk
            .ops
            .iter()
            .enumerate()
            .filter_map(|(pc, op)| {
                matches!(op,Op::InlineGuard(t,_) if *t as usize==index).then_some(pc.to_string())
            })
            .collect::<Vec<_>>()
            .join(",");
        let fields = [
            "\"reason\":\"compiled-target\"".to_owned(),
            format!(
                "\"root_function\":{},\"root_chunk\":{}",
                function as *const Function as usize, chunk as *const Chunk as usize
            ),
            format!("\"root_source\":{}", diagnostics::source(function)),
            format!(
                "\"target_index\":{index},\"callee_object\":{}",
                Rc::as_ptr(&object) as usize
            ),
            format!(
                "\"callee_function\":{},\"callee_source\":{}",
                Rc::as_ptr(&user.func) as usize,
                diagnostics::source(&user.func)
            ),
            format!(
                "\"expected_env\":{},\"callee_env\":{}",
                target.expected_env,
                Rc::as_ptr(&user.env) as usize
            ),
            format!(
                "\"argc\":{},\"check_this\":{},\"guard_pcs\":[{pcs}]",
                target.argc, target.check_this
            ),
        ];
        eprintln!("[inline-admission] {{{}}}", fields.join(","));
    }
}
