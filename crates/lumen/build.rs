// Include every engine source in the cache identity: parser fixes must invalidate old ASTs
// even when the package version is unchanged in a development checkout.
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::Path,
};
fn hash_tree(dir: &Path, hash: &mut DefaultHasher) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            hash_tree(&path, hash);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            path.hash(hash);
            std::fs::read(path).unwrap().hash(hash);
        }
    }
}
fn main() {
    println!("cargo:rerun-if-changed=src");
    let mut hash = DefaultHasher::new();
    hash_tree(Path::new("src"), &mut hash);
    println!(
        "cargo:rustc-env=LUMEN_PARSE_CACHE_BUILD_ID={:016x}",
        hash.finish()
    );
}
