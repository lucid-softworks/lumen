//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(super) fn install_globals(it: &mut Interp) {
    // The test262 async harness ($DONE) reports completion via `print`; route it to the console.
    global_fn(it, "print", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        i.console.push(s.to_string());
        Ok(Value::Undefined)
    });
    global_fn(it, "parseInt", 2, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        // The radix is ToUint32'd (wrapping), so e.g. 2^32+2 means radix 2 and Infinity means 0.
        let radix = match arg(a, 1) {
            Value::Undefined => 0,
            v => ab(i.to_uint32(&v))?,
        };
        Ok(Value::Num(parse_int(&s, radix)))
    });
    global_fn(it, "parseFloat", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        Ok(Value::Num(parse_float(&s)))
    });
    // Number.parseInt / Number.parseFloat are the same functions as the globals.
    if let Some(num) = it.global.borrow().props.get("Number").map(|p| p.value()) {
        let pi = it.global.borrow().props.get("parseInt").map(|p| p.value());
        let pf = it
            .global
            .borrow()
            .props
            .get("parseFloat")
            .map(|p| p.value());
        if let (Value::Obj(n), Some(pi), Some(pf)) = (num, pi, pf) {
            n.borrow_mut()
                .props
                .insert("parseInt", Property::builtin(pi));
            n.borrow_mut()
                .props
                .insert("parseFloat", Property::builtin(pf));
        }
    }
    global_fn(it, "isNaN", 1, |i, _t, a| {
        Ok(Value::Bool(ab(i.to_number(&arg(a, 0)))?.is_nan()))
    });
    global_fn(it, "isFinite", 1, |i, _t, a| {
        Ok(Value::Bool(ab(i.to_number(&arg(a, 0)))?.is_finite()))
    });
    // Annex B escape/unescape.
    global_fn(it, "escape", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        let mut out = String::new();
        for c in crate::jstr::units(&s) {
            let ch = c as u32;
            let keep = c < 128 && {
                let a = ch as u8 as char;
                a.is_ascii_alphanumeric() || "@*_+-./".contains(a)
            };
            if keep {
                out.push(ch as u8 as char);
            } else if ch < 256 {
                out.push_str(&format!("%{ch:02X}"));
            } else {
                out.push_str(&format!("%u{ch:04X}"));
            }
        }
        Ok(Value::from_string(out))
    });
    global_fn(it, "unescape", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        let chars: Vec<char> = s.chars().collect();
        let mut units: Vec<u16> = Vec::new();
        let mut k = 0;
        while k < chars.len() {
            if chars[k] == '%' {
                if k + 5 < chars.len() + 1 && chars.get(k + 1) == Some(&'u') {
                    if let Some(h) = chars
                        .get(k + 2..k + 6)
                        .and_then(|s| u16::from_str_radix(&s.iter().collect::<String>(), 16).ok())
                    {
                        units.push(h);
                        k += 6;
                        continue;
                    }
                } else if let Some(h) = chars
                    .get(k + 1..k + 3)
                    .and_then(|s| u16::from_str_radix(&s.iter().collect::<String>(), 16).ok())
                {
                    units.push(h);
                    k += 3;
                    continue;
                }
            }
            units.push(chars[k] as u16);
            k += 1;
        }
        Ok(Value::from_string(crate::jstr::from_units(&units)))
    });
    global_fn(it, "encodeURIComponent", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        uri_encode(&s, "")
            .map(Value::from_string)
            .ok_or_else(|| i.make_error("URIError", "URI malformed"))
    });
    global_fn(it, "encodeURI", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        uri_encode(&s, ";,/?:@&=+$#")
            .map(Value::from_string)
            .ok_or_else(|| i.make_error("URIError", "URI malformed"))
    });
    global_fn(it, "decodeURIComponent", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        uri_decode(&s, "")
            .map(Value::from_string)
            .ok_or_else(|| i.make_error("URIError", "URI malformed"))
    });
    // decodeURI leaves escapes of the reservedSet (and '#') untouched.
    global_fn(it, "decodeURI", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        uri_decode(&s, ";/?:@&=+$,#")
            .map(Value::from_string)
            .ok_or_else(|| i.make_error("URIError", "URI malformed"))
    });

    // Indirect eval: runs in the global scope. (A *direct* `eval(...)` call is intercepted in
    // `eval_call` and run in the caller's scope; both share this same function object.)
    let eval_fn = it.make_native("eval", 1, |i, _this, args| {
        let code = match arg(args, 0) {
            Value::Str(s) => s,
            other => return Ok(other),
        };
        let env = i.global_env.clone();
        ab(i.perform_eval(&code, &env, false))
    });
    set_builtin(&it.global, "eval", Value::Obj(eval_fn.clone()));
    it.eval_fn = Some(eval_fn);
}

pub(super) fn install_console(it: &mut Interp) {
    let console = it.new_object();
    let log: NativeFn = |i, _t, a| {
        let parts: Result<Vec<String>, Value> = a
            .iter()
            .map(|v| ab(i.to_string(v)).map(|s| s.to_string()))
            .collect();
        i.console.push(parts?.join(" "));
        Ok(Value::Undefined)
    };
    for name in ["log", "info", "warn", "error", "debug"] {
        it.def_method(&console, name, 0, log);
    }
    set_builtin(&it.global, "console", Value::Obj(console));
}

fn parse_int(s: &str, mut radix: u32) -> f64 {
    let t = s.trim_matches(is_js_ws);
    let (neg, mut body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if radix == 0 {
        if let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            radix = 16;
            body = rest;
        } else {
            radix = 10;
        }
    } else if radix == 16 {
        if let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            body = rest;
        }
    }
    if !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let mut acc = 0.0;
    let mut any = false;
    for c in body.chars() {
        match c.to_digit(radix) {
            Some(d) => {
                acc = acc * radix as f64 + d as f64;
                any = true;
            }
            None => break,
        }
    }
    if !any {
        return f64::NAN;
    }
    if neg { -acc } else { acc }
}

fn parse_float(s: &str) -> f64 {
    // StrDecimalLiteral: optional sign, then "Infinity" or digits [. digits] [exponent]. Scan the
    // longest prefix that is itself a valid literal (so "1ex" -> 1, not NaN from "1e").
    let t = s.trim_start_matches(is_js_ws);
    let bytes = t.as_bytes();
    let mut pos = 0;
    let neg = match bytes.first() {
        Some(b'-') => {
            pos += 1;
            true
        }
        Some(b'+') => {
            pos += 1;
            false
        }
        _ => false,
    };
    if t[pos..].starts_with("Infinity") {
        return if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let mut digits = 0;
    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
        pos += 1;
        digits += 1;
    }
    if pos < bytes.len() && bytes[pos] == b'.' {
        pos += 1;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return f64::NAN;
    }
    // An exponent part only counts if at least one digit follows the marker (and optional sign).
    if pos < bytes.len() && matches!(bytes[pos], b'e' | b'E') {
        let mut ep = pos + 1;
        if ep < bytes.len() && matches!(bytes[ep], b'+' | b'-') {
            ep += 1;
        }
        let exp_start = ep;
        while ep < bytes.len() && bytes[ep].is_ascii_digit() {
            ep += 1;
        }
        if ep > exp_start {
            pos = ep;
        }
    }
    t[..pos].parse::<f64>().unwrap_or(f64::NAN)
}
