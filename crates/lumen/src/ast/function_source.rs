//! Exact source views share immutable storage instead of copying every nested function's text.
use std::{ops::Deref, rc::Rc};

#[derive(Debug)]
enum Backing {
    Text(String),
    Snapshot {
        bytes: Rc<[u8]>,
        start: usize,
        end: usize,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SharedSource(Rc<Backing>);

impl SharedSource {
    pub(crate) fn text(text: String) -> Self {
        Self(Rc::new(Backing::Text(text)))
    }

    /// `text` is the validated UTF-8 view returned by the snapshot reader. Check that it names
    /// this exact immutable backing range without scanning its contents a second time.
    pub(crate) fn snapshot(bytes: Rc<[u8]>, start: usize, text: &str) -> Self {
        let end = start
            .checked_add(text.len())
            .expect("source range overflow");
        assert!(bytes
            .get(start..end)
            .is_some_and(|range| { std::ptr::eq(range.as_ptr(), text.as_ptr()) }));
        Self(Rc::new(Backing::Snapshot { bytes, start, end }))
    }

    fn bytes(&self) -> &[u8] {
        match &*self.0 {
            Backing::Text(text) => text.as_bytes(),
            Backing::Snapshot { bytes, start, end } => &bytes[*start..*end],
        }
    }

    pub(crate) fn checked_range(&self, start: usize, end: usize) -> Option<FunctionSource> {
        let bytes = self.bytes();
        let boundary = |index| {
            index == bytes.len() || bytes.get(index).is_some_and(|byte| byte & 0xc0 != 0x80)
        };
        if start > end || end > bytes.len() || !boundary(start) || !boundary(end) {
            return None;
        }
        Some(self.validated_range(start, end))
    }

    fn range(&self, start: usize, end: usize) -> &str {
        match &*self.0 {
            Backing::Text(text) => &text[start..end],
            Backing::Snapshot { .. } => {
                std::str::from_utf8(&self.bytes()[start..end]).expect("validated function source")
            }
        }
    }

    /// Called with a UTF-8 range already checked by the parser or snapshot reader. The owned
    /// fallback preserves large inputs without narrowing their offsets to the compact fields.
    pub(crate) fn validated_range(&self, start: usize, end: usize) -> FunctionSource {
        debug_assert!(start <= end);
        debug_assert_eq!(self.range(start, end).len(), end - start);
        match (u32::try_from(start), u32::try_from(end)) {
            (Ok(start), Ok(end)) => FunctionSource(Storage::Shared {
                source: self.clone(),
                start,
                end,
            }),
            _ => FunctionSource::owned(Rc::from(self.range(start, end))),
        }
    }
}

#[derive(Debug, Clone)]
enum Storage {
    Owned(Rc<str>),
    Shared {
        source: SharedSource,
        start: u32,
        end: u32,
    },
}

#[derive(Clone)]
pub struct FunctionSource(Storage);

impl std::fmt::Debug for FunctionSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, formatter)
    }
}

impl FunctionSource {
    pub(crate) fn owned(text: Rc<str>) -> Self {
        Self(Storage::Owned(text))
    }

    pub(crate) fn root_key(&self) -> (u8, usize) {
        match &self.0 {
            Storage::Owned(text) => (0, Rc::as_ptr(text) as *const () as usize),
            Storage::Shared { source, .. } => (1, Rc::as_ptr(&source.0) as usize),
        }
    }

    pub(crate) fn root(&self) -> Self {
        match &self.0 {
            Storage::Owned(_) => self.clone(),
            Storage::Shared { source, .. } => source.validated_range(0, source.bytes().len()),
        }
    }

    pub(crate) fn offsets(&self) -> (usize, usize) {
        match &self.0 {
            Storage::Owned(text) => (0, text.len()),
            Storage::Shared { start, end, .. } => (*start as usize, *end as usize),
        }
    }
}

impl Deref for FunctionSource {
    type Target = str;

    fn deref(&self) -> &str {
        match &self.0 {
            Storage::Owned(text) => text,
            Storage::Shared { source, start, end } => source.range(*start as usize, *end as usize),
        }
    }
}
