//! Body-level object bindings may share the ordinary captured-lexical activation.
use super::*;

impl Compiler {
    pub(super) fn declare_body_pattern(
        &mut self,
        pat: &Pattern,
        is_const: bool,
        captured: &FastSet<String>,
    ) -> CResult {
        match pat {
            Pattern::Ident(name) => {
                if captured.contains(name) {
                    self.cap_inits
                        .push(CapInit::Lexical(Rc::from(name.as_str()), is_const));
                    self.env_bind(name, is_const);
                } else {
                    let slot = self.fresh_slot(name);
                    self.scope_bind(name, slot, is_const);
                    self.tdz_slots.insert(slot);
                    self.emit(Op::Tdz(slot));
                }
                Ok(())
            }
            Pattern::Object(object) if object.rest.is_none() => {
                for prop in &object.props {
                    if prop.default.is_some()
                        || !matches!(&prop.key, PropKey::Ident(_) | PropKey::Str(_))
                    {
                        return Err(Bail);
                    }
                    self.declare_body_pattern(&prop.value, is_const, captured)?;
                }
                Ok(())
            }
            Pattern::Array(_) => {
                // Array lowering batches iterator steps before storing any leaves. A next()
                // callback could observe captured leaves between steps, so keep that case
                // on the interpreter. Object lowering reads and initializes each leaf in order.
                let mut names = FastSet::default();
                pat_idents(pat, &mut names);
                if names.iter().any(|name| captured.contains(name)) {
                    return Err(Bail);
                }
                self.declare_lexical_pattern(pat, is_const)
            }
            _ => Err(Bail),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Completion, Engine};

    #[test]
    fn captured_object_bindings_compile_and_preserve_initialization_order() {
        let source = include_str!("../../tests/fixtures/captured-objects.js");
        let body =
            crate::parser::parse_script(source, false).unwrap_or_else(|e| panic!("{}", e.message));
        let (mut accepted, mut refused) = (0, 0);
        for stmt in &body {
            if let Stmt::FuncDecl(f) = stmt {
                let name = f.name.as_deref().unwrap();
                if name.starts_with("compiled_") {
                    assert!(compile(f).is_some(), "{name}");
                    accepted += 1;
                } else if name.starts_with("fallback_") {
                    assert!(compile(f).is_none(), "{name}");
                    refused += 1;
                }
            }
        }
        assert_eq!((accepted, refused), (5, 4));
        let snapshot = crate::compile_snapshot(source).unwrap();
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            for threshold in [0, 8] {
                for cached in [false, true] {
                    let mut engine = Engine::new();
                    engine.set_tier(tier);
                    engine.set_tier_threshold(threshold);
                    let result = if cached {
                        engine.eval_snapshot(&snapshot, false)
                    } else {
                        engine.eval(source, false)
                    }
                    .unwrap_or_else(|e| panic!("{}", e.message));
                    match result {
                        Completion::Value(v) => {
                            assert_eq!(v, "passed", "{tier:?} {threshold} {cached}")
                        }
                        Completion::Throw { name, message } => {
                            panic!("{tier:?}: {name}: {message}")
                        }
                    }
                }
            }
        }
    }
}
