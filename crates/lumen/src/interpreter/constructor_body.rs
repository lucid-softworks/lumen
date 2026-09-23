//! Eligibility for executing a constructor body in the compiled tiers.
use super::Interp;
use crate::value::Gc;
use std::rc::Rc;

impl Interp {
    /// Base-class fields/private members/decorator initializers have already run in
    /// `run_constructor_on`. Its body can use the ordinary compiled calling convention.
    /// Derived constructors still need a TDZ this binding, super rebinding and return checks.
    pub(super) fn constructor_body_can_compile(&self, constructor: &Gc) -> bool {
        self.class_info
            .get(&(Rc::as_ptr(constructor) as usize))
            .is_none_or(|info| !info.derived)
    }

    /// Whether construction has no class-owned instance setup before the body. Only this class
    /// shape may share the ordinary function-constructor IC path: derived classes need TDZ
    /// `this`/`super()` handling, while every other metadata list has observable work or errors.
    pub(super) fn is_empty_base_class(&self, constructor: &Gc) -> bool {
        self.class_info
            .get(&(Rc::as_ptr(constructor) as usize))
            .is_some_and(|info| {
                !info.derived
                    && info.fields.is_empty()
                    && info.instance_initializers.is_empty()
                    && info.private_members.is_empty()
            })
    }
}

#[cfg(test)]
mod tests {
    use crate::value::Callable;
    use crate::{bytecode::Tier, Completion, Engine};

    fn run(source: &str, compiled: &[&str]) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let script = format!("function assert(v){{if(!v)throw new Error('constructor assertion');}} {source}; 'passed'");
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if !matches!(tier, Tier::Interp) {
                for name in compiled {
                    let value = engine
                        .interp
                        .global
                        .borrow()
                        .props
                        .get(name)
                        .unwrap()
                        .value();
                    let object = value.as_obj().unwrap().borrow();
                    let Callable::User(user) = &object.call else {
                        panic!("constructor callable")
                    };
                    let chunk = user
                        .func
                        .code
                        .get()
                        .and_then(|code| code.as_ref())
                        .unwrap_or_else(|| panic!("{name} did not compile in {tier:?}"));
                    if matches!(tier, Tier::Jit) {
                        assert!(
                            chunk.jit.get().is_some_and(|code| code.is_some()),
                            "{name} did not JIT"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn base_fields_defaults_and_captured_this_precede_the_compiled_body() {
        run(
            r#"
            const log=[];
            class Base {
                field=(log.push('field'),3);
                constructor(options={}) {
                    log.push('body');this.options=options;
                    this.read=()=>this.field+options.value;
                }
            }
            globalThis.CompiledBase=Base;
            const a=new Base(),b=new Base();a.options.value=4;b.options.value=9;
            assert(a.options!==b.options && a.read()===7 && b.read()===12);
            assert(log.join(',')==='field,body,field,body');
            function Other(){}
            const c=Reflect.construct(Base,[{value:2}],Other);
            assert(Object.getPrototypeOf(c)===Other.prototype && c.read()===5);
        "#,
            &["CompiledBase"],
        );
    }

    #[test]
    fn base_return_overrides_and_derived_super_keep_their_construction_rules() {
        run(
            r#"
            class Base {
                x=1;
                constructor(mode) {
                    if(mode===1)return {x:4};
                    if(mode===2)return 5;
                    this.y=2;
                }
            }
            class Derived extends Base { z=3;constructor(mode){super(mode);this.w=4;} }
            globalThis.CompiledBase=Base;
            const a=new Base(1),b=new Base(2),c=new Derived(1),d=new Derived(2);
            assert(a.x===4 && !(a instanceof Base));assert(b.x===1 && b instanceof Base);
            assert(c.x===4 && c.z===3 && c.w===4);
            assert(d.x===1 && d.z===3 && d.w===4 && d instanceof Derived);
            class Bad extends Base {constructor(){return 5;}}
            let rejected=false;try{new Bad();}catch(e){rejected=e instanceof TypeError;}assert(rejected);
        "#,
            &["CompiledBase"],
        );
    }

    #[test]
    fn a_hot_compiled_class_still_requires_new() {
        run(
            r#"
            class Base {constructor(value){this.value=value;}}
            globalThis.CompiledBase=Base;
            for(let n=0;n<200;n++)assert(new Base(n).value===n);
            const bound=Base.bind(null,17);
            const calls=[()=>Base(1),()=>Base.call({},1),()=>Base.apply({},[1]),
                ()=>Reflect.apply(Base,{},[1]),()=>bound()];
            for(const call of calls){let rejected=false;try{call();}catch(e){rejected=e instanceof TypeError;}assert(rejected);}
            assert(new bound().value===17);
        "#,
            &["CompiledBase"],
        );
    }

    #[test]
    fn empty_base_class_cached_construction_preserves_context_and_failures() {
        run(
            r#"
            class Empty {
                constructor(value) {
                    order.push('body');
                    if (value < 0) throw new Error('negative');
                    this.value=value;
                    if (value > 0) this.next=new Empty(value-1);
                }
            }
            const order=[];
            globalThis.CompiledEmpty=Empty;
            for(let n=0;n<500;n++)assert(new Empty(0).value===0);
            order.length=0;
            function argument(){order.push('argument');return 0;}
            assert(new Empty(argument()).value===0 && order.join(',')==='argument,body');
            const recursive=new Empty(3);
            assert(recursive.next.next.next.value===0);
            let threw=false;
            try{new Empty(-1);}catch(e){
                threw=e.message==='negative' && e.stack.includes('at Empty');
            }
            assert(threw && new Empty(1).next.value===0);
        "#,
            &["CompiledEmpty"],
        );
    }

    #[test]
    fn hot_construction_does_not_bypass_field_initializers_or_their_errors() {
        run(
            r#"
            let fields=0,bodies=0,fail=false;
            function field(){fields++;if(fail)throw new Error('field');return fields;}
            class Base {x=field();constructor(){bodies++;this.y=2;}}
            globalThis.CompiledBase=Base;
            for(let n=0;n<200;n++)assert(new Base().y===2);
            fail=true;let rejected=false;
            try{new Base();}catch(e){rejected=e.message==='field';}
            assert(rejected && fields===201 && bodies===200);
            fail=false;assert(new Base().x===202 && bodies===201);
        "#,
            &["CompiledBase"],
        );
    }

    #[test]
    fn private_members_and_escaped_callbacks_keep_their_instance() {
        run(
            r#"
            class Base {
                #value=7;
                read(){return this.#value;}
                constructor(options={}){this.options=options;this.callback=()=>this.read();}
            }
            globalThis.CompiledBase=Base;
            const keep=[];for(let n=0;n<500;n++)keep.push(new Base());
            assert(keep[0].callback()===7 && keep[499].callback()===7);
            assert(keep[0].options!==keep[1].options);
            let rejected=false;try{new Base(null).options.value;}catch(e){rejected=e instanceof TypeError;}assert(rejected);
            assert(new Base().callback()===7);
        "#,
            &["CompiledBase"],
        );
    }
}
