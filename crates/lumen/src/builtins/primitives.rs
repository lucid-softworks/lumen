//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(super) fn install_number(it: &mut Interp) {
    let np = it.number_proto.clone();
    it.def_method(&np, "toLocaleString", 0, |i, this, args| {
        let n = this_number(i, &this)?;
        intl_delegate(
            i,
            "NumberFormat",
            arg(args, 0),
            arg(args, 1),
            "format",
            &[Value::Num(n)],
        )
    });
    it.def_method(&np, "toString", 1, |i, this, args| {
        let n = this_number(i, &this)?;
        let radix = match arg(args, 0) {
            Value::Undefined => 10.0,
            v => {
                let r = ab(i.to_number(&v))?;
                if r.is_nan() {
                    0.0
                } else {
                    r.trunc()
                }
            }
        };
        if !(2.0..=36.0).contains(&radix) {
            return Err(i.make_error("RangeError", "toString() radix must be between 2 and 36"));
        }
        if radix == 10.0 {
            Ok(Value::from_string(i.num_to_str(n)))
        } else {
            Ok(Value::from_string(to_radix_string(n, radix as u32)))
        }
    });
    it.def_method(&np, "valueOf", 0, |i, this, _| {
        Ok(Value::Num(this_number(i, &this)?))
    });
    it.def_method(&np, "toExponential", 1, |i, this, args| {
        let x = this_number(i, &this)?;
        let fd = arg(args, 0);
        let f = ab(i.to_number(&fd))?;
        let f = if f.is_nan() { 0.0 } else { f.trunc() };
        if x.is_nan() {
            return Ok(Value::str("NaN"));
        }
        if x.is_infinite() {
            return Ok(Value::str(if x > 0.0 { "Infinity" } else { "-Infinity" }));
        }
        if !(0.0..=100.0).contains(&f) {
            return Err(i.make_error(
                "RangeError",
                "toExponential() argument must be between 0 and 100",
            ));
        }
        let f = f as usize;
        Ok(Value::from_string(to_exponential(
            x,
            f,
            matches!(fd, Value::Undefined),
        )))
    });
    it.def_method(&np, "toPrecision", 1, |i, this, args| {
        let n = this_number(i, &this)?;
        if matches!(arg(args, 0), Value::Undefined) {
            return Ok(Value::from_string(i.num_to_str(n)));
        }
        let p = ab(i.to_number(&arg(args, 0)))?;
        if n.is_nan() {
            return Ok(Value::str("NaN"));
        }
        if n.is_infinite() {
            return Ok(Value::from_string(i.num_to_str(n)));
        }
        if !(1.0..=100.0).contains(&p) {
            return Err(i.make_error(
                "RangeError",
                "toPrecision() argument must be between 1 and 100",
            ));
        }
        Ok(Value::from_string(to_precision(n, p as usize)))
    });
    it.def_method(&np, "toFixed", 1, |i, this, args| {
        let n = this_number(i, &this)?;
        // ToIntegerOrInfinity(fractionDigits): undefined/NaN → 0, otherwise truncate toward zero.
        let raw = ab(i.to_number(&arg(args, 0)))?;
        let d = if raw.is_nan() { 0.0 } else { raw.trunc() };
        // Spec: fractionDigits in 0..=100, else RangeError (also guards a giant `format!`).
        if !(0.0..=100.0).contains(&d) {
            return Err(i.make_error(
                "RangeError",
                "toFixed() digits argument must be between 0 and 100",
            ));
        }
        if n.is_nan() {
            return Ok(Value::str("NaN"));
        }
        // For magnitudes ≥ 1e21 toFixed falls back to Number::toString.
        if n.abs() >= 1e21 {
            return Ok(Value::from_string(i.num_to_str(n)));
        }
        let digits = d as usize;
        // The sign is `-` only for a strictly-negative value (not -0), and the magnitude is rounded.
        let body = to_fixed_magnitude(n.abs(), digits);
        Ok(Value::from_string(if n < 0.0 {
            format!("-{body}")
        } else {
            body
        }))
    });

    let ctor = it.make_native("Number", 1, |i, _this, args| {
        let n = match args.first() {
            None => 0.0,
            // Number(bigint) explicitly converts (only *implicit* ToNumber of a BigInt throws).
            Some(Value::BigInt(n)) => n.to_f64(),
            Some(v) => ab(i.to_number(v))?,
        };
        maybe_box(i, Value::Num(n))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(np.clone()), false, false, false),
    );
    np.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    // The numeric constants are { writable:false, enumerable:false, configurable:false }.
    for (name, val) in [
        ("MAX_SAFE_INTEGER", 9007199254740991.0),
        ("MIN_SAFE_INTEGER", -9007199254740991.0),
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", f64::from_bits(1)), // 5e-324, the smallest subnormal
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
        ("EPSILON", f64::EPSILON),
    ] {
        ctor.borrow_mut()
            .props
            .insert(name, Property::data(Value::Num(val), false, false, false));
    }
    it.def_method(&ctor, "isNaN", 1, |_i, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_nan()),
        ))
    });
    it.def_method(&ctor, "isFinite", 1, |_i, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_finite()),
        ))
    });
    it.def_method(&ctor, "isSafeInteger", 1, |_i, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0 && n.abs() <= 9007199254740991.0),
        ))
    });
    it.def_method(&ctor, "isInteger", 1, |_i, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0),
        ))
    });
    set_builtin(&it.global, "Number", Value::Obj(ctor));
}

