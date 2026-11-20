//! Immutable identifier text. Short names occupy the AST payload instead of a separate allocation.
use std::{
    borrow::{Borrow, Cow},
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
};

const INLINE_CAPACITY: usize = 22;

#[derive(Clone)]
enum Storage {
    Inline {
        bytes: [u8; INLINE_CAPACITY],
        len: u8,
    },
    Heap(Box<str>),
}

#[derive(Clone)]
pub struct IdentName(Storage);

impl IdentName {
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Storage::Inline { bytes, len } => {
                // The private storage is constructed only by copying a complete valid str.
                // No API exposes mutable bytes, and clones preserve both bytes and length.
                unsafe { std::str::from_utf8_unchecked(&bytes[..*len as usize]) }
            }
            Storage::Heap(text) => text,
        }
    }
}

impl From<&str> for IdentName {
    fn from(text: &str) -> Self {
        if text.len() <= INLINE_CAPACITY {
            let mut bytes = [0; INLINE_CAPACITY];
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            Self(Storage::Inline {
                bytes,
                len: text.len() as u8,
            })
        } else {
            Self(Storage::Heap(text.into()))
        }
    }
}
impl From<String> for IdentName {
    fn from(text: String) -> Self {
        // Already-owned names stay heap-backed. A boxed slice keeps capacity metadata
        // out of the AST; copying short owned names inline would add another copy.
        Self(Storage::Heap(text.into_boxed_str()))
    }
}
impl From<Cow<'_, str>> for IdentName {
    fn from(text: Cow<'_, str>) -> Self {
        match text {
            Cow::Borrowed(text) => text.into(),
            Cow::Owned(text) => text.into(),
        }
    }
}
impl Deref for IdentName {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl AsRef<str> for IdentName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl Borrow<str> for IdentName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}
impl<T: AsRef<str> + ?Sized> PartialEq<T> for IdentName {
    fn eq(&self, other: &T) -> bool {
        self.as_str() == other.as_ref()
    }
}
impl Eq for IdentName {}
impl Hash for IdentName {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}
impl fmt::Debug for IdentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}
impl fmt::Display for IdentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn text_and_hashes_survive_cloning_and_source_reuse() {
        for unit in ["a", "é", "π", "𐐀", "aπ𐐀"] {
            for count in 0..40 {
                let mut source = unit.repeat(count);
                let expected = source.clone();
                let borrowed = IdentName::from(source.as_str());
                let owned = IdentName::from(source.clone());
                let cloned = borrowed.clone();
                source.clear();
                source.push_str("overwritten");
                drop(source);
                assert_eq!(borrowed.as_str(), expected);
                assert_eq!(owned, borrowed);
                assert_eq!(cloned, borrowed);
                let set = HashSet::from([borrowed, owned]);
                assert_eq!(set.len(), 1);
                assert!(set.contains(expected.as_str()));
            }
        }
    }
}
