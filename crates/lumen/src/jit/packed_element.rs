//! Select a live packed Property address without changing VM ownership.
use super::{asm::Asm, C_HS};
use crate::value::JitLayout;
use std::sync::OnceLock;

#[derive(Clone, Copy)]
pub(super) enum Site {
    Stack,
    Local,
}

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("LUMEN_JIT_NO_INLINE_PACKED_READS").is_none())
}

fn supported(layout: &JitLayout) -> bool {
    super::packed_elem_inlinable(layout)
        && layout.dense_inline_len < 4096
        && layout.dense_inline_slots < 4096
}

/// x12=dense buffers, x9=unsigned index -> x15=Property. Clobbers x14/x15 only.
/// Preserves x12 and x9 on the classic edge; callers retain descriptor, hole,
/// value decoding and ownership checks. No pointer survives a runtime helper.
pub(super) fn address(a: &mut Asm, layout: &JitLayout, classic: usize, fail: usize, _site: Site) {
    let inline = enabled() && supported(layout);
    let unpacked = if inline { a.new_label() } else { classic };
    let done = a.new_label();
    a.ldr_imm(15, 12, layout.dense_packed as u32);
    a.cbz(15, true, unpacked);
    a.ldr_imm(14, 15, layout.vec_len_off as u32);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_HS, fail);
    a.ldr_imm(15, 15, layout.vec_ptr_off as u32);
    a.add_shifted(15, 15, 9, 4);
    if inline {
        a.b(done);
        a.bind(unpacked);
        a.ldrb_imm(14, 12, layout.dense_inline_len as u32);
        a.cbz(14, false, classic);
        a.cmp_reg_x(9, 14);
        a.b_cond(C_HS, fail);
        #[cfg(test)]
        {
            let address = match _site {
                Site::Stack => STACK_ADDRESSES.with(|count| count.as_ptr() as u64),
                Site::Local => LOCAL_ADDRESSES.with(|count| count.as_ptr() as u64),
            };
            a.mov_imm64(14, address);
            a.ldr_imm(15, 14, 0);
            a.add_imm(15, 15, 1);
            a.str_imm(15, 14, 0);
        }
        a.add_imm(15, 12, layout.dense_inline_slots as u32);
        a.add_shifted(15, 15, 9, 4);
    }
    a.bind(done);
}

#[cfg(test)]
thread_local! {
    static STACK_ADDRESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LOCAL_ADDRESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::{enabled, LOCAL_ADDRESSES, STACK_ADDRESSES};
    use crate::{bytecode::Tier, Completion, Engine};

    #[test]
    fn storage_boundaries_and_scalar_and_alias_values_survive_reads() {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let source = r#"
                function assert(v){if(!v)throw new Error('packed boundaries');}
                function read(a,k){return a[k];}
                var lengths=[1,10,11,32];
                for(var q=0;q<lengths.length;q++){
                    var n=lengths[q],s='x';
                    for(var j=1;j<n;j++)s+=',x';
                    var a=s.split(',');assert(a.length===n);
                    for(var k=0;k<100;k++)assert(read(a,0)==='x'&&read(a,n-1)==='x');
                    assert(read(a,n)===undefined);
                    Object.defineProperty(a,'0',{value:NaN,writable:false});
                    var got=read(a,0);assert(got!==got);
                    a[0]=17;got=read(a,0);assert(got!==got);
                    a[n-1]=a;var held=read(a,n-1);
                    if(n>1){a=null;$262.gc();assert(held[n-1]===held);}
                    else assert(held!==held);
                }
                var big=[123n];for(var k=0;k<100;k++)assert(read(big,0)===123n);
                'passed';
            "#;
            match engine.eval(source, false).unwrap() {
                Completion::Value(value) => assert_eq!(value, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
        }
    }

    #[test]
    fn builtin_capture_reads_use_inline_addresses_and_preserve_fallbacks() {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            STACK_ADDRESSES.with(|count| count.set(0));
            LOCAL_ADDRESSES.with(|count| count.set(0));
            let source = r#"
                function assert(v){if(!v)throw new Error('inline packed read');}
                function read(a,k){return a[k];}
                function invoke(a,k){return read(a,k);}
                function pair(m){return m.indices[0][0]+m.indices[0][1];}
                var m=/(a)(b)?/d.exec('za');
                for(var k=0;k<200;k++){
                    assert(pair(m)===3&&invoke(m,1)==='a'&&invoke(m,2)===undefined);
                }
                var p=m.indices[0];
                assert(invoke(p,-1)===undefined&&invoke(p,1.5)===undefined);
                assert(invoke(p,NaN)===undefined&&invoke(p,Infinity)===undefined);
                var gets=0,proto=Object.create(Array.prototype);
                Object.defineProperty(proto,'0',{get:function(){gets++;return 7;}});
                Object.setPrototypeOf(p,proto);delete p[0];
                assert(invoke(p,0)===7&&gets===1);
                Object.defineProperty(p,'0',{get:function(){gets++;$262.gc();return 9;},configurable:true});
                assert(invoke(p,0)===9&&gets===2);
                Object.defineProperty(p,'0',{value:11,writable:true,configurable:true});
                assert(invoke(p,0)===11);p.length=0;assert(invoke(p,1)===undefined);
                Object.setPrototypeOf(p,Array.prototype);
                p.push(21,22);assert(invoke(p,1)===22);
                var child={n:33},entries=Object.entries({a:child,b:child});
                var one=invoke(entries,0),two=invoke(entries,1);
                var held=invoke(one,1);entries=null;one=null;$262.gc();
                assert(held===invoke(two,1)&&held.n===33);
                var proxy=new Proxy([4],{get:function(t,k){return k==='0'?8:t[k];}});
                assert(invoke(proxy,0)===8);
                'passed';
            "#;
            match engine.eval(source, false).unwrap() {
                Completion::Value(value) => assert_eq!(value, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            let stack_hits = STACK_ADDRESSES.with(|count| count.get());
            let local_hits = LOCAL_ADDRESSES.with(|count| count.get());
            let hits = stack_hits + local_hits;
            if !enabled() || !matches!(tier, Tier::Jit) {
                assert_eq!(hits, 0);
            } else if std::env::var_os("LUMEN_NO_COMPACT_BUILTIN_ARRAYS").is_none() {
                assert!(
                    stack_hits > 0 && local_hits > 0,
                    "missing emitter coverage: stack={stack_hits}, local={local_hits}"
                );
            }
        }
    }
}
