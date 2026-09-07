//! Guarded numeric comparison branches without an intermediate shadow Boolean.
use super::{plan::Plan, shadow};
use crate::{
    bytecode::Op,
    jit::{asm::Asm, C_EQ, C_GE, C_GT, C_LS, C_MI, C_NE},
    jit_ir::Cfg,
};

pub(super) struct Pair {
    pub branch: usize,
    pub yes: usize,
    pub no: usize,
    condition: u32,
    depth: usize,
}

pub(super) fn select(ops: &[Op], cfg: &Cfg, plan: &Plan, pc: usize) -> Option<Pair> {
    let branch = pc.checked_add(1)?;
    let yes = pc.checked_add(2)?;
    let Op::JumpIfFalse(no) = *ops.get(branch)? else {
        return None;
    };
    let no = no as usize;
    let condition = match *ops.get(pc)? {
        Op::EqEq | Op::StrictEq => C_EQ,
        Op::NotEq | Op::StrictNotEq => C_NE,
        Op::Lt => C_MI,
        Op::Le => C_LS,
        Op::Gt => C_GT,
        Op::Ge => C_GE,
        _ => return None,
    };
    let depth = cfg.stack_depth_at(pc)?;
    let block = cfg.block_at(pc)?;
    // All independent branch targets are CFG leaders. Staying in this same block
    // proves nobody can enter the consumed JumpIfFalse with a separately produced Boolean.
    if cfg.block_at(branch) != Some(block) {
        #[cfg(test)]
        INDEPENDENT_ENTRIES.with(|n| n.set(n.get() + 1));
        return None;
    }
    if !plan.pcs.contains(&branch)
        || no <= branch
        || depth < 2
        || cfg.stack_depth_at(branch)? != depth - 1
        || cfg.stack_depth_at(yes)? != depth - 2
        || cfg.stack_depth_at(no)? != depth - 2
    {
        return None;
    }
    Some(Pair {
        branch,
        yes,
        no,
        condition,
        depth,
    })
}

pub(super) fn emit(a: &mut Asm, plan: &Plan, pair: &Pair, fail: usize, yes: usize, no: usize) {
    #[cfg(test)]
    let number_fail = a.new_label();
    #[cfg(not(test))]
    let number_fail = fail;
    shadow::number(a, plan.stack(pair.depth - 2), 16, number_fail);
    shadow::number(a, plan.stack(pair.depth - 1), 17, number_fail);
    #[cfg(test)]
    {
        a.mov_imm64(9, SUCCESSES.with(|n| n.as_ptr() as usize) as u64);
        a.ldr_imm(10, 9, 0);
        a.add_imm(10, 10, 1);
        a.str_imm(10, 9, 0);
    }
    a.fcmp(16, 17);
    a.b_cond(pair.condition, yes);
    a.b(no);
    #[cfg(test)]
    {
        a.bind(number_fail);
        a.mov_imm64(9, GUARD_FAILURES.with(|n| n.as_ptr() as usize) as u64);
        a.ldr_imm(10, 9, 0);
        a.add_imm(10, 10, 1);
        a.str_imm(10, 9, 0);
        a.b(fail);
    }
}

#[cfg(test)]
thread_local! {
    static SUCCESSES: std::cell::Cell<usize> = const {std::cell::Cell::new(0)};
    static INDEPENDENT_ENTRIES: std::cell::Cell<usize> = const {std::cell::Cell::new(0)};
    static GUARD_FAILURES: std::cell::Cell<usize> = const {std::cell::Cell::new(0)};
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    #[test]
    fn independently_targeted_branch_preserves_ternary_boolean_input() {
        check(
            r#"
            function scan(a,pick,other,x,y){var sum=0,c;for(var i=0;i<a.length;i++){
                c=a[i];c.value=c.value+1;
                if(pick ? other : x<y)sum+=1;
            }return sum;}
            var a=[];for(var k=0;k<40;k++)a.push({value:0});
            assert(scan(a,true,true,2,1)===40);
            assert(scan(a,true,false,1,2)===0);
            assert(scan(a,false,false,1,2)===40);
            assert(scan(a,false,true,2,1)===0);
            assert(a[0].value===4&&a[39].value===4);
        "#,
        );
        assert!(
            super::INDEPENDENT_ENTRIES.with(|n| n.get()) > 0,
            "no real independently targeted comparison branch rejected"
        );
    }

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::SUCCESSES.with(|n| n.set(0));
            super::INDEPENDENT_ENTRIES.with(|n| n.set(0));
            super::GUARD_FAILURES.with(|n| n.set(0));
            let script =
                format!("function assert(v){{if(!v)throw new Error('compare branch');}} {source}");
            match engine.eval(&script, false).unwrap() {
                Completion::Value(_) => {}
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if tier == Tier::Jit {
                assert!(
                    super::SUCCESSES.with(|n| n.get()) > 0,
                    "no fused comparison executed"
                );
            }
        }
    }

    #[test]
    fn nan_signed_zero_and_infinities_keep_all_comparison_results() {
        check(
            r#"
            function scan(a,x,y){var sum=0,c;for(var i=0;i<a.length;i++){
                c=a[i];c.value=c.value+1;
                if(x<y)sum+=1;if(x<=y)sum+=2;if(x>y)sum+=4;if(x>=y)sum+=8;
                if(x==y)sum+=16;if(x!=y)sum+=32;if(x===y)sum+=64;if(x!==y)sum+=128;
            }return sum;}
            var a=[];for(var k=0;k<40;k++)a.push({value:0});
            assert(scan(a,NaN,1)===160*40);assert(scan(a,1,NaN)===160*40);
            assert(scan(a,-0,0)===90*40);assert(scan(a,-Infinity,Infinity)===163*40);
            assert(scan(a,Infinity,-Infinity)===172*40);
        "#,
        );
    }

    #[test]
    fn coercion_after_heap_write_resumes_at_comparison_without_replay() {
        check(
            r#"
            function scan(a){var sum=0,c;for(var i=0;i<a.length;i++){
                c=a[i];c.value=c.value+1;if(c.limit<10)sum+=c.value;
            }return sum;}
            var a=[];for(var k=0;k<40;k++)a.push({value:0,limit:1});
            assert(scan(a)===40);
            var calls=0;a[10].limit={valueOf(){calls++;assert(a[10].value===2);$262.gc();return 1;}};
            assert(scan(a)===80&&calls===1&&a[10].value===2);
        "#,
        );
        assert!(
            super::GUARD_FAILURES.with(|n| n.get()) > 0,
            "no fused comparison guard failed after numeric property read"
        );
    }
}
