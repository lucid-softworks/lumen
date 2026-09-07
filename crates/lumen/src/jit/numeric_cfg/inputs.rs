//! Guarded numeric inputs remain invariant throughout a closed, helper-free region.
mod property;
use crate::bytecode::{Chunk, Op};
use crate::jit::{asm::Asm, C_NE};
use crate::value::JitLayout;

#[derive(Clone, Debug)]
pub(super) enum Input {
    Name {
        name: u32,
        cache: usize,
    },
    Property {
        receiver: Option<u16>,
        name: String,
        cache: usize,
    },
}

impl Input {
    pub(super) fn decode(chunk: &Chunk, op: Op) -> Option<Self> {
        Some(match op {
            Op::LoadName(name, cache) => Self::Name {
                name,
                cache: chunk.jit_name_cache_ptr(cache),
            },
            Op::GetPropLocal(slot, name, cache) => Self::Property {
                receiver: Some(slot),
                name: chunk.jit_name(name).to_owned(),
                cache: chunk.jit_cache_ptr(cache),
            },
            Op::GetPropThis(name, cache) => Self::Property {
                receiver: None,
                name: chunk.jit_name(name).to_owned(),
                cache: chunk.jit_cache_ptr(cache),
            },
            _ => return None,
        })
    }

    pub(super) fn same_source(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Name { name: a, .. }, Self::Name { name: b, .. }) => a == b,
            (
                Self::Property {
                    receiver: a,
                    name: x,
                    ..
                },
                Self::Property {
                    receiver: b,
                    name: y,
                    ..
                },
            ) => a == b && x == y,
            _ => false,
        }
    }

    pub(super) fn receiver(&self) -> Option<u16> {
        match self {
            Self::Property { receiver, .. } => *receiver,
            _ => None,
        }
    }
}

pub(super) fn supported(inputs: &[Input], layout: &JitLayout) -> bool {
    inputs.iter().all(|input| match input {
        Input::Name { .. } => super::super::load_name_inlinable(layout),
        Input::Property { .. } => super::super::get_prop_inlinable(layout),
    })
}

pub(super) fn register(index: u8) -> u32 {
    2 + index as u32
}

pub(super) fn emit(a: &mut Asm, inputs: &[Input], layout: &JitLayout, fail: usize) {
    // d2..d7 are caller-saved and disjoint from local/temporary homes. Probe scratch is in
    // x7..x17, so emit all inputs before pinning array pointers or the continuation counter.
    for (index, input) in inputs.iter().enumerate() {
        let out = register(index as u8);
        match input {
            Input::Name { cache, .. } => name(a, layout, *cache, out, fail),
            Input::Property {
                receiver,
                name,
                cache,
            } => {
                property::emit(a, layout, *receiver, *cache, name, out, fail);
            }
        }
    }
}

