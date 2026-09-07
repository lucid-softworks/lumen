//! Shared for-in key snapshots, including namespace checks and prototype ordering.
use crate::interpreter::{Abrupt, Interp};
use crate::value::Value;
use std::rc::Rc;

impl Interp {
    pub(crate) fn for_in_keys(&mut self, rhs: &Value) -> Result<Vec<Value>, Abrupt> {
        // A module namespace's [[GetOwnProperty]] runs during enumeration, so an uninitialized export
        // makes the loop throw ReferenceError before any iteration.
        if let Value::Obj(o) = rhs {
            let ptr = std::rc::Rc::as_ptr(o) as usize;
            if self.is_namespace(ptr) {
                for k in self.enum_keys(rhs)? {
                    if let Some(res) = self.namespace_own_property(ptr, &k) {
                        res?;
                    }
                }
            }
        }
        Ok(self
            .enum_keys(rhs)?
            .into_iter()
            .map(Value::from_string)
            .collect())
    }

    fn enum_keys(&mut self, v: &Value) -> Result<Vec<String>, Abrupt> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        // A string primitive enumerates its index keys (ToObject makes them own properties).
        if let Value::Str(sv) = v {
            for k in 0..crate::jstr::unit_len(sv) {
                out.push(k.to_string());
            }
            return Ok(out);
        }
        let mut cur = match v {
            Value::Obj(o) => Some(o.clone()),
            _ => None,
        };
        while let Some(o) = cur {
            let ov = Value::Obj(o.clone());
            // A proxy level enumerates via its [[OwnPropertyKeys]] filtered by [[GetOwnProperty]]'s
            // enumerable flag, then walks its [[GetPrototypeOf]].
            if self.proxies.contains_key(&(Rc::as_ptr(&o) as usize)) {
                let keys =
                    crate::builtins::proxy_enum_string_keys(self, &ov).map_err(Abrupt::Throw)?;
                for k in keys {
                    if let Value::Str(ks) = k {
                        if seen.insert(ks.to_string()) {
                            out.push(ks.to_string());
                        }
                    }
                }
                let parent =
                    crate::builtins::js_get_prototype_of(self, &ov).map_err(Abrupt::Throw)?;
                cur = match parent {
                    Value::Obj(p) => Some(p),
                    _ => None,
                };
                continue;
            }
            // for-in visits own enumerable string keys in spec order, then up the prototype chain.
            // TypedArray elements enumerate first (they live outside the property map).
            if let Some(info) = self.typed_arrays.get(&(Rc::as_ptr(&o) as usize)).copied() {
                for idx in 0..self.ta_len(&info).unwrap_or(0) {
                    let k = idx.to_string();
                    if seen.insert(k.clone()) {
                        out.push(k);
                    }
                }
            }
            let (level, parent) = {
                let b = o.borrow();
                let level: Vec<(String, bool)> = b
                    .props
                    .ordered_keys()
                    .into_iter()
                    .filter(|k| !Interp::is_sym_key(k) && !Interp::is_private_key(k))
                    .map(|k| {
                        let e = b.props.get(&k).map(|p| p.enumerable()).unwrap_or(false);
                        (k.to_string(), e)
                    })
                    .collect();
                (level, b.proto.clone())
            };
            for (k, enumerable) in level {
                // A non-enumerable own property still *shadows* an enumerable prototype one.
                if seen.insert(k.clone()) && enumerable {
                    out.push(k);
                }
            }
            cur = parent;
        }
        Ok(out)
    }
}
