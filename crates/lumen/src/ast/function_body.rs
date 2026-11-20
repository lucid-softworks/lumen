//! Function bodies decoded from snapshots can stay encoded until execution or analysis needs them.
use super::Stmt;
use std::cell::OnceCell;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Default)]
pub struct FunctionBody {
    parsed: OnceCell<Vec<Stmt>>,
    // Keep the ordinary parsed-body representation small; only cached bodies need this metadata.
    deferred: Option<Box<crate::snapshot::DeferredBody>>,
}

impl FunctionBody {
    pub(crate) fn deferred(body: crate::snapshot::DeferredBody) -> Self {
        Self {
            parsed: OnceCell::new(),
            deferred: Some(Box::new(body)),
        }
    }

    fn materialize(&self) -> &Vec<Stmt> {
        self.parsed.get_or_init(|| {
            self.deferred
                .as_ref()
                .map_or_else(Vec::new, |body| body.decode())
        })
    }

    #[cfg(test)]
    pub(crate) fn is_materialized(&self) -> bool {
        self.parsed.get().is_some()
    }
}

impl From<Vec<Stmt>> for FunctionBody {
    fn from(body: Vec<Stmt>) -> Self {
        Self {
            parsed: OnceCell::from(body),
            deferred: None,
        }
    }
}

impl FromIterator<Stmt> for FunctionBody {
    fn from_iter<T: IntoIterator<Item = Stmt>>(iter: T) -> Self {
        Vec::from_iter(iter).into()
    }
}

impl Deref for FunctionBody {
    type Target = Vec<Stmt>;
    fn deref(&self) -> &Self::Target {
        self.materialize()
    }
}

impl DerefMut for FunctionBody {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.materialize();
        self.parsed.get_mut().expect("body materialized")
    }
}

impl<'a> IntoIterator for &'a FunctionBody {
    type Item = &'a Stmt;
    type IntoIter = std::slice::Iter<'a, Stmt>;
    fn into_iter(self) -> Self::IntoIter {
        self.materialize().iter()
    }
}

impl IntoIterator for FunctionBody {
    type Item = Stmt;
    type IntoIter = std::vec::IntoIter<Stmt>;
    fn into_iter(self) -> Self::IntoIter {
        self.parsed
            .into_inner()
            .unwrap_or_else(|| self.deferred.map_or_else(Vec::new, |body| body.decode()))
            .into_iter()
    }
}

impl std::fmt::Debug for FunctionBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.materialize().fmt(f)
    }
}