/// Format the magnitude `x` (assumed ≥ 0, finite, and < 1e21) to exactly `digits` fractional
/// places for `Number.prototype.toFixed`. The spec rounds to nearest and, on an *exact* tie, picks
/// the larger n (ties away from zero) — whereas Rust's `{:.*}` rounds ties to even. The tie must be
/// judged on the true binary64 value: `0.15` is really `0.1499…` and must round down, while `0.5`
/// is an exact tie and rounds up. We therefore expand `x` to its exact decimal digits and round
/// half-up ourselves: round up iff the exact digit just past the cut is ≥ 5.
fn to_fixed_magnitude(x: f64, digits: usize) -> String {
    // The exact decimal expansion of a finite f64 terminates. Its number of fractional digits is
    // `-e2` where `x = significand × 2^e2` with an integer significand — bounded by 1074 (the
    // smallest subnormal). Formatting to at least that many places incurs no rounding, so the digit
    // we inspect is the true one. Derive the needed precision from the bit pattern to avoid always
    // emitting ~1074 digits.
    let bits = x.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i64;
    let mantissa = bits & 0xf_ffff_ffff_ffff;
    let significand = if biased == 0 {
        mantissa // subnormal (or zero)
    } else {
        (1u64 << 52) | mantissa // normal: implicit leading 1
    };
    let e2 = if biased == 0 { -1074 } else { biased - 1075 } + significand.trailing_zeros() as i64;
    let exact_frac = if e2 < 0 && significand != 0 {
        (-e2) as usize
    } else {
        0
    };
    // Expand to enough places to (a) reach the digit past the cut and (b) be exact there.
    let prec = exact_frac.max(digits + 1);
    let exact = format!("{:.*}", prec, x);
    let dot = exact.find('.').unwrap();
    let int_digits = exact[..dot].bytes();
    let frac = &exact.as_bytes()[dot + 1..];
    let round_up = frac[digits] >= b'5';
    // Digits we keep: the whole integer part plus the first `digits` fractional digits.
    let mut ds: Vec<u8> = int_digits
        .chain(frac[..digits].iter().copied())
        .map(|b| b - b'0')
        .collect();
    if round_up {
        // Propagate the carry leftward; a carry out of the most-significant digit prepends a 1.
        let mut i = ds.len();
        loop {
            match i.checked_sub(1) {
                None => {
                    ds.insert(0, 1);
                    break;
                }
                Some(j) => {
                    i = j;
                    if ds[j] == 9 {
                        ds[j] = 0;
                    } else {
                        ds[j] += 1;
                        break;
                    }
                }
            }
        }
    }
    let int_len = ds.len() - digits; // grew by 1 iff the carry rippled out the front
    let mut out = String::with_capacity(ds.len() + 1);
    out.extend(ds[..int_len].iter().map(|d| (d + b'0') as char));
    if digits > 0 {
        out.push('.');
        out.extend(ds[int_len..].iter().map(|d| (d + b'0') as char));
    }
    out
}

