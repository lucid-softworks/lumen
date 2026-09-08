//! Version selection and copied, non-executable proof outcomes.
use crate::ast::Function;
use crate::jit_ir::iterator_entry::{self, Candidate, Reject};
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Version {
    Cold,
    Base,
    Optimized,
}

pub(super) enum Outcome {
    Cold,
    Accepted(Candidate, String),
    Rejected(Reject, String),
    Unsupported(&'static str),
}

pub(super) fn version(function: &Function) -> Version {
    if function.code2.get().is_some() {
        Version::Optimized
    } else if function.code.get().is_some() {
        Version::Base
    } else {
        Version::Cold
    }
}
pub(super) fn classify(function: &Function) -> Outcome {
    if function.is_arrow || function.is_async || function.is_generator {
        Outcome::Unsupported("unsupported-function-kind")
    } else {
        match function.code2.get().or_else(|| function.code.get()) {
            None => Outcome::Cold,
            Some(None) => Outcome::Unsupported("uncompiled-body"),
            Some(Some(chunk)) => match iterator_entry::analyze(chunk) {
                Ok(plan) => {
                    let label = format!(
                        "accepted:ops={}:return={}:store={}:branches={}",
                        chunk.jit_ops().len(),
                        plan.return_pc,
                        plan.store
                            .as_ref()
                            .map_or_else(|| "none".into(), |s| s.pc.to_string()),
                        plan.branches.len()
                    );
                    Outcome::Accepted(plan, label)
                }
                Err(reason) => {
                    let label = format!("rejected:{}", reason.reason.replace(' ', "_"));
                    Outcome::Rejected(reason, label)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Feedback, Site, COUNTS, REPLANS};
    use super::*;
    use crate::{ast::Stmt, bytecode, parser, Engine};
    use std::{
        cell::RefCell,
        rc::{Rc, Weak},
    };

    fn parse_function(source: &str) -> Rc<Function> {
        let statements =
            parser::parse_script(source, false).unwrap_or_else(|_| panic!("valid function"));
        let Stmt::FuncDecl(function) = &statements[0] else {
            panic!("function")
        };
        function.clone()
    }

    #[test]
    fn observations_refresh_code_versions_and_recheck_active_realm() {
        let engine = Engine::new();
        let function = parse_function("function next(){return {value:7,done:false};}");
        let next = engine
            .interp
            .make_function(function.clone(), engine.interp.global_env.clone());
        let feedback = Feedback {
            sites: vec![(
                12,
                RefCell::new(Site {
                    callee: Weak::new(),
                    version: Version::Cold,
                    outcome: Outcome::Cold,
                }),
            )],
        };
        REPLANS.with(|n| n.set(0));
        feedback.observe(12, &engine.interp, &next);
        assert_eq!(REPLANS.with(|n| n.get()), 1);
        assert!(matches!(
            feedback.sites[0].1.borrow().outcome,
            Outcome::Cold
        ));
        assert!(function
            .code
            .set(Some(bytecode::compile(&function).unwrap()))
            .is_ok());
        feedback.observe(12, &engine.interp, &next);
        feedback.observe(12, &engine.interp, &next);
        assert_eq!(REPLANS.with(|n| n.get()), 2);
        assert!(matches!(
            feedback.sites[0].1.borrow().outcome,
            Outcome::Accepted(..)
        ));
        let optimized = parse_function("function other(){return {value:9,done:false};}");
        assert!(function
            .code2
            .set(Some(bytecode::compile(&optimized).unwrap()))
            .is_ok());
        feedback.observe(12, &engine.interp, &next);
        feedback.observe(12, &engine.interp, &next);
        assert_eq!(REPLANS.with(|n| n.get()), 3);
        assert!(matches!(
            feedback.sites[0].1.borrow().version,
            Version::Optimized
        ));
        {
            let site = feedback.sites[0].1.borrow();
            let Outcome::Accepted(plan, _) = &site.outcome else {
                panic!("optimized candidate")
            };
            assert_eq!(
                plan.expressions[plan.value],
                iterator_entry::Expr::Constant(iterator_entry::Literal::Number(9f64.to_bits()))
            );
        }
        let other_realm = Engine::new();
        COUNTS.with(|counts| counts.borrow_mut().0.clear());
        feedback.observe(12, &other_realm.interp, &next);
        assert_eq!(REPLANS.with(|n| n.get()), 3);
        COUNTS.with(|counts| {
            let counts = counts.borrow();
            assert_eq!(counts.0.get("foreign-realm"), Some(&1));
            assert_eq!(counts.0.len(), 1);
        });
    }
}
