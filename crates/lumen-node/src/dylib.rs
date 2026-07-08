//! Minimal dynamic-library loader over the OS dynamic linker, reached through raw FFI
//! declarations (unix `dlopen`/`dlsym`/`dlclose`, Windows `LoadLibraryA`/`GetProcAddress`/
//! `FreeLibrary`).
//!
//! No third-party crate: these live in libSystem (macOS) / libc+libdl (Linux) / kernel32
//! (Windows), all of which std already links into every Rust program. Declaring them via `extern`
//! is the same category as std's own libc-syscall FFI — no dependency is added to the build. This
//! is the substrate for loading Node native addons (`.node` files — ELF/Mach-O shared objects on
//! unix, DLLs on Windows) to implement N-API.

pub use imp::DynLib;

// ---------------------------------------------------------------------------------------------
// Unix: dlopen / dlsym / dlclose
// ---------------------------------------------------------------------------------------------
#[cfg(unix)]
mod imp {
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

    /// An open handle to a dynamic library. `dlclose`s on drop. Holds a raw pointer, so it is
    /// `!Send`/`!Sync` — it must stay on the loop thread that owns the engine, which is exactly
    /// where addon code runs.
    pub struct DynLib {
        handle: *mut c_void,
    }

    impl DynLib {
        /// `dlopen` a library by filesystem path.
        pub fn open(path: &str) -> Result<DynLib, String> {
            let c =
                CString::new(path).map_err(|_| "library path contains a NUL byte".to_string())?;
            // Clear any stale error state so a later `dlerror()` reflects only this call.
            unsafe { dlerror() };
            let handle = unsafe { dlopen(c.as_ptr(), RTLD_NOW) };
            if handle.is_null() {
                return Err(last_error().unwrap_or_else(|| format!("dlopen failed for '{path}'")));
            }
            Ok(DynLib { handle })
        }

        /// Resolve a symbol to its raw address. Returns `None` when the symbol is absent. A symbol
        /// can, in principle, legitimately resolve to a null address, so a null result is
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
}

// ---------------------------------------------------------------------------------------------
// Windows: LoadLibraryA / GetProcAddress / FreeLibrary
// ---------------------------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_void};

    // kernel32 is linked into every Windows Rust program by std; declaring these adds no
    // dependency. `LoadLibraryA` maps the DLL and eagerly resolves its import table, so — like
    // `RTLD_NOW` on unix — an addon referencing a missing symbol fails at load, not mid-callback.
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(lp_lib_file_name: *const c_char) -> *mut c_void;
        fn GetProcAddress(h_module: *mut c_void, lp_proc_name: *const c_char) -> *mut c_void;
        fn FreeLibrary(h_module: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    /// An open handle (`HMODULE`) to a dynamic library. `FreeLibrary`s on drop. `!Send`/`!Sync`
    /// like the unix handle — addon code runs on the engine's loop thread.
    pub struct DynLib {
        handle: *mut c_void,
    }

    impl DynLib {
        /// `LoadLibraryA` a DLL by filesystem path (a `.node` addon is a DLL on Windows).
        pub fn open(path: &str) -> Result<DynLib, String> {
            let c =
                CString::new(path).map_err(|_| "library path contains a NUL byte".to_string())?;
            let handle = unsafe { LoadLibraryA(c.as_ptr()) };
            if handle.is_null() {
                let code = unsafe { GetLastError() };
                return Err(format!("LoadLibrary failed for '{path}' (error {code})"));
            }
            Ok(DynLib { handle })
        }

        /// Resolve a symbol via `GetProcAddress`. Windows never resolves an export to a null
        /// address, so a null result unambiguously means "not found".
        pub fn symbol(&self, name: &str) -> Option<*mut c_void> {
            let c = CString::new(name).ok()?;
            let sym = unsafe { GetProcAddress(self.handle, c.as_ptr()) };
            if sym.is_null() {
                return None;
            }
            Some(sym)
        }
    }

    impl Drop for DynLib {
        fn drop(&mut self) {
            unsafe { FreeLibrary(self.handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_system_lib_and_resolves_symbols() {
        // A system library that is always present, plus one of its exports.
        #[cfg(target_os = "macos")]
        let (path, sym) = ("/usr/lib/libSystem.B.dylib", "malloc");
        #[cfg(target_os = "linux")]
        let (path, sym) = ("libc.so.6", "malloc");
        #[cfg(windows)]
        let (path, sym) = ("kernel32.dll", "GetProcAddress");

        let lib = DynLib::open(path).expect("open system library");
        assert!(lib.symbol(sym).is_some(), "{sym} should resolve");
        assert!(lib.symbol("__lumen_no_such_symbol__").is_none(), "bogus symbol must miss");
    }

    #[test]
    fn missing_library_errors() {
        assert!(DynLib::open("/no/such/library.that-does-not-exist").is_err());
    }
}
