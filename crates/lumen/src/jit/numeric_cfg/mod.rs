//! Register-resident numeric loops with general forward branches and multiple backedges.
mod emit;
mod plan;
use super::asm::Asm;
use crate::{bytecode::Chunk, jit_ir::Cfg};

pub(super) fn try_emit(
    a: &mut Asm,
    chunk: &Chunk,
    cfg: &Cfg,
    head: usize,
    labels: &[usize],
    targeted: &mut [bool],
) -> bool {
    if super::PACKED_LOCAL_SLOTS || std::env::var_os("LUMEN_JIT_NO_NUMERIC_CFG").is_some() {
        return false;
    }
    let Some(plan) = plan::build(chunk, cfg, head) else {
        return false;
    };
    let plain = emit::emit(a, &plan, labels);
    a.bind(plain);
    for block in &plan.blocks {
        for flag in &mut targeted[block.start..block.end] {
            *flag = true;
        }
    }
    if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
        eprintln!(
            "[jit-region] head {head}: EMITTED numeric CFG ({} blocks, {} locals)",
            plan.blocks.len(),
            plan.locals.len()
        );
    }
    true
}

#[cfg(test)]
thread_local! { static ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
fn record_entry(a: &mut Asm) {
    // Engine/Chunk are Rc-based and cannot move between threads. This code and its TLS cell
    // therefore share a lifetime and thread; the counter introduces no shared-memory race.
    let ptr = ENTRIES.with(|entries| entries.as_ptr() as usize);
    a.mov_imm64(9, ptr as u64);
    a.ldr_imm(10, 9, 0);
    a.add_imm(10, 10, 1);
    a.str_imm(10, 9, 0);
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, enters: bool) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::ENTRIES.with(|n| n.set(0));
            let script = format!(
                "function assert(v){{if(!v)throw new Error('numeric CFG');}} {source}; 'passed'"
            );
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if matches!(tier, Tier::Jit) {
                assert_eq!(super::ENTRIES.with(|n| n.get() > 0), enters);
            }
        }
    }

    #[test]
    fn branches_continues_and_budget_exits_restore_locals() {
        check(
            r#"
            function calc(n) {
                var total=0;
                for(var i=0;i<n;i++) {
                    if(i<8) {total+=2;continue;}
                    if(i>3000) break;
                    if(i<1000) total-=1; else total+=3;
                }
                return total*10000+i;
            }
            assert(calc(5000)===50273001);
            assert(calc(4)===80004);
        "#,
            true,
        );
    }

    #[test]
    fn unordered_comparisons_and_signed_zero_are_preserved() {
        check(
            r#"
            function calc(n,x) {
                var s=0;
                for(var i=0;i<n;i++) {
                    if(x<0)s+=1;else s+=2;
                    if(x>=0)s+=4;
                    if(x!==x)s+=8;
                }
                return s;
            }
            assert(calc(3,NaN)===30);
            assert(calc(3,1)===18);
            assert(calc(3,-1)===3);
            assert(calc(3,Infinity)===18);
            assert(calc(3,-Infinity)===3);
            function zero(n,x) {
                for(var i=0;i<n;i++) {if(i<2)x=-x;else x=x/1;}
                return x;
            }
            assert(Object.is(zero(2000,-0),-0));
        "#,
            true,
        );
    }

    #[test]
    fn guard_misses_preserve_coercion_and_bigint_updates() {
        check(
            r#"
            function acc(n,sum) {
                for(var i=0;i<n;i++) {if(i<4)sum++;else sum--;}
                return sum;
            }
            assert(acc('8',0)===0);
            var calls=0;
            assert(acc({valueOf(){calls++;return 8;}},0)===0);
            assert(calls===9);
            assert(acc(8,0n)===0n);
        "#,
            false,
        );
    }

    #[test]
    fn effectful_loops_and_handlers_use_the_existing_path() {
        check(
            r#"
            var calls=0;
            function effect(){calls++;return 2;}
            function calc(n) {
                var sum=0;
                for(var i=0;i<n;i++) {if(i<4)sum+=effect();else sum--;}
                return sum;
            }
            assert(calc(8)===4 && calls===4);
            function handled(n) {
                var sum=0;
                try {for(var i=0;i<n;i++){if(i>3)throw 7;else sum++;}}
                catch(e){sum+=e;}finally{sum*=2;}
                return sum;
            }
            assert(handled(8)===22);
        "#,
            false,
        );
    }
}