fn to_radix_string(n: f64, radix: u32) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n < 0.0 { "-Infinity" } else { "Infinity" }.to_string();
    }
    if n == 0.0 {
        return "0".to_string();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let neg = n < 0.0;
    let x = n.abs();
    let mut int = x.trunc() as u64;
    // Integer part (most-significant digit first after reversal).
    let mut ipart = Vec::new();
    if int == 0 {
        ipart.push(b'0');
    }
    while int > 0 {
        ipart.push(digits[(int % radix as u64) as usize]);
        int /= radix as u64;
    }
    ipart.reverse();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(&ipart).unwrap());
    // Fractional part: repeatedly multiply by the radix, emitting the integer digit each step.
    let mut frac = x.fract();
    if frac > 0.0 {
        out.push('.');
        let mut count = 0;
        while frac > 0.0 && count < 52 {
            frac *= radix as f64;
            let d = frac.trunc() as usize;
            out.push(digits[d] as char);
            frac -= d as f64;
            count += 1;
        }
    }
    out
}

pub(super) fn install_boolean(it: &mut Interp) {
    let bp = it.boolean_proto.clone();
    it.def_method(&bp, "toString", 0, |i, this, _| {
        Ok(Value::str(if this_boolean(i, &this)? {
            "true"
        } else {
            "false"
        }))
    });
    it.def_method(&bp, "valueOf", 0, |i, this, _| {
        Ok(Value::Bool(this_boolean(i, &this)?))
    });
    let ctor = it.make_native("Boolean", 1, |i, _this, args| {
        let b = Value::Bool(i.to_boolean(&arg(args, 0)));
        maybe_box(i, b)
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(bp.clone()), false, false, false),
    );
    bp.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "Boolean", Value::Obj(ctor));
}

/// thisBooleanValue: a Boolean primitive or Boolean wrapper, else TypeError.
fn this_boolean(i: &mut Interp, this: &Value) -> Result<bool, Value> {
    match this {
        Value::Bool(b) => Ok(*b),
        Value::Obj(o) => match o.borrow().exotic {
            Exotic::BoolWrap(b) => Ok(b),
            _ => Err(i.make_error(
                "TypeError",
                "Boolean method called on incompatible receiver",
            )),
        },
        _ => Err(i.make_error(
            "TypeError",
            "Boolean method called on incompatible receiver",
        )),
    }
}

/// thisSymbolValue: a Symbol primitive or Symbol wrapper object, else TypeError.
fn this_symbol(i: &mut Interp, this: &Value) -> Result<Rc<SymbolData>, Value> {
    match this {
        Value::Sym(s) => Ok(s.clone()),
        Value::Obj(o) => match &o.borrow().exotic {
            Exotic::SymWrap(s) => Ok(s.clone()),
            _ => Err(i.make_error("TypeError", "Symbol method called on incompatible receiver")),
        },
        _ => Err(i.make_error("TypeError", "Symbol method called on incompatible receiver")),
    }
}

