//! A consuming helper for exact collection builtin hits with one argument.
use crate::builtins::collection_lookup;
use crate::interpreter::{Abrupt, MAX_EVAL_DEPTH};
use crate::jit::{JitCtx, SpFlag};
use crate::value::Value;

/// # Safety
/// The call IC has validated the builtin identity and active realm. The three initialized
/// operands below `sp` are receiver, callee and key, and are consumed on every exit.
pub(crate) unsafe extern "C" fn read(ctx: *mut JitCtx, packed: u32, sp: *mut Value) -> SpFlag {
    let ctx = unsafe { &mut *ctx };
    let chunk = unsafe { &*ctx.chunk };
    unsafe { chunk.record_jit_inline_location(ctx, (packed & 0xffff) as usize) };
    let interp = unsafe { &mut *ctx.interp };
    let base = unsafe { sp.sub(3) };
    interp.depth += 1;
    let result = if interp.depth > MAX_EVAL_DEPTH {
        Err(interp.throw("RangeError", "Maximum call stack size exceeded"))
    } else {
        interp.gc_check_amortized().and_then(|()| {
            // These reads never coerce keys or invoke JS, including brand-error creation.
            // Consequently no constructor-state transition or pending-tail drain is needed.
            collection_lookup::read_intrinsic(
                interp,
                unsafe { &*base },
                unsafe { &*base.add(2) },
                (packed >> 16) as u8,
            )
            .map_err(Abrupt::Throw)
        })
    };
    interp.depth -= 1;
    // Clone the result before releasing any input: it can be the key or collection itself.
    unsafe {
        std::ptr::drop_in_place(base);
        std::ptr::drop_in_place(base.add(1));
        std::ptr::drop_in_place(base.add(2));
    }
    match result {
        Ok(value) => {
            unsafe { base.write(value) };
            SpFlag {
                sp: unsafe { base.add(1) },
                flag: 0,
            }
        }
        Err(error) => {
            ctx.error = Some(error);
            SpFlag { sp: base, flag: 1 }
        }
    }
}
