//! String.prototype.substring with direct byte slicing for known ASCII inputs.
use super::{ab, arg, this_string};
use crate::{
    interpreter::Interp,
    value::{Gc, Value},
};

pub(super) fn install(i: &mut Interp, prototype: &Gc) {
    // Select once per realm: no getenv or cache lookup in the substring hot path.
    let function: crate::value::NativeFn = if std::env::var_os("LUMEN_NO_ASCII_SUBSTRING").is_none()
    {
        substring::<true>
    } else {
        substring::<false>
    };
    i.def_method(prototype, "substring", 2, function);
}

fn substring<const ASCII: bool>(
    i: &mut Interp,
    this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let s = this_string(i, &this)?;
    // A clear hint is inconclusive, so use the existing complete UTF-16 path then.
    // Determine length before argument coercion, retaining the original receiver string.
    let chars = if ASCII && s.ascii_hint() {
        None
    } else {
        Some(i.units_full(&s))
    };
    let len = chars.as_ref().map_or(s.len(), |chars| chars.len()) as i64;
    let mut a = (ab(i.to_number(&arg(args, 0)))? as i64).clamp(0, len);
    let mut b = match arg(args, 1) {
        Value::Undefined => len,
        v => (ab(i.to_number(&v))? as i64).clamp(0, len),
    };
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    match chars {
        None => {
            #[cfg(test)]
            ASCII_CALLS.with(|calls| calls.set(calls.get() + 1));
            Ok(Value::str(&s[a as usize..b as usize]))
        }
        Some(chars) => {
            #[cfg(test)]
            UTF16_CALLS.with(|calls| calls.set(calls.get() + 1));
            Ok(Value::from_string(crate::jstr::from_units(
                &chars[a as usize..b as usize],
            )))
        }
    }
}

#[cfg(test)]
thread_local! {
    static ASCII_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static UTF16_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, ascii: bool) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::ASCII_CALLS.with(|n| n.set(0));
            super::UTF16_CALLS.with(|n| n.set(0));
            let script=format!("function assert(v){{if(!v)throw new Error('substring');}} function sub(s,a,b){{return s.substring(a,b);}} {source}");
            match engine.eval(&script, false).unwrap() {
                Completion::Value(_) => {}
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if ascii {
                assert!(
                    super::ASCII_CALLS.with(|n| n.get()) > 0,
                    "ASCII path unused"
                );
            } else {
                assert!(
                    super::UTF16_CALLS.with(|n| n.get()) > 0,
                    "UTF16 fallback unused"
                );
            }
        }
    }

    #[test]
    fn ascii_ranges_clamp_swap_and_truncate() {
        check(
            r#"
            for(var k=0;k<100;k++){
                assert(sub('abcdef',4,1)==='bcd');
                assert(sub('abcdef',-Infinity,Infinity)==='abcdef');
                assert(sub('abcdef',Infinity,2)==='cdef');
                assert(sub('abcdef',NaN,3)==='abc');
                assert(sub('abcdef',-0,2)==='ab');
                assert(sub('abcdef',-2,2.9)==='ab');
                assert(sub('abcdef',2.9,undefined)==='cdef');
                assert(sub('abcdef',3,NaN)==='abc');
                assert(sub('abcdef',4,4)==='');assert(sub('',0,99)==='');
                assert(sub('abcdef',undefined,undefined)==='abcdef');
            }
            var long='abcdefghijklmnopqrstuvwxyz'.repeat(8);
            assert(sub(long,26,52)==='abcdefghijklmnopqrstuvwxyz');
        "#,
            true,
        );
    }

    #[test]
    fn utf16_surrogate_halves_and_lone_surrogates_remain_exact() {
        check(
            r#"
            var s='A\uD83D\uDE00B\uD800C\uDC00';
            assert(sub(s,1,2)==='\uD83D');assert(sub(s,2,3)==='\uDE00');
            assert(sub(s,1,3)==='\uD83D\uDE00');assert(sub(s,3,1)==='\uD83D\uDE00');
            assert(sub(s,4,5)==='\uD800');assert(sub(s,6,7)==='\uDC00');
            assert(sub('é中',1,2)==='中');
        "#,
            false,
        );
    }

    #[test]
    fn coercions_keep_receiver_then_start_then_end_and_short_circuit_errors() {
        check(
            r#"
            var events='',source='abcdef';
            var receiver={toString(){events+='S';return source;}};
            var start={valueOf(){events+='A';source='changed';$262.gc();return 4;}};
            var end={valueOf(){events+='B';return 1;}};
            assert(String.prototype.substring.call(receiver,start,end)==='bcd');
            assert(events==='SAB');
            events='';var marker={};var caught=false;
            try{String.prototype.substring.call(receiver,{valueOf(){events+='A';throw marker;}},end);}
            catch(e){caught=e===marker;}
            assert(caught&&events==='SA');
            events='';caught=false;
            try{String.prototype.substring.call(null,start,end);}catch(e){caught=e instanceof TypeError;}
            assert(caught&&events==='');
            caught=false;try{sub('abc',Symbol(),2);}catch(e){caught=e instanceof TypeError;}assert(caught);
        "#,
            true,
        );
    }
}