pub(super) fn install_symbol(it: &mut Interp) {
    let sp = it.symbol_proto.clone();
    it.def_method(&sp, "toString", 0, |i, this, _| {
        match this_symbol(i, &this) {
            Ok(s) => Ok(Value::from_string(format!(
                "Symbol({})",
                s.description.as_deref().unwrap_or("")
            ))),
            Err(e) => Err(e),
        }
    });
    it.def_method(&sp, "valueOf", 0, |i, this, _| {
        match this_symbol(i, &this) {
            Ok(s) => Ok(Value::Sym(s)),
            Err(e) => Err(e),
        }
    });

    let desc_getter = it.make_native("get description", 0, |i, this, _| {
        match this_symbol(i, &this) {
            Ok(s) => Ok(s
                .description
                .as_deref()
                .map(|d| Value::from_string(d.to_string()))
                .unwrap_or(Value::Undefined)),
            _ => Err(i.make_error(
                "TypeError",
                "Symbol.prototype.description requires a symbol",
            )),
        }
    });
    sp.borrow_mut().props.insert(
        "description",
        Property::accessor_prop(Some(Value::Obj(desc_getter)), None, false, true),
    );

    let ctor = it.make_native("Symbol", 0, |i, _this, args| {
        if i.constructing {
            return Err(i.make_error("TypeError", "Symbol is not a constructor"));
        }
        let desc = match arg(args, 0) {
            Value::Undefined => None,
            v => Some(ab(i.to_string(&v))?),
        };
        Ok(i.new_symbol(desc.map(Into::into)))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(sp.clone()), false, false, false),
    );
    sp.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));

    // The intrinsic %Symbol% is cached so well-known-symbol lookups survive `globalThis.Symbol`
    // being replaced by user code.
    it.extra_protos.insert("%SymbolCtor%", ctor.clone());
    // Well-known symbols (each a unique, frozen instance on the constructor).
    for name in [
        "iterator",
        "asyncIterator",
        "hasInstance",
        "isConcatSpreadable",
        "match",
        "matchAll",
        "replace",
        "search",
        "species",
        "split",
        "toPrimitive",
        "toStringTag",
        "unscopables",
        "dispose",
        "asyncDispose",
        "metadata",
    ] {
        // Reuse the descriptor if this Interp already minted it (a secondary realm): well-known
        // symbols are shared across realms.
        let sym = match it.wk_syms.iter().find(|(n, _, _)| *n == name) {
            Some((_, v, _)) => v.clone(),
            None => {
                let s = it.new_symbol(Some(Rc::from(format!("Symbol.{name}").as_str())));
                let Value::Sym(data) = &s else {
                    unreachable!("new_symbol must return a symbol")
                };
                let key: Rc<str> = Rc::from(Interp::sym_key(data));
                it.wk_syms.push((name, s.clone(), key));
                s
            }
        };
        if name == "iterator" {
            if let Value::Sym(d) = &sym {
                it.iterator_sym = Some(d.clone());
            }
        }
        ctor.borrow_mut()
            .props
            .insert(name, Property::data(sym, false, false, false));
    }

    it.def_method(&ctor, "for", 1, |i, _this, args| {
        let key = ab(i.to_string(&arg(args, 0)))?.to_string();
        if let Some(d) = crate::interpreter::sym_for_get(&key) {
            return Ok(Value::Sym(d));
        }
        let sym = i.new_symbol(Some(Rc::from(key.as_str())));
        if let Value::Sym(d) = &sym {
            crate::interpreter::sym_for_insert(key, d.clone());
        }
        Ok(sym)
    });
    it.def_method(&ctor, "keyFor", 1, |i, _this, args| {
        let Value::Sym(s) = arg(args, 0) else {
            return Err(i.make_error("TypeError", "Symbol.keyFor: argument is not a Symbol"));
        };
        Ok(crate::interpreter::sym_for_key_of(&s)
            .map(Value::from_string)
            .unwrap_or(Value::Undefined))
    });
    set_builtin(&it.global, "Symbol", Value::Obj(ctor));

    // These need the well-known symbols, which are only reachable via the global Symbol now installed.
    // Symbol.prototype[@@toPrimitive](hint) returns thisSymbolValue; { writable:false }.
    let to_prim = it.make_native("[Symbol.toPrimitive]", 1, |i, this, _| {
        Ok(Value::Sym(this_symbol(i, &this)?))
    });
    if let Some(key) = well_known_key(it, "toPrimitive") {
        sp.borrow_mut()
            .props
            .insert(key, Property::data(Value::Obj(to_prim), false, false, true));
    }
    // Symbol.prototype[@@toStringTag] = "Symbol"; { writable:false }.
    if let Some(key) = well_known_key(it, "toStringTag") {
        sp.borrow_mut().props.insert(
            key,
            Property::data(Value::from_string("Symbol".to_string()), false, false, true),
        );
    }
}

/// ToBigInt: coerce a value to a BigInt primitive, following the spec's allowed conversions.
fn to_bigint(i: &mut Interp, v: &Value) -> Result<crate::bigint::JsBigInt, Value> {
    ab(i.to_bigint(v))
}

/// thisBigIntValue: a BigInt primitive or BigInt wrapper object, else TypeError.
fn this_bigint(i: &mut Interp, this: &Value) -> Result<crate::bigint::JsBigInt, Value> {
    match this {
        Value::BigInt(n) => Ok(n.clone()),
        Value::Obj(o) => match &o.borrow().exotic {
            Exotic::BigIntWrap(n) => Ok((**n).clone()),
            _ => Err(i.make_error("TypeError", "BigInt method called on incompatible receiver")),
        },
        _ => Err(i.make_error("TypeError", "BigInt method called on incompatible receiver")),
    }
}

