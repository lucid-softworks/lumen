//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

/// `Atomics` over integer TypedArrays. lumen is single-threaded, so the read-modify-write ops are
/// plain operations; `wait`/`notify` are no-ops.
pub(super) fn install_atomics(it: &mut Interp) {
    let atomics = Object::new(Some(it.object_proto.clone()));

    fn target(i: &mut Interp, args: &[Value]) -> Result<(TaInfo, usize), Value> {
        target_rw(i, args, false)
    }
    fn target_rw(i: &mut Interp, args: &[Value], write: bool) -> Result<(TaInfo, usize), Value> {
        let ptr = map_ptr(&arg(args, 0))
            .ok_or_else(|| i.make_error("TypeError", "Atomics: not an integer TypedArray"))?;
        let info = *i
            .typed_arrays
            .get(&ptr)
            .ok_or_else(|| i.make_error("TypeError", "Atomics: not an integer TypedArray"))?;
        if matches!(
            info.kind,
            TaKind::F16 | TaKind::F32 | TaKind::F64 | TaKind::U8Clamped
        ) {
            return Err(i.make_error("TypeError", "Atomics requires an integer TypedArray"));
        }
        if write && i.immutable_buffers.contains(&info.buffer) {
            return Err(i.make_error(
                "TypeError",
                "Atomics: cannot write into a view over an immutable ArrayBuffer",
            ));
        }
        // ValidateAtomicAccess: the length is retrieved BEFORE the index coercion (which may
        // detach or resize the buffer); ToIndex truncates toward zero (NaN→0); a negative or
        // out-of-bounds result is a RangeError, a fractional request index simply truncated.
        // ValidateIntegerTypedArray: an already-detached buffer is a TypeError up front.
        let len0 = i
            .ta_len(&info)
            .ok_or_else(|| i.make_error("TypeError", "Atomics: detached TypedArray"))?;
        let raw = ab(i.to_number(&arg(args, 1)))?;
        let idx = if raw.is_nan() { 0.0 } else { raw.trunc() };
        if idx < 0.0 || !idx.is_finite() || idx as usize >= len0 {
            return Err(i.make_error("RangeError", "Atomics: index out of range"));
        }
        Ok((info, idx as usize))
    }

    /// RevalidateAtomicAccess after a value coercion (which may detach or shrink the buffer).
    fn revalidate(i: &mut Interp, info: &TaInfo, idx: usize) -> Result<(), Value> {
        if i.ta_len(info).map(|l| idx < l) != Some(true) {
            return Err(i.make_error(
                "TypeError",
                "Atomics: buffer was detached or shrunk during value coercion",
            ));
        }
        Ok(())
    }
    fn operand(i: &mut Interp, info: &TaInfo, v: &Value) -> Result<i128, Value> {
        if info.kind.is_bigint() {
            Ok(ab(i.to_bigint(v))?.to_i128_wrapping())
        } else {
            let n = ab(i.to_number(v))?;
            Ok(if n.is_finite() { n.trunc() as i128 } else { 0 })
        }
    }
    fn read_i128(i: &Interp, info: &TaInfo, idx: usize) -> i128 {
        match i.ta_read(info, idx) {
            Value::Num(n) => n as i128,
            Value::BigInt(n) => n.to_i128_wrapping(),
            _ => 0,
        }
    }
    fn write_i128(i: &mut Interp, info: &TaInfo, idx: usize, n: i128) {
        if info.kind.is_bigint() {
            i.ta_write_bigint(info, idx, n);
        } else {
            i.ta_write(info, idx, n as f64);
        }
    }
    /// Truncate `v` to the element type's bit width (unsigned domain) for raw comparison.
    fn wrap_bits(kind: TaKind, v: i128) -> u128 {
        let bits = kind.elsize() * 8;
        (v as u128) & (u128::MAX >> (128 - bits))
    }
    fn rmw(i: &mut Interp, args: &[Value], f: fn(i128, i128) -> i128) -> Result<Value, Value> {
        let (info, idx) = target_rw(i, args, true)?;
        let val = operand(i, &info, &arg(args, 2))?;
        revalidate(i, &info, idx)?;
        // One atomic read-modify-write (a shared buffer's lock is held across both halves).
        let old = i.ta_modify(&info, idx, |o| Some(f(o, val))).unwrap_or(0);
        Ok(if info.kind.is_bigint() {
            Value::BigInt(old.into())
        } else {
            Value::Num(old as f64)
        })
    }

    it.def_method(&atomics, "add", 3, |i, _t, a| rmw(i, a, |o, v| o + v));
    it.def_method(&atomics, "sub", 3, |i, _t, a| rmw(i, a, |o, v| o - v));
    it.def_method(&atomics, "and", 3, |i, _t, a| rmw(i, a, |o, v| o & v));
    it.def_method(&atomics, "or", 3, |i, _t, a| rmw(i, a, |o, v| o | v));
    it.def_method(&atomics, "xor", 3, |i, _t, a| rmw(i, a, |o, v| o ^ v));
    it.def_method(&atomics, "exchange", 3, |i, _t, a| rmw(i, a, |_o, v| v));
    it.def_method(&atomics, "load", 2, |i, _t, a| {
        let (info, idx) = target(i, a)?;
        revalidate(i, &info, idx)?;
        Ok(i.ta_read(&info, idx))
    });
    it.def_method(&atomics, "store", 3, |i, _t, a| {
        let (info, idx) = target_rw(i, a, true)?;
        let val = operand(i, &info, &arg(a, 2))?;
        revalidate(i, &info, idx)?;
        write_i128(i, &info, idx, val);
        // store returns the coerced value itself, not the (possibly wrapped) stored representation.
        Ok(if info.kind.is_bigint() {
            Value::BigInt(val.into())
        } else {
            Value::Num(val as f64)
        })
    });
    it.def_method(&atomics, "compareExchange", 4, |i, _t, a| {
        let (info, idx) = target_rw(i, a, true)?;
        let expected = operand(i, &info, &arg(a, 2))?;
        let replacement = operand(i, &info, &arg(a, 3))?;
        revalidate(i, &info, idx)?;
        // The comparison is on the element's raw byte representation, so the expected value
        // wraps to the element type first (e.g. 68547 matches an Int16 2979); the compare and
        // the conditional write happen under one lock hold.
        let old = i
            .ta_modify(&info, idx, |o| {
                if wrap_bits(info.kind, o) == wrap_bits(info.kind, expected) {
                    Some(replacement)
                } else {
                    None
                }
            })
            .unwrap_or(0);
        Ok(if info.kind.is_bigint() {
            Value::BigInt(old.into())
        } else {
            Value::Num(old as f64)
        })
    });
    it.def_method(&atomics, "isLockFree", 1, |i, _t, a| {
        let n = ab(i.to_number(&arg(a, 0)))?;
        Ok(Value::Bool(matches!(n as i64, 1 | 2 | 4 | 8)))
    });
    // ValidateIntegerTypedArray(ta, waitable=true): only a non-detached Int32Array / BigInt64Array,
    // checked BEFORE any index/value coercion (so a poisoned index doesn't run first).
    fn require_waitable(i: &mut Interp, ta: &Value) -> Result<TaInfo, Value> {
        let err = || {
            i.make_error(
                "TypeError",
                "Atomics.wait/notify requires an Int32Array or BigInt64Array",
            )
        };
        let info = map_ptr(ta)
            .and_then(|p| i.typed_arrays.get(&p).copied())
            .ok_or_else(err)?;
        if !matches!(info.kind, TaKind::I32 | TaKind::I64) {
            return Err(err());
        }
        // A detached buffer (regular buffer removed from the store) has no [[ArrayBufferData]].
        if !i.array_buffers.contains_key(&info.buffer)
            && !i.shared_buffers.contains_key(&info.buffer)
        {
            return Err(i.make_error("TypeError", "Atomics.wait/notify: buffer is detached"));
        }
        Ok(info)
    }
    it.def_method(&atomics, "wait", 4, |i, _t, a| {
        let winfo = require_waitable(i, &arg(a, 0))?;
        // Atomics.wait needs a *shared* buffer — checked before ValidateAtomicAccess (index coercion).
        let id = match i.shared_buffers.get(&winfo.buffer) {
            Some(&id) => id,
            None => {
                return Err(i.make_error(
                    "TypeError",
                    "Atomics.wait requires a SharedArrayBuffer-backed array",
                ))
            }
        };
        let (info, idx) = target(i, a)?;
        let expected = operand(i, &info, &arg(a, 2))?;
        // timeout: ToNumber (NaN → +Infinity), clamped at 0.
        let q = ab(i.to_number(&arg(a, 3)))?;
        let timeout = if q.is_nan() || q == f64::INFINITY {
            None
        } else {
            Some(std::time::Duration::from_millis(q.max(0.0) as u64))
        };
        // Only an agent with [[CanBlock]] may suspend (the main agent cannot).
        if !i.can_block {
            return Err(i.make_error(
                "TypeError",
                "Atomics.wait: the calling agent cannot be suspended",
            ));
        }
        if read_i128(i, &info, idx) != expected {
            return Ok(Value::str("not-equal"));
        }
        let byte_index = info.offset + idx * info.kind.elsize();
        let woken = crate::interpreter::futex_wait(id, byte_index, timeout);
        Ok(Value::str(if woken { "ok" } else { "timed-out" }))
    });
    it.def_method(&atomics, "notify", 3, |i, _t, a| {
        require_waitable(i, &arg(a, 0))?;
        let (info, idx) = target(i, a)?;
        // count = ToIntegerOrInfinity(arg2), default +Infinity (= all); negative clamps to 0.
        let count: i64 = match arg(a, 2) {
            Value::Undefined => -1,
            v => {
                let n = ab(i.to_number(&v))?;
                if n.is_nan() || n < 0.0 {
                    0
                } else if n == f64::INFINITY {
                    -1
                } else {
                    n as i64
                }
            }
        };
        // notify is a no-op (returns 0) on a non-shared buffer.
        match i.shared_buffers.get(&info.buffer) {
            Some(&id) => {
                let byte_index = info.offset + idx * info.kind.elsize();
                Ok(Value::Num(
                    crate::interpreter::futex_notify(id, byte_index, count) as f64,
                ))
            }
            None => Ok(Value::Num(0.0)),
        }
    });
    it.def_method(&atomics, "waitAsync", 4, |i, _t, a| {
        // ValidateIntegerTypedArray(waitable) runs before any index/value coercion.
        let winfo = require_waitable(i, &arg(a, 0))?;
        let id = match i.shared_buffers.get(&winfo.buffer) {
            Some(&id) => id,
            None => {
                return Err(i.make_error(
                    "TypeError",
                    "Atomics.waitAsync requires a SharedArrayBuffer-backed array",
                ))
            }
        };
        let (info, idx) = target(i, a)?;
        let expected = operand(i, &info, &arg(a, 2))?;
        // timeout: ToNumber (NaN → +Infinity); None means "no timeout".
        let q = ab(i.to_number(&arg(a, 3)))?;
        let timeout = if q.is_nan() || q == f64::INFINITY {
            None
        } else {
            Some(std::time::Duration::from_millis(q.max(0.0) as u64))
        };
        let result = i.new_object();
        // The value already differs ⇒ resolved synchronously (not async).
        if read_i128(i, &info, idx) != expected {
            set_data(&result, "async", Value::Bool(false));
            set_data(&result, "value", Value::str("not-equal"));
            return Ok(Value::Obj(result));
        }
        // A zero timeout times out immediately (still synchronous).
        if matches!(timeout, Some(d) if d.is_zero()) {
            set_data(&result, "async", Value::Bool(false));
            set_data(&result, "value", Value::str("timed-out"));
            return Ok(Value::Obj(result));
        }
        // Otherwise wait asynchronously: a waiter thread reports the outcome, which the event loop
        // uses to resolve the returned promise.
        let byte_index = info.offset + idx * info.kind.elsize();
        // The waiter joins the wait list synchronously (a notify later in this same job must see
        // it); only the blocking happens on the helper thread.
        let waiter = crate::interpreter::futex_register(id, byte_index);
        let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
        std::thread::spawn(move || {
            let woken = crate::interpreter::futex_block(&waiter, id, byte_index, timeout);
            let _ = tx.send(if woken { "ok" } else { "timed-out" });
        });
        let promise = i.new_promise();
        i.pending_async_waits.push((promise.clone(), rx));
        set_data(&result, "async", Value::Bool(true));
        set_data(&result, "value", promise);
        Ok(Value::Obj(result))
    });
    it.def_method(&atomics, "pause", 0, |i, _t, a| {
        // The optional iterationNumber, if present, must be an integral Number.
        match arg(a, 0) {
            Value::Undefined => {}
            Value::Num(n) if n.fract() == 0.0 && n.is_finite() => {}
            _ => {
                return Err(i.make_error(
                    "TypeError",
                    "Atomics.pause iterationNumber must be an integer",
                ))
            }
        }
        Ok(Value::Undefined)
    });
    set_to_string_tag(it, &atomics, "Atomics");
    set_builtin(&it.global, "Atomics", Value::Obj(atomics));
}
