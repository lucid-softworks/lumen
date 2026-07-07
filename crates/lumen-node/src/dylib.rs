//! Minimal dynamic-library loader over the system dynamic linker (`dlopen`/`dlsym`/`dlclose`),
//! reached through raw `extern "C"` declarations.
//!
//! No third-party crate: `dlopen` and friends live in libSystem (macOS) / libc+libdl (Linux),
//! which std already links into every Rust program. Declaring them `extern "C"` is the same
//! category as std's own libc-syscall FFI — no dependency is added to the build. This is the
//! substrate for loading Node native addons (`.node` files) to implement N-API.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlerror() -> *mut c_char;
}

/// `RTLD_NOW` — resolve every undefined symbol before `dlopen` returns rather than lazily on
/// first call. The value is 2 on both macOS and Linux. We want eager resolution so a native
/// addon that references an `napi_*` symbol the host doesn't yet export fails loudly at load
/// time instead of crashing deep inside a callback.
const RTLD_NOW: c_int = 2;

/// An open handle to a dynamic library. `dlclose`s on drop.
///
/// Holds a raw pointer, so it is `!Send`/`!Sync` — it must stay on the loop thread that owns the
/// engine, which is exactly where addon code runs.
pub struct DynLib {
    handle: *mut c_void,
    /// Kept for diagnostics (error messages), not used for resolution.
    path: String,
}

impl DynLib {
    /// `dlopen` a library by filesystem path.
    pub fn open(path: &str) -> Result<DynLib, String> {
        let c = CString::new(path).map_err(|_| "library path contains a NUL byte".to_string())?;
        // Clear any stale error state so a later `dlerror()` reflects only this call.
        unsafe { dlerror() };
        let handle = unsafe { dlopen(c.as_ptr(), RTLD_NOW) };
        if handle.is_null() {
            return Err(last_error().unwrap_or_else(|| format!("dlopen failed for '{path}'")));
        }
        Ok(DynLib { handle, path: path.to_string() })
    }

    /// Resolve a symbol to its raw address. Returns `None` when the symbol is absent.
    ///
    /// A symbol can, in principle, legitimately resolve to a null address, so a null result is
    /// disambiguated from "not found" via `dlerror()`.
    pub fn symbol(&self, name: &str) -> Option<*mut c_void> {
        let c = CString::new(name).ok()?;
        unsafe { dlerror() };
        let sym = unsafe { dlsym(self.handle, c.as_ptr()) };
        if sym.is_null() && last_error().is_some() {
            return None;
        }
        Some(sym)
    }

    /// The path this library was opened from (for diagnostics).
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for DynLib {
    fn drop(&mut self) {
        unsafe { dlclose(self.handle) };
    }
}

/// The current dynamic-linker error message, if any. Consumes the error (POSIX `dlerror` is
/// one-shot).
fn last_error() -> Option<String> {
    let e = unsafe { dlerror() };
    if e.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_system_lib_and_resolves_symbols() {
        // The platform C library is always present and exports `malloc`.
        #[cfg(target_os = "macos")]
        let path = "/usr/lib/libSystem.B.dylib";
        #[cfg(target_os = "linux")]
        let path = "libc.so.6";

        let lib = DynLib::open(path).expect("open system C library");
        assert!(lib.symbol("malloc").is_some(), "malloc should resolve");
        assert!(lib.symbol("__lumen_no_such_symbol__").is_none(), "bogus symbol must miss");
    }

    #[test]
    fn missing_library_errors() {
        assert!(DynLib::open("/no/such/library.dylib").is_err());
    }
}
