//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(super) fn install_math(it: &mut Interp) {
    let math = it.new_object();
    // The Math constants are { writable:false, enumerable:false, configurable:false }.
    for (name, val) in [
        ("E", std::f64::consts::E),
        ("LN10", std::f64::consts::LN_10),
        ("LN2", std::f64::consts::LN_2),
        ("LOG10E", std::f64::consts::LOG10_E),
        ("LOG2E", std::f64::consts::LOG2_E),
        ("PI", std::f64::consts::PI),
        ("SQRT1_2", std::f64::consts::FRAC_1_SQRT_2),
        ("SQRT2", std::f64::consts::SQRT_2),
    ] {
        math.borrow_mut()
            .props
            .insert(name, Property::data(Value::Num(val), false, false, false));
    }
    // Math[@@toStringTag] = "Math" (non-writable, non-enumerable, configurable).
    if let Some(key) = well_known_key(it, "toStringTag") {
        math.borrow_mut()
            .props
            .insert(key, Property::data(Value::str("Math"), false, false, true));
    }
    macro_rules! unary {
        ($name:expr, $f:expr) => {
            it.def_method(&math, $name, 1, |i, _t, a| {
                let x = ab(i.to_number(&arg(a, 0)))?;
                Ok(Value::Num($f(x)))
            });
        };
    }
    unary!("abs", f64::abs);
    unary!("floor", f64::floor);
    unary!("ceil", f64::ceil);
    // Math.round ties toward +Inf and keeps a negative sign for [-0.5, 0). Computed from floor(x)
    // (not floor(x + 0.5), which wrongly rounds up e.g. 0.5 - ε/4 and some large odd integers).
    unary!("round", |x: f64| {
        if x.is_nan() || x.is_infinite() || x == 0.0 {
            x
        } else {
            let f = x.floor();
            let r = if x - f >= 0.5 { f + 1.0 } else { f };
            if r == 0.0 && x < 0.0 {
                -0.0
            } else {
                r
            }
        }
    });
    unary!("trunc", f64::trunc);
    unary!("sqrt", f64::sqrt);
    unary!("cbrt", f64::cbrt);
    unary!("sign", |x: f64| if x.is_nan() || x == 0.0 {
        x
    } else {
        x.signum()
    });
    unary!("expm1", f64::exp_m1);
    unary!("log1p", f64::ln_1p);
    unary!("sinh", f64::sinh);
    unary!("cosh", f64::cosh);
    unary!("tanh", f64::tanh);
    unary!("asinh", f64::asinh);
    unary!("acosh", f64::acosh);
    // fdlibm atanh — the platform libm loses ~400 ulp near |x| = 1; log1p keeps it exact.
    unary!("atanh", |x: f64| {
        let ax = x.abs();
        if ax > 1.0 {
            return f64::NAN;
        }
        let t = if ax < 0.5 {
            let t = ax + ax;
            0.5 * (t + t * ax / (1.0 - ax)).ln_1p()
        } else {
            0.5 * ((ax + ax) / (1.0 - ax)).ln_1p()
        };
        if x < 0.0 {
            -t
        } else if x == 0.0 {
            x
        } else {
            t
        }
    });
    unary!("fround", |x: f64| x as f32 as f64);
    unary!(
        "f16round",
        |x: f64| crate::value::f16_to_f32(crate::value::f64_to_f16(x)) as f64
    );
    unary!("clz32", |x: f64| (to_uint32(x)).leading_zeros() as f64);
    it.def_method(&math, "sumPrecise", 1, |i, _t, a| {
        // Iterate the argument, requiring every element to be a Number; compute the correctly
        // rounded sum. Infinities dominate (mixed signs → NaN), any NaN → NaN, empty → -0.
        let (iter, next) = ab(i.get_iterator(&arg(a, 0)))?;
        let mut finite: Vec<f64> = Vec::new();
        let (mut pos_inf, mut neg_inf, mut nan) = (false, false, false);
        loop {
            let item = match ab(i.iterator_step(&iter, &next))? {
                Some(v) => v,
                None => break,
            };
            match item {
                Value::Num(n) => {
                    if n.is_nan() {
                        nan = true;
                    } else if n.is_infinite() {
                        if n > 0.0 {
                            pos_inf = true;
                        } else {
                            neg_inf = true;
                        }
                    } else {
                        finite.push(n);
                    }
                }
                _ => {
                    i.iterator_close(&iter);
                    return Err(i.make_error("TypeError", "Math.sumPrecise: not a number"));
                }
            }
        }
        let result = if pos_inf && neg_inf || nan {
            f64::NAN
        } else if pos_inf {
            f64::INFINITY
        } else if neg_inf {
            f64::NEG_INFINITY
        } else if finite.is_empty() {
            -0.0
        } else {
            let s = fsum_exact(&finite);
            if s.is_finite() {
                s
            } else {
                // The exact-summation partials transiently overflowed. Retry on values scaled by a
                // power of two (exact, so the correctly-rounded result is unchanged) centred near
                // 2^500, then scale back — a genuine overflow re-materialises as ±Infinity.
                let max_abs = finite.iter().map(|x| x.abs()).fold(0.0_f64, f64::max);
                let scale_exp = max_abs.log2().floor() as i32 - 500;
                let down = 2f64.powi(-scale_exp);
                let up = 2f64.powi(scale_exp);
                let scaled: Vec<f64> = finite.iter().map(|&x| x * down).collect();
                fsum_exact(&scaled) * up
            }
        };
        Ok(Value::Num(result))
    });
    it.def_method(&math, "hypot", 2, |i, _t, a| {
        // Coerce every argument (in order), then: any infinite operand yields +Infinity (even
        // alongside a NaN), otherwise any NaN yields NaN, otherwise the Euclidean norm.
        let mut sum = 0.0;
        let mut any_inf = false;
        let mut any_nan = false;
        for v in a {
            let n = ab(i.to_number(v))?;
            if n.is_infinite() {
                any_inf = true;
            } else if n.is_nan() {
                any_nan = true;
            } else {
                sum += n * n;
            }
        }
        Ok(Value::Num(if any_inf {
            f64::INFINITY
        } else if any_nan {
            f64::NAN
        } else {
            sum.sqrt()
        }))
    });
    it.def_method(&math, "imul", 2, |i, _t, a| {
        let x = to_uint32(ab(i.to_number(&arg(a, 0)))?) as i32;
        let y = to_uint32(ab(i.to_number(&arg(a, 1)))?) as i32;
        Ok(Value::Num(x.wrapping_mul(y) as f64))
    });
    it.def_method(&math, "random", 0, |_i, _t, _a| {
        Ok(Value::Num(next_random()))
    });
    unary!("log", f64::ln);
    unary!("log2", f64::log2);
    unary!("log10", f64::log10);
    unary!("exp", f64::exp);
    unary!("sin", f64::sin);
    unary!("cos", f64::cos);
    unary!("tan", f64::tan);
    unary!("atan", f64::atan);
    unary!("asin", f64::asin);
    unary!("acos", f64::acos);
    it.def_method(&math, "pow", 2, |i, _t, a| {
        let base = ab(i.to_number(&arg(a, 0)))?;
        let exp = ab(i.to_number(&arg(a, 1)))?;
        // Number::exponentiate special cases Rust's powf doesn't share: a NaN exponent is NaN even
        // for base 1, and a base of ±1 with an infinite exponent is NaN.
        let r = if exp.is_nan() || (base.abs() == 1.0 && exp.is_infinite()) {
            f64::NAN
        } else {
            base.powf(exp)
        };
        Ok(Value::Num(r))
    });
    it.def_method(&math, "atan2", 2, |i, _t, a| {
        Ok(Value::Num(
            ab(i.to_number(&arg(a, 0)))?.atan2(ab(i.to_number(&arg(a, 1)))?),
        ))
    });
    it.def_method(&math, "max", 2, |i, _t, a| {
        // ToNumber every argument first (side effects in order), then reduce. +0 is larger than -0.
        let mut nums = Vec::with_capacity(a.len());
        for v in a {
            nums.push(ab(i.to_number(v))?);
        }
        let mut m = f64::NEG_INFINITY;
        for &n in &nums {
            if n.is_nan() {
                return Ok(Value::Num(f64::NAN));
            }
            if n > m || (n == 0.0 && m == 0.0 && n.is_sign_positive() && m.is_sign_negative()) {
                m = n;
            }
        }
        Ok(Value::Num(m))
    });
    it.def_method(&math, "min", 2, |i, _t, a| {
        let mut nums = Vec::with_capacity(a.len());
        for v in a {
            nums.push(ab(i.to_number(v))?);
        }
        let mut m = f64::INFINITY;
        for &n in &nums {
            if n.is_nan() {
                return Ok(Value::Num(f64::NAN));
            }
            if n < m || (n == 0.0 && m == 0.0 && n.is_sign_negative() && m.is_sign_positive()) {
                m = n;
            }
        }
        Ok(Value::Num(m))
    });
    set_to_string_tag(it, &math, "Math");
    set_builtin(&it.global, "Math", Value::Obj(math));
}

