//! Runtime values and the object model. Objects are `Rc<RefCell<Object>>` ([`Gc`]); there is no
//! real garbage collector yet (reference counting, so cycles leak — acceptable for the test262
//! loop). Properties are stored in insertion order in a small map.

use crate::ast::Function;
use crate::interpreter::{Env, Interp};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub type Gc = Rc<RefCell<Object>>;

/// A native (Rust-implemented) function. It can only throw (via `Err`), never break/return/continue,
/// so a plain `Result<Value, Value>` (Err = the thrown value) is the whole contract.
pub type NativeFn = fn(&mut Interp, Value, &[Value]) -> Result<Value, Value>;

#[derive(Clone)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(Rc<str>),
    Obj(Gc),
}

impl Value {
    pub fn str(s: impl Into<Rc<str>>) -> Value {
        Value::Str(s.into())
    }
    pub fn from_string(s: String) -> Value {
        Value::Str(Rc::from(s.as_str()))
    }
    pub fn as_obj(&self) -> Option<&Gc> {
        match self {
            Value::Obj(o) => Some(o),
            _ => None,
        }
    }
    pub fn is_callable(&self) -> bool {
        matches!(self, Value::Obj(o) if !matches!(o.borrow().call, Callable::None))
    }
    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undefined => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Obj(o) => {
                if matches!(o.borrow().call, Callable::None) {
                    "object"
                } else {
                    "function"
                }
            }
        }
    }
}

/// How an object can be called. Most objects are not callable (`None`).
#[derive(Clone)]
pub enum Callable {
    None,
    Native(NativeFn),
    /// An interpreted function: its AST plus the lexical environment it closed over.
    User(Rc<Function>, Env),
    /// The result of `Function.prototype.bind`.
    Bound { target: Gc, this: Value, args: Vec<Value> },
}

/// Exotic internal data for built-in object kinds (arrays, primitive wrappers). The wrapper
/// variants are read by the `this_*` coercion helpers but not yet constructed (`new String()` etc.
/// still return primitives — boxing is the next built-ins milestone).
#[derive(Clone)]
#[allow(dead_code)]
pub enum Exotic {
    None,
    Array,
    BoolWrap(bool),
    NumWrap(f64),
    StrWrap(Rc<str>),
    /// An error object — carries no extra data (name/message live as ordinary properties) but the
    /// tag lets `Error.prototype.toString` and the test262 runner recognise it cheaply.
    Error,
}

pub struct Object {
    pub proto: Option<Gc>,
    pub props: Props,
    pub extensible: bool,
    pub call: Callable,
    pub exotic: Exotic,
    /// The construct-time prototype handed to instances (`F.prototype`), cached for `new`.
    pub is_constructor: bool,
}

impl Object {
    pub fn new(proto: Option<Gc>) -> Gc {
        Rc::new(RefCell::new(Object {
            proto,
            props: Props::new(),
            extensible: true,
            call: Callable::None,
            exotic: Exotic::None,
            is_constructor: false,
        }))
    }
}

/// A property descriptor. A data property uses `value`/`writable`; an accessor uses `get`/`set`.
#[derive(Clone)]
pub struct Property {
    pub value: Value,
    pub get: Option<Value>,
    pub set: Option<Value>,
    pub accessor: bool,
    pub writable: bool,
    pub enumerable: bool,
    pub configurable: bool,
}

impl Property {
    pub fn data(value: Value, writable: bool, enumerable: bool, configurable: bool) -> Property {
        Property {
            value,
            get: None,
            set: None,
            accessor: false,
            writable,
            enumerable,
            configurable,
        }
    }
    /// A default plain data property: writable, enumerable, configurable.
    pub fn plain(value: Value) -> Property {
        Property::data(value, true, true, true)
    }
    /// A non-enumerable method/builtin property: writable + configurable, not enumerable.
    pub fn builtin(value: Value) -> Property {
        Property::data(value, true, false, true)
    }
}

/// Insertion-ordered string-keyed property map. A `Vec` of entries preserves order (good enough for
/// `for-in`/`Object.keys`); a side `HashMap` keeps lookup O(1).
pub struct Props {
    entries: Vec<(Rc<str>, Property)>,
    index: HashMap<Rc<str>, usize>,
}

impl Default for Props {
    fn default() -> Self {
        Self::new()
    }
}

impl Props {
    pub fn new() -> Props {
        Props { entries: Vec::new(), index: HashMap::new() }
    }
    pub fn get(&self, key: &str) -> Option<&Property> {
        self.index.get(key).map(|i| &self.entries[*i].1)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Property> {
        if let Some(i) = self.index.get(key) {
            Some(&mut self.entries[*i].1)
        } else {
            None
        }
    }
    pub fn contains(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }
    pub fn insert(&mut self, key: impl Into<Rc<str>>, prop: Property) {
        let key = key.into();
        if let Some(i) = self.index.get(&key) {
            self.entries[*i].1 = prop;
        } else {
            self.index.insert(key.clone(), self.entries.len());
            self.entries.push((key, prop));
        }
    }
    pub fn remove(&mut self, key: &str) -> bool {
        if let Some(i) = self.index.remove(key) {
            self.entries.remove(i);
            // Re-index everything after the removed slot.
            for (j, (k, _)) in self.entries.iter().enumerate().skip(i) {
                self.index.insert(k.clone(), j);
            }
            true
        } else {
            false
        }
    }
    /// Keys in insertion order.
    pub fn keys(&self) -> Vec<Rc<str>> {
        self.entries.iter().map(|(k, _)| k.clone()).collect()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&Rc<str>, &Property)> {
        self.entries.iter().map(|(k, p)| (k, p))
    }
}

/// Convenience: define a plain own data property by key/value.
pub fn set_data(obj: &Gc, key: &str, value: Value) {
    obj.borrow_mut().props.insert(key, Property::plain(value));
}

/// Convenience: define a non-enumerable builtin property by key/value.
pub fn set_builtin(obj: &Gc, key: &str, value: Value) {
    obj.borrow_mut().props.insert(key, Property::builtin(value));
}
