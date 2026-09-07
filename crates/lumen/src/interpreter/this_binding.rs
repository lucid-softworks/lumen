//! Lexical this resolution shared by interpreted and compiled arrow bodies.
use super::{Abrupt, Env, Interp};
use crate::value::Value;

impl Interp {
    pub(crate) fn lexical_this(&mut self, env: &Env) -> Result<Value, Abrupt> {
        // A TDZ read (derived constructor before super()) must surface as a
        // ReferenceError; only a genuinely absent binding reads undefined. Single walk:
        // the binding is read where it is found (get_var would walk a second time).
        let mut cur = Some(env.clone());
        while let Some(scope) = cur {
            let parent = {
                let b = scope.borrow();
                if let Some(bd) = b.vars.get("this") {
                    if bd.initialized && bd.import_ref.is_none() {
                        return Ok(bd.value.clone());
                    }
                    drop(b);
                    return self.get_var("this", env);
                }
                b.parent.clone()
            };
            cur = parent;
        }
        Ok(Value::Undefined)
    }
}
