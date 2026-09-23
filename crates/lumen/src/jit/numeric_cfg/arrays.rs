//! Borrow read-only numeric mirrors while their owning local slots remain live.
use super::super::{asm, C_EQ, C_HS, C_NE};
use super::plan::Plan;
use crate::value::{JitLayout, MIRROR_NO_HOLES, MIRROR_OK};

pub(super) fn supported(layout: &JitLayout) -> bool {
    super::super::get_elem_inlinable(layout) && layout.obj_props + layout.props_mirror_flags < 4096
}

// x0..x7 hold (data, length) pairs. The region is helper-free, never changes receiver
// slots and never writes objects, so neither the ownership nor the mirror can change.
fn registers(plan: &Plan, slot: u16) -> (u32, u32) {
    let data = plan.receivers.iter().position(|s| *s == slot).unwrap() as u32 * 2;
    (data, data + 1)
}

pub(super) fn preamble(a: &mut asm::Asm, plan: &Plan, layout: &JitLayout, fail: usize) {
    for &slot in &plan.receivers {
        let (data, len) = registers(plan, slot);
        a.ldrb_imm(9, 22, slot as u32 * 16);
        a.cmp_imm_w(9, 8);
        a.b_cond(C_NE, fail);
        a.ldr_imm(9, 22, slot as u32 * 16 + 8);
        a.add_imm(9, 9, layout.obj_from_rc as u32);
        a.ldrb_imm(10, 9, layout.obj_exotic as u32);
        let plain = a.new_label();
        a.cmp_imm_w(10, layout.exotic_none_tag as u32);
        a.b_cond(C_EQ, plain);
        a.cmp_imm_w(10, layout.exotic_array_tag as u32);
        a.b_cond(C_NE, fail);
        a.bind(plain);
        a.ldrb_imm(10, 9, layout.obj_ic_plain as u32);
        a.cbz(10, false, fail);
        let need = (MIRROR_OK | MIRROR_NO_HOLES) as u32;
        a.ldrb_imm(10, 9, (layout.obj_props + layout.props_mirror_flags) as u32);
        a.logic_imm_w(0, 10, 10, asm::logical_imm_w(need).unwrap());
        a.cmp_imm_w(10, need);
        a.b_cond(C_NE, fail);
        a.ldr_imm(9, 9, (layout.obj_props + layout.props_elems) as u32);
        a.cbz(9, true, fail);
        a.ldr_imm(data, 9, (layout.dense_mirror + layout.vec_ptr_off) as u32);
        a.ldr_imm(len, 9, (layout.dense_mirror + layout.vec_len_off) as u32);
    }
}

pub(super) fn read(
    a: &mut asm::Asm,
    plan: &Plan,
    slot: u16,
    key: u32,
    result: u32,
    fail: usize,
    converted: bool,
) {
    let (data, len) = registers(plan, slot);
    if !converted {
        a.fcvtzu_w_d(9, key);
        a.ucvtf_d_w(0, 9);
        a.fcmp(0, key);
        a.b_cond(C_NE, fail); // Exact uint32 only; -0 correctly addresses element zero.
    }
    a.cmp_reg_x(9, len);
    a.b_cond(C_HS, fail);
    a.ldr_d_lsl3(result, data, 9);
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, enters: bool, bails: bool) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::super::ENTRIES.with(|n| n.set(0));
            super::super::BAILS.with(|n| n.set(0));
            let script = format!("function assert(v){{if(!v)throw new Error('numeric array region');}} {source}; 'passed'");
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if matches!(tier, Tier::Jit) {
                assert_eq!(super::super::ENTRIES.with(|n| n.get() > 0), enters);
                assert_eq!(super::super::BAILS.with(|n| n.get() > 0), bails);
            }
        }
    }

    #[test]
    fn numeric_array_reads_cross_branches_and_continuations() {
        check(
            r#"
            function scan(a,n) {
                var sum=0;
                for(var i=0;i<n;i++) {
                    if(i<5000)sum+=a[i]*2;else sum-=a[i];
                }
                return sum;
            }
            assert(scan(new Array(10000).fill(2),10000)===10000);
            function pair(a,b,n) {
                var sum=0;
                for(var i=0;i<n;i++) {
                    if(i<2)sum+=a[i]*b[i];else sum-=a[i]*b[i];
                }
                return sum;
            }
            assert(pair([1,2,3],[4,5,6],3)===-4);
        "#,
            true,
            false,
        );
    }

    #[test]
    fn failed_index_guards_restore_prior_updates_and_live_operands() {
        check(
            r#"
            function scan(a,n) {
                var sum=0,count=0;
                for(var i=0;i<n;i++) {
                    if(i<10)sum+=(++count)*a[i];else sum--;
                }
                return sum*100+count;
            }
            var calls=0,a=[1,2,3];
            var proto=Object.create(Array.prototype);
            Object.defineProperty(proto,'3',{get(){calls++;a[0]=10;return 4;}});
            Object.setPrototypeOf(a,proto);
            assert(scan(a,4)===3004 && calls===1 && a[0]===10);
            function keyed(a,n,key) {
                var sum=0;
                for(var i=0;i<n;i++) {if(i<2)sum+=a[i];else sum+=a[key];}
                return sum;
            }
            var b=[1,2,3];
            b['1.5']=9;b['-1']=7;b['NaN']=8;b['Infinity']=6;
            assert(keyed(b,3,1.5)===12);
            assert(keyed(b,3,-1)===10);
            assert(keyed(b,3,NaN)===11);
            assert(keyed(b,3,Infinity)===9);
            assert(keyed(b,3,-0)===4);
            function pair(a,b,n) {
                var sum=0;
                for(var i=0;i<n;i++) {if(i<10)sum+=a[i]*b[i];else sum--;}
                return sum;
            }
            var short=[4],pairCalls=0,pairProto=Object.create(Array.prototype);
            Object.defineProperty(pairProto,'1',{get(){pairCalls++;return 5;}});
            Object.setPrototypeOf(short,pairProto);
            assert(pair([1,2],short,2)===14 && pairCalls===1);
        "#,
            true,
            true,
        );
    }

    #[test]
    fn exotic_holey_and_mutated_receivers_keep_checked_reads() {
        check(
            r#"
            function scan(a,n) {
                var sum=0;
                for(var i=0;i<n;i++) {if(i<2)sum+=a[i];else sum-=a[i];}
                return sum;
            }
            var calls=0;
            var accessor=[1,2,3];
            Object.defineProperty(accessor,'1',{get(){calls++;return 2;}});
            assert(scan(accessor,3)===0 && calls===1);
            var proxy=new Proxy([1,2,3],{get(o,k){calls++;return o[k];}});
            assert(scan(proxy,3)===0 && calls===4);
            assert(scan(new Float64Array([1,2,3]),3)===0);
            var hole=[1,,3],proto=Object.create(Array.prototype);
            Object.defineProperty(proto,'1',{get(){calls++;return 2;}});
            Object.setPrototypeOf(hole,proto);
            assert(scan(hole,3)===0 && calls===5);
            function reassigned(a,b,n) {
                var sum=0;
                for(var i=0;i<n;i++) {if(i>1)a=b;sum+=a[i];}
                return sum;
            }
            assert(reassigned([1,2,3],[4,5,6],3)===9);
        "#,
            false,
            false,
        );
    }
}
