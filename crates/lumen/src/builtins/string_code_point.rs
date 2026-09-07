//! Full codePointAt semantics, also used when the ASCII call intrinsic declines.
use super::{ab, arg, this_string};
use crate::{interpreter::Interp, value::Value};

pub(crate) fn nf_code_point_at(
    i: &mut Interp,
    this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    #[cfg(test)]
    NATIVE_CALLS.with(|calls| calls.set(calls.get() + 1));
    let s = this_string(i, &this)?;
    let n = ab(i.to_number(&arg(args, 0)))?;
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    if n < 0.0 || !n.is_finite() {
        return Ok(Value::Undefined);
    }
    let idx = n as usize;
    Ok(match i.unit_at(&s, idx) {
        Some(u) if (0xD800..0xDC00).contains(&u) => match i.unit_at(&s, idx + 1) {
            Some(lo) if (0xDC00..0xE000).contains(&lo) => {
                let c = 0x10000 + ((u as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00);
                Value::Num(c as f64)
            }
            _ => Value::Num(u as f64),
        },
        Some(u) => Value::Num(u as f64),
        None => Value::Undefined,
    })
}

#[cfg(test)]
thread_local! {
    static NATIVE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, ascii_only: bool) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::NATIVE_CALLS.with(|calls| calls.set(0));
            let script = format!("function assert(x){{if(!x)throw new Error('code point assertion');}} function point(s,n){{return s.codePointAt(n);}} {source}; 'passed'");
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            #[cfg(all(
                target_arch = "aarch64",
                any(target_os = "macos", target_os = "linux", target_os = "windows")
            ))]
            if ascii_only && matches!(tier, Tier::Jit) {
                let calls = super::NATIVE_CALLS.with(|calls| calls.get());
                assert!(
                    calls < 20,
                    "ASCII reads must bypass native dispatch: {calls}"
                );
            }
            let _ = ascii_only;
        }
    }

    #[test]
    fn ascii_reads_reach_the_native_byte_load() {
        check(
            r#"
            const text='ASCII text';
            for(let i=0;i<1000;i++)assert(point(text,i%text.length)===text.charCodeAt(i%text.length));
        "#,
            true,
        );
    }

    #[test]
    fn warmed_calls_preserve_unicode_indices_and_distinct_out_of_bounds_results() {
        check(
            r#"
            const text='ASCII';for(let i=0;i<1000;i++)assert(point(text,0)===65);
            const cases=[['',0,undefined],['A',1,undefined],['A',-1,undefined],
                ['A',Infinity,undefined],['A',-Infinity,undefined],['AB',1.9,66],
                ['A',-0.5,65],['A',NaN,65],['A',undefined,65],['AB','1',66],
                ['é',0,233],['😀',0,128512],['😀',1,56832],['😀',2,undefined],
                ['\ud800A',0,55296],['\udc00',0,56320],['A',4294967296,undefined]];
            for(const c of cases)assert(Object.is(point(c[0],c[1]),c[2]));
            assert(Number.isNaN('A'.charCodeAt(1)) && point('A',1)===undefined);
            for(let i=0;i<1000;i++)assert(point(text,0)===65);
        "#,
            false,
        );
    }

    #[test]
    fn coercion_overrides_and_foreign_builtins_keep_their_semantics() {
        check(
            r#"
            const text='ABC';for(let i=0;i<1000;i++)assert(point(text,0)===65);
            const original=String.prototype.codePointAt;
            String.prototype.codePointAt=function(n){return n+900;};assert(point(text,2)===902);
            String.prototype.codePointAt=original;
            const log=[];
            const receiver={codePointAt:original,toString(){log.push('s');return '😀';}};
            const index={valueOf(){log.push('i');return 0;}};
            assert(point(receiver,index)===128512 && log.join('')==='si');
            let rejected=false;try{point(text,1n);}catch(e){rejected=e instanceof TypeError;}assert(rejected);
            const realm=$262.createRealm();
            String.prototype.codePointAt=realm.global.String.prototype.codePointAt;
            for(let i=0;i<1000;i++)assert(point(text,1)===66);
            rejected=false;try{point(text,1n);}catch(e){rejected=e instanceof realm.global.TypeError;}assert(rejected);
            String.prototype.codePointAt=original;assert(point(text,2)===67);
        "#,
            false,
        );
    }
}
