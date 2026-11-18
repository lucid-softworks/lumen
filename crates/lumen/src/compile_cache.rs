//! Optional source-validated parse cache. A miss or unreadable/corrupt entry always parses
//! the original source. Entries contain ASTs, never executable machine code or live values.
use crate::{ast::Stmt, parser::ParseError};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAGIC: &[u8; 8] = b"LUMAST01";
const MAX_ENTRY: usize = 64 * 1024 * 1024;

fn hash(value: impl Hash) -> u64 {
    let mut hash = DefaultHasher::new();
    value.hash(&mut hash);
    hash.finish()
}

pub(crate) struct CompileCache {
    directory: PathBuf,
}
impl CompileCache {
    pub(crate) fn new(root: &Path) -> std::io::Result<Self> {
        let directory = root.join(format!(
            "lumen-{}-{}-{}",
            env!("LUMEN_PARSE_CACHE_BUILD_ID"),
            std::env::consts::ARCH,
            std::env::consts::OS
        ));
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        let directory = std::fs::canonicalize(directory)?;
        let metadata = std::fs::metadata(&directory)?;
        if !private(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "compile cache directory must be private to its owner",
            ));
        }
        // Detect unwritable directories now, so enableCompileCache reports FAILED honestly.
        let probe = directory.join(format!("probe-{}-{}", std::process::id(), unique()));
        let file = private_create(&probe)?;
        drop(file);
        std::fs::remove_file(probe)?;
        Ok(Self { directory })
    }
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }
    fn path(&self, source: &str, module: bool) -> PathBuf {
        self.directory
            .join(format!("{:016x}.ast", hash((source, module))))
    }
    pub(crate) fn parse(&self, source: &str, module: bool) -> Result<Vec<Stmt>, ParseError> {
        let path = self.path(source, module);
        if let Some(body) = self.read(&path, source, module) {
            return Ok(body);
        }
        let body = parse(source, module)?;
        self.write(&path, source, module, &body);
        Ok(body)
    }
    fn read(&self, path: &Path, source: &str, module: bool) -> Option<Vec<Stmt>> {
        let file = std::fs::File::open(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || !private(&metadata) || metadata.len() > MAX_ENTRY as u64 {
            return None;
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_ENTRY as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > MAX_ENTRY || bytes.get(..8)? != MAGIC || *bytes.get(8)? != u8::from(module)
        {
            return None;
        }
        let source_len = u64::from_le_bytes(bytes.get(9..17)?.try_into().ok()?);
        if source_len != source.len() as u64 {
            return None;
        }
        let checksum = u64::from_le_bytes(bytes.get(17..25)?.try_into().ok()?);
        let source_end = 25usize.checked_add(source.len())?;
        // Comparing the full source makes filename-hash collisions a cache miss.
        if bytes.get(25..source_end)? != source.as_bytes() {
            return None;
        }
        let payload = bytes.get(source_end..)?;
        if hash(payload) != checksum {
            return None;
        }
        crate::snapshot::decode(payload).ok()
    }
    fn write(&self, path: &Path, source: &str, module: bool, body: &[Stmt]) {
        if source.len() > MAX_ENTRY / 4 {
            return;
        }
        let Some(payload) = crate::snapshot::encode_limited(body, MAX_ENTRY - 25 - source.len())
        else {
            return;
        };
        // Exclusive private temp files plus rename avoid truncated entries and following a
        // pre-existing cache-file symlink on writes; readers retain the opened inode.
        let temp = self
            .directory
            .join(format!("entry-{}-{}.tmp", std::process::id(), unique()));
        let result = (|| -> std::io::Result<()> {
            let mut file = private_create(&temp)?;
            file.write_all(MAGIC)?;
            file.write_all(&[u8::from(module)])?;
            file.write_all(&(source.len() as u64).to_le_bytes())?;
            file.write_all(&hash(&payload[..]).to_le_bytes())?;
            file.write_all(source.as_bytes())?;
            file.write_all(&payload)?;
            drop(file);
            std::fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
    }
}

fn unique() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
fn private_create(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn private(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        metadata.uid() == unsafe { geteuid() } && metadata.mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

pub(crate) fn parse(source: &str, module: bool) -> Result<Vec<Stmt>, ParseError> {
    if module {
        crate::parser::parse_module(source)
    } else {
        crate::parser::parse_script(source, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "lumen-cache-test-{}-{}",
                std::process::id(),
                unique()
            )))
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cached_programs_preserve_execution_and_function_source() {
        let root = Directory::new();
        let cache = CompileCache::new(&root.0).unwrap();
        let source = "function f(a=3) { /* λ */ return a*7; } f()+':'+f.toString()";
        for _ in 0..2 {
            let body = cache
                .parse(source, false)
                .unwrap_or_else(|e| panic!("{}", e.message));
            let mut engine = crate::Engine::new();
            let result = engine
                .interp
                .run_program(&body)
                .ok()
                .expect("program succeeds");
            assert_eq!(
                match &result {
                    crate::value::Value::Str(s) => &**s,
                    _ => panic!("expected string"),
                },
                "21:function f(a=3) { /* λ */ return a*7; }"
            );
        }
        assert!(cache
            .read(&cache.path(source, false), source, false)
            .is_some());
        assert!(cache
            .read(&cache.path(source, false), source, true)
            .is_none());
        assert!(cache
            .read(&cache.path(source, false), "different source", false)
            .is_none());
    }

    #[test]
    fn corrupt_missing_or_unwritable_entries_fall_back_to_source() {
        let root = Directory::new();
        let cache = CompileCache::new(&root.0).unwrap();
        let source = "1+2";
        cache
            .parse(source, false)
            .unwrap_or_else(|e| panic!("{}", e.message));
        let path = cache.path(source, false);
        let original = std::fs::read(&path).unwrap();
        for corruption in [vec![], original[..12].to_vec(), {
            let mut bytes = original.clone();
            *bytes.last_mut().unwrap() ^= 1;
            bytes
        }] {
            std::fs::write(&path, corruption).unwrap();
            let body = cache
                .parse(source, false)
                .unwrap_or_else(|e| panic!("{}", e.message));
            let mut engine = crate::Engine::new();
            assert_eq!(
                engine
                    .interp
                    .run_program(&body)
                    .ok()
                    .expect("program succeeds")
                    .as_num_opt(),
                Some(3.0)
            );
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap(); // read/rename fail; execution still succeeds
        assert!(cache.parse(source, false).is_ok());
        assert!(cache.parse("let = ;", false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cache_rejects_shared_directory_and_shared_entries() {
        use std::os::unix::fs::PermissionsExt;
        let root = Directory::new();
        let cache = CompileCache::new(&root.0).unwrap();
        cache
            .parse("2", false)
            .unwrap_or_else(|e| panic!("{}", e.message));
        let path = cache.path("2", false);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(cache.read(&path, "2", false).is_none());
        std::fs::set_permissions(cache.directory(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(CompileCache::new(&root.0).is_err());
    }
}
