//! Local stores and adjacent last-use loads with no observable slot lifetime between them.
use super::{asm::Asm, emit_exec, C_EQ, C_HI, C_LO, C_LS, H_DROP_AT};
use crate::bytecode::Op;
use crate::value::JitLayout;

pub(super) fn can_forward(ops: &[Op], pc: usize, last: bool, targeted: bool) -> bool {
    last && !targeted
        && matches!(ops.get(pc..pc + 2), Some([Op::StoreLocal(a), Op::LoadLocal(b)]) if a == b)
        && std::env::var_os("LUMEN_JIT_NO_STORE_LOAD_FORWARD").is_none()
}

pub(super) fn emit(
    a: &mut Asm,
    slot: u16,
    pc: usize,
    layout: &JitLayout,
    unwind: usize,
    continuation: Option<usize>,
) {
    let off = slot as u32 * 16;
    let slow = a.new_label();
    let done = a.new_label();
    a.ldrb_imm(9, 22, off);
    if layout.valid && layout.rc_strong_off < 256 {
        drop_previous(a, off, layout.rc_strong_off as i32);
        if let Some(continuation) = continuation {
            let ordinary = a.new_label();
            // An internal Empty value must still be stored then throw at LoadLocal.
            // Keep that load emitted and take the original path for this sentinel.
            a.ldurb(9, 20, -16);
            a.cmp_imm_w(9, 1);
            a.b_cond(C_EQ, ordinary);
            // LastUses proved the slot dead after the load. Retain its new owner on
            // the operand stack and give frame cleanup exactly the normal moved state.
            a.strb_imm(31, 22, off);
            #[cfg(test)]
            record_success(a);
            a.b(continuation);
            a.bind(ordinary);
        }
    } else {
        a.cmp_imm_w(9, 4);
        a.b_cond(C_HI, slow);
    }
    a.ldur(9, 20, -16);
    a.ldur(10, 20, -8);
    a.str_imm(9, 22, off);
    a.str_imm(10, 22, off + 8);
    a.sub_imm(20, 20, 16);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc as u32, unwind);
    a.bind(done);
}

fn drop_previous(a: &mut Asm, off: u32, rc_strong: i32) {
    let drop_old = a.new_label();
    let done = a.new_label();
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, drop_old);
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, done);
    a.ldr_imm(10, 22, off + 8);
    a.ldur(9, 10, rc_strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, drop_old);
    a.sub_imm(9, 9, 1);
    a.stur(9, 10, rc_strong);
    a.b(done);
    // StoreLocal cannot throw. Preserve its real-destructor path for the old
    // slot's final references and BigInts before transferring the new owner.
    a.bind(drop_old);
    a.mov(0, 19);
    a.movz(1, 0, 0);
    a.add_imm(2, 22, off);
    a.ldr_imm(16, 21, (H_DROP_AT * 8) as u32);
    a.blr(16);
    a.bind(done);
}

#[cfg(test)]
thread_local! {
    static SUCCESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_success(a: &mut Asm) {
    a.mov_imm64(9, SUCCESSES.with(|n| n.as_ptr() as usize) as u64);
    a.ldr_imm(10, 9, 0);
    a.add_imm(10, 10, 1);
    a.str_imm(10, 9, 0);
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::SUCCESSES.with(|n| n.set(0));
            let script = format!(
                "function assert(v){{if(!v)throw new Error('store forwarding');}} {source}; 'passed'"
            );
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if tier == Tier::Jit {
                assert!(
                    super::SUCCESSES.with(|n| n.get()) > 0,
                    "fusion not exercised"
                );
            }
        }
    }

    #[test]
    fn values_and_overwritten_owners_survive_last_use_forwarding() {
        check(
            r#"
            function take(value){var local=value;return local;}
            function overwrite(old,value){old=value;return old;}
            const values=[undefined,null,false,true,-0,NaN,Infinity,1.5,1n<<100n,'text',Symbol('s'),{}];
            for(let i=0;i<600;i++){
                const value=values[i%values.length];
                assert(Object.is(take(value),value));
                assert(Object.is(overwrite({nested:{x:i}},value),value));
                assert(Object.is(overwrite(1n<<150n,value),value));
            }
            function loop(n){var result;for(let i=0;i<n;i++){var item={x:i};result=item;}return result.x;}
            assert(loop(1000)===999);
        "#,
        );
    }

    #[test]
    fn live_successors_captures_and_handlers_preserve_local_values() {
        check(
            r#"
            function take(value){var local=value;return local;}
            for(let i=0;i<600;i++)assert(take(i)===i);
            function branch(v,b){var x=v;if(b)return x;return x;}
            const value={};assert(branch(value,true)===value && branch(value,false)===value);
            function capture(v){var x=v;var read=function(){return x;};var y=x;x={};return [y,read()];}
            const pair=capture(value);assert(pair[0]===value && pair[1]!==value);
            function caught(v){var x=v;try{var y=x;throw 1;}catch(e){return x;}}
            assert(caught(value)===value);
            function tdz(){var x=y;let y=3;return x;}
            let threw=false;try{tdz();}catch(e){threw=e instanceof ReferenceError;}assert(threw);
        "#,
        );
    }

    #[test]
    fn forwarding_requires_a_dead_same_slot_and_unshared_load_entry() {
        use crate::bytecode::Op;
        let pair = [Op::StoreLocal(2), Op::LoadLocal(2)];
        assert!(!super::can_forward(&pair, 0, false, false));
        assert!(!super::can_forward(&pair, 0, true, true));
        assert!(!super::can_forward(
            &[Op::StoreLocal(2), Op::LoadLocal(3)],
            0,
            true,
            false
        ));
    }
}
