//! Export the CLI binary's dynamic symbols so that native addons (`.node` files) loaded at
//! runtime via `dlopen` can resolve their undefined `napi_*` references against the host — the
//! same mechanism the real `node` binary relies on. Without this the linker keeps the `napi_*`
//! symbols internal and an addon fails to load.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        // macOS `ld`: export all global symbols into the dynamic table.
        "macos" => println!("cargo:rustc-link-arg-bins=-Wl,-export_dynamic"),
        // GNU/BSD `ld`: the `--export-dynamic` (a.k.a. `-rdynamic`) equivalent.
        "linux" => println!("cargo:rustc-link-arg-bins=-Wl,--export-dynamic"),
        _ => {}
    }
}
