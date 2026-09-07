//! Immutable name-to-slot layout shared by compiled activations.
use crate::fasthash::FastMap;
use std::rc::Rc;

pub(crate) struct BindingLayout {
    pub(super) names: Vec<Rc<str>>,
    indices: FastMap<Rc<str>, usize>,
}

impl BindingLayout {
    pub(crate) fn new(names: impl IntoIterator<Item = Rc<str>>) -> Rc<Self> {
        let mut layout = Self {
            names: Vec::new(),
            indices: FastMap::default(),
        };
        for name in names {
            if !layout.indices.contains_key(&name) {
                let slot = layout.names.len();
                layout.names.push(name.clone());
                layout.indices.insert(name, slot);
            }
        }
        Rc::new(layout)
    }

    pub(crate) fn slot(&self, name: &str) -> Option<usize> {
        self.indices.get(name).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::BindingLayout;
    use crate::interpreter::{Binding, VarMap};
    use crate::value::Value;
    use std::rc::Rc;

    #[test]
    fn shared_layout_keeps_activation_values_independent() {
        let layout = BindingLayout::new([Rc::from("a"), Rc::from("b"), Rc::from("a")]);
        let mut first = VarMap::from_layout(layout.clone());
        let second = VarMap::from_layout(layout.clone());
        assert_eq!(first.iter().count(), 2);
        let slot = layout.slot("a").unwrap();
        *first.layout_binding_mut(&layout, slot).unwrap() =
            Binding::data(Value::Num(7.0), true, true);
        assert!(matches!(first.get("a").unwrap().value, Value::Num(7.0)));
        assert!(matches!(second.get("a").unwrap().value, Value::Undefined));
        assert_eq!(first.generation(), 0);
    }

    #[test]
    fn structural_changes_promote_without_losing_binding_metadata() {
        let layout = BindingLayout::new([Rc::from("a"), Rc::from("b")]);
        let mut vars = VarMap::from_layout(layout.clone());
        *vars.layout_binding_mut(&layout, 1).unwrap() =
            Binding::data(Value::Undefined, false, false);
        vars.insert("a", Binding::data(Value::Num(2.0), true, true));
        assert_eq!(vars.generation(), 1);
        vars.insert("new", Binding::data(Value::Num(3.0), true, true));
        assert_eq!(vars.generation(), 2);
        assert!(vars.layout_binding_mut(&layout, 0).is_none());
        assert!(matches!(vars.get("a").unwrap().value, Value::Num(2.0)));
        let binding = vars.get("b").unwrap();
        assert!(!binding.mutable && !binding.initialized && binding.strict_immutable);
        assert!(matches!(vars.remove("new").unwrap().value, Value::Num(3.0)));
        assert_eq!(vars.keys().count(), 2);
        vars.clear();
        assert_eq!(vars.values().count(), 0);
        assert_eq!(vars.generation(), 4);
    }

    #[test]
    fn generation_wrap_does_not_revive_a_pristine_layout() {
        let layout = BindingLayout::new([Rc::from("a")]);
        let mut vars = VarMap::from_layout(layout);
        vars.generation.set(u32::MAX);
        vars.insert("new", Binding::data(Value::Undefined, true, true));
        assert_eq!(vars.generation(), 1);
    }
}