pub(super) fn install_bigint(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("BigInt", proto.clone());
    it.def_method(&proto, "toString", 0, |i, this, a| {
        let n = this_bigint(i, &this)?;
        let radix = match arg(a, 0) {
            Value::Undefined => 10,
            v => {
                let r = ab(i.to_number(&v))?.trunc();
                if !(2.0..=36.0).contains(&r) {
                    return Err(
                        i.make_error("RangeError", "toString radix must be between 2 and 36")
                    );
                }
                r as u32
            }
        };
        Ok(Value::from_string(n.to_string_radix(radix)))
    });
    it.def_method(&proto, "valueOf", 0, |i, this, _| {
        Ok(Value::BigInt(this_bigint(i, &this)?))
    });
    it.def_method(&proto, "toLocaleString", 0, |i, this, args| {
        let b = this_bigint(i, &this)?;
        intl_delegate(
            i,
            "NumberFormat",
            arg(args, 0),
            arg(args, 1),
            "format",
            &[Value::BigInt(b)],
        )
    });
    let ctor = it.make_native("BigInt", 1, |i, _t, a| {
        if i.constructing {
            return Err(i.make_error("TypeError", "BigInt is not a constructor"));
        }
        // ToPrimitive(value, number) first; a Number primitive then goes through
        // NumberToBigInt (RangeError for NaN/Infinity/non-integral), unlike plain ToBigInt.
        let prim = match arg(a, 0) {
            v @ Value::Obj(_) => ab(i.to_primitive(&v, crate::eval::Hint::Number))?,
            v => v,
        };
        match prim {
            Value::BigInt(n) => Ok(Value::BigInt(n)),
            Value::Num(n) => crate::bigint::JsBigInt::from_f64(n)
                .map(Value::BigInt)
                .ok_or_else(|| {
                    i.make_error("RangeError", "The number cannot be converted to a BigInt")
                }),
            Value::Bool(b) => Ok(Value::BigInt(crate::bigint::JsBigInt::from_u64(b as u64))),
            Value::Str(s) => ab(i.to_bigint(&Value::Str(s))).map(Value::BigInt),
            _ => Err(i.make_error("TypeError", "Cannot convert value to a BigInt")),
        }
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "asIntN", 2, |i, _t, a| {
        use crate::bigint::JsBigInt;
        let bits = to_index(i, &arg(a, 0))? as u64;
        let n = to_bigint(i, &arg(a, 1))?;
        if bits == 0 {
            return Ok(Value::BigInt(JsBigInt::zero()));
        }
        // A width beyond any plausible magnitude: the value is already in range (or the result
        // would be too large to represent).
        if bits > (1 << 26) {
            if (n.bit_len() as u64) < bits {
                return Ok(Value::BigInt(n));
            }
            return Err(i.make_error("RangeError", "BigInt is too large to allocate"));
        }
        // n mod 2^bits via two's-complement masking, then subtract 2^bits if the sign bit is set.
        let m = JsBigInt::from_u64(1).shl(bits);
        let r = n.bitand(&m.sub(&JsBigInt::from_u64(1)));
        let half = JsBigInt::from_u64(1).shl(bits - 1);
        Ok(Value::BigInt(if r.cmp(&half).is_lt() {
            r
        } else {
            r.sub(&m)
        }))
    });
    it.def_method(&ctor, "asUintN", 2, |i, _t, a| {
        use crate::bigint::JsBigInt;
        let bits = to_index(i, &arg(a, 0))? as u64;
        let n = to_bigint(i, &arg(a, 1))?;
        if bits == 0 {
            return Ok(Value::BigInt(JsBigInt::zero()));
        }
        if bits > (1 << 26) {
            if !n.is_negative() && (n.bit_len() as u64) <= bits {
                return Ok(Value::BigInt(n));
            }
            return Err(i.make_error("RangeError", "BigInt is too large to allocate"));
        }
        let mask = JsBigInt::from_u64(1).shl(bits).sub(&JsBigInt::from_u64(1));
        Ok(Value::BigInt(n.bitand(&mask)))
    });
    set_builtin(&it.global, "BigInt", Value::Obj(ctor));

    // BigInt.prototype[@@toStringTag] = "BigInt"; { writable:false, enumerable:false, configurable:true }.
    if let Some(key) = well_known_key(it, "toStringTag") {
        proto.borrow_mut().props.insert(
            key,
            Property::data(Value::from_string("BigInt".to_string()), false, false, true),
        );
    }
}
