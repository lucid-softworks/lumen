//! Restore the source function's strictness around writes reached by fast JIT calls.
use super::{Chunk, Op};
use crate::interpreter::Interp;

pub(super) fn enter(interp: &mut Interp, chunk: &Chunk, pc: u32) -> bool {
    let strict = chunk.execution_strictness(interp, pc as usize);
    std::mem::replace(&mut interp.strict, strict)
}

pub(super) fn needed(op: Op) -> bool {
    matches!(
        op,
        Op::SetProp(..)
            | Op::SetPropDrop(..)
            | Op::SetPropThisDrop(..)
            | Op::SetPropLocalDrop(..)
            | Op::SetElem
            | Op::SetElemDrop
            | Op::SetElemLocal(..)
            | Op::SetElemLocalDrop(..)
            | Op::AppendProp(..)
            | Op::UpdateProp(..)
            | Op::UpdateElem(..)
            | Op::StoreName(..)
            | Op::StoreNameCached(..)
            | Op::UpdateName(..)
            | Op::UpdateNameCached(..)
            | Op::StoreCap(..)
            | Op::StoreCapInit(..)
            | Op::UpdateCap(..)
    )
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let script = format!("function assert(v){{if(!v)throw new Error('write strictness');}} {source}; 'passed'");
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
        }
    }

    fn property_case(body: &str, expected: i32, receiver: bool) {
        let call = if receiver {
            ".call(o,o,'x')"
        } else {
            "(o,'x')"
        };
        check(&format!(
            r#"
            function sloppyWrite(o,k){{{body}}}
            function strictWrite(o,k){{'use strict';{body}}}
            function strictCaller(o){{'use strict';return sloppyWrite{call};}}
            function sloppyCaller(o){{return strictWrite{call};}}
            function warm(){{for(let i=0;i<500;i++){{strictCaller({{x:1}});sloppyCaller({{x:1}});}}}}
            warm();
            const frozen=Object.freeze({{x:1}});
            assert(strictCaller(frozen)==={expected} && frozen.x===1);
            let threw=false;try{{sloppyCaller(frozen);}}catch(e){{threw=e instanceof TypeError;}}assert(threw);
            const proxy=new Proxy({{x:1}},{{set(){{return false;}}}});
            assert(strictCaller(proxy)==={expected} && proxy.x===1);
            threw=false;try{{sloppyCaller(proxy);}}catch(e){{threw=e instanceof TypeError;}}assert(threw);
        "#
        ));
    }

    #[test]
    fn property_and_element_writes_use_the_callee_mode_after_fast_calls() {
        for (body, expected) in [
            ("o.x=2;return o.x", 1),
            ("return o.x=2", 2),
            ("o[k]=2;return o[k]", 1),
            ("return o[k]=2", 2),
            ("o.x++;return o.x", 1),
            ("o[k]++;return o[k]", 1),
            ("o.x+=2;return o.x", 1),
        ] {
            property_case(body, expected, false);
        }
        property_case("this.x=2;return this.x", 1, true);
    }

    #[test]
    fn unresolved_name_writes_keep_strictness_and_restore_it_after_errors() {
        check(
            r#"
            globalThis.targetName=0;
            function sloppyWrite(){targetName=1;}
            function strictWrite(){'use strict';targetName=2;}
            function strictCaller(){'use strict';sloppyWrite();}
            function sloppyCaller(){strictWrite();}
            function warm(){for(let i=0;i<500;i++){strictCaller();sloppyCaller();}}
            warm();delete globalThis.targetName;
            strictCaller();assert(targetName===1);delete globalThis.targetName;
            let threw=false;try{sloppyCaller();}catch(e){threw=e instanceof ReferenceError;}assert(threw);
            afterError=3;assert(globalThis.afterError===3);
            function strictParent(){'use strict';strictCaller();let failed=false;
                try{stillMissing=1;}catch(e){failed=e instanceof ReferenceError;}return failed;}
            assert(strictParent());
        "#,
        );
    }

    #[test]
    fn read_only_array_elements_and_length_keep_strict_write_errors() {
        check(
            r#"
            function sloppyWrite(o){o[0]=2;return o[0];}
            function strictWrite(o){'use strict';o[0]=2;return o[0];}
            function strictCaller(o){'use strict';return sloppyWrite(o);}
            function sloppyCaller(o){return strictWrite(o);}
            function warm(){for(let i=0;i<500;i++){strictCaller([1]);sloppyCaller([1]);}}
            warm();const array=Object.freeze([1]);assert(strictCaller(array)===1);
            let threw=false;try{sloppyCaller(array);}catch(e){threw=e instanceof TypeError;}assert(threw);
            const empty=[];Object.defineProperty(empty,'length',{writable:false});
            assert(strictCaller(empty)===undefined);
            threw=false;try{sloppyCaller(empty);}catch(e){threw=e instanceof TypeError;}assert(threw);
        "#,
        );
    }
}