/// Correctly-rounded sum of finite f64s, via Shewchuk's nonoverlapping-partials algorithm with
/// CPython's final round-half-to-even step (the `math.fsum` algorithm).
fn fsum_exact(values: &[f64]) -> f64 {
    let mut partials: Vec<f64> = Vec::new();
    for &xi in values {
        let mut x = xi;
        let mut i = 0;
        for j in 0..partials.len() {
            let mut y = partials[j];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                partials[i] = lo;
                i += 1;
            }
            x = hi;
        }
        partials.truncate(i);
        partials.push(x);
    }
    let n = partials.len();
    if n == 0 {
        return 0.0;
    }
    let mut hi = partials[n - 1];
    let mut lo = 0.0;
    let mut idx = n - 1;
    while idx > 0 {
        idx -= 1;
        let x = hi;
        let y = partials[idx];
        hi = x + y;
        lo = y - (hi - x);
        if lo != 0.0 {
            break;
        }
    }
    // Round half to even: nudge when the residual and the next partial agree in sign.
    if idx > 0 && ((lo < 0.0 && partials[idx - 1] < 0.0) || (lo > 0.0 && partials[idx - 1] > 0.0)) {
        let y = lo * 2.0;
        let x = hi + y;
        if y == x - hi {
            hi = x;
        }
    }
    hi
}