fn name(a: &mut Asm, layout: &JitLayout, cache: usize, out: u32, fail: usize) {
    super::super::emit_name_ic_value_ptr(a, layout, cache, fail, true);
    let wide = a.new_label();
    let done = a.new_label();
    if layout.entry_accessor == layout.entry_value + 8 {
        a.cbz(7, false, wide);
        super::super::emit_region_packed_number(a, 14, 0, out, fail);
        a.b(done);
    }
    a.bind(wide);
    a.ldurb(9, 14, 0);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, fail);
    a.ldur_d(out, 14, 8);
    a.bind(done);
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, enters: bool) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::super::ENTRIES.with(|n| n.set(0));
            super::super::BAILS.with(|n| n.set(0));
            let source = format!("function assert(v){{if(!v)throw new Error('numeric region inputs');}} {source}; 'passed'");
            match engine.eval(&source, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if matches!(tier, Tier::Jit) {
                assert_eq!(super::super::ENTRIES.with(|n| n.get() > 0), enters);
            }
        }
    }

    #[test]
    fn numeric_properties_and_shifted_array_slots_are_loaded_live() {
        check(
            r#"
            function scan(a,cfg) {
                var sum=0;
                for(var i=0;i<a.length;i++) {
                    if(i<cfg.pivot)sum+=a[i]*cfg.scale;else sum-=a[i];
                }
                return sum;
            }
            var a=[1,2,3,4];
            assert(scan(a,{pivot:2,scale:2})===-1);
            assert(scan(a,{unused:0,pivot:3,scale:3})===14);
            assert(scan(a,Object.freeze({pivot:2,scale:2}))===-1);
            function weighted(a) {
                var sum=0;
                for(var i=0;i<a.length;i++) {if(i<2)sum+=a[i]*a.scale;else sum-=a[i];}
                return sum;
            }
            var x=[1,2],y=[5,6,7];x.scale=4;y.scale=9;
            assert(weighted(x)===12);assert(weighted(y)===92);assert(weighted(x)===12);
            y.push(8);assert(weighted(y)===84);
            x.length=1;assert(weighted(x)===4);
            function receiver() {
                var sum=0;
                for(var i=0;i<this.limit;i++) {if(i<2)sum+=this.scale;else sum--;}
                return sum;
            }
            assert(receiver.call({limit:4,scale:2})===2);
            assert(receiver.call({scale:5,limit:3})===9);
        "#,
            true,
        );
    }

    #[test]
    fn enclosing_bindings_and_global_values_are_revalidated() {
        check(
            r#"
            globalThis.regionLimit=4;globalThis.regionWeight=2;
            function globalSum() {
                var sum=0;
                for(var i=0;i<regionLimit;i++) {if(i<2)sum+=regionWeight;else sum--;}
                return sum;
            }
            assert(globalSum()===2);
            regionLimit=3;regionWeight=5;assert(globalSum()===9);
            function factory(n) {
                return {run:function(a) {
                    var sum=0;
                    for(var i=0;i<n;i++) {if(i<2)sum+=a[i];else sum-=a[i];}
                    return sum;
                },set:function(v){n=v;}};
            }
            var f=factory(4),g=factory(3),a=[1,2,3,4];
            assert(f.run(a)===-4 && g.run(a)===0);
            f.set(2);assert(f.run(a)===3);g.set(4);assert(g.run(a)===-4);
        "#,
            true,
        );
    }

    #[test]
    fn a_mid_loop_getter_invalidates_inputs_before_reentry() {
        check(
            r#"
            function scan(a,cfg) {
                var sum=0;
                for(var i=0;i<cfg.limit;i++) {
                    if(i<2)sum+=a[i]*cfg.scale;else sum+=a[i]+cfg.scale;
                }
                return sum;
            }
            var a=[1,2],cfg={limit:4,scale:2},calls=0;
            var proto=Object.create(Array.prototype);
            Object.defineProperty(proto,'2',{get(){calls++;cfg.scale=10;cfg.limit=3;return 3;}});
            Object.setPrototypeOf(a,proto);
            assert(scan(a,cfg)===19 && calls===1);
        "#,
            true,
        );
        assert!(super::super::BAILS.with(|n| n.get() > 0));
    }

    #[test]
    fn accessors_proxies_prototypes_and_coercion_remain_effectful() {
        check(
            r#"
            function scan(cfg) {
                var sum=0;
                for(var i=0;i<cfg.limit;i++) {if(i<2)sum+=cfg.scale;else sum--;}
                return sum;
            }
            var calls=0;
            var accessor={get limit(){calls++;return 4;},scale:2};
            assert(scan(accessor)===2 && calls===5);
            var proxy=new Proxy({limit:4,scale:2},{get(o,k){calls++;return o[k];}});
            assert(scan(proxy)===2 && calls===12);
            assert(scan(Object.create({limit:4,scale:2}))===2);
            var coerced={limit:{valueOf(){calls++;return 4;}},scale:2};
            assert(scan(coerced)===2 && calls===17);
            globalThis.regionLimit=4;
            Object.defineProperty(globalThis,'regionLimit',{get(){calls++;return 4;}});
            function globalSum() {
                var sum=0;
                for(var i=0;i<regionLimit;i++) {if(i<2)sum+=2;else sum--;}
                return sum;
            }
            assert(globalSum()===2 && calls===22);
        "#,
            false,
        );
    }
}
