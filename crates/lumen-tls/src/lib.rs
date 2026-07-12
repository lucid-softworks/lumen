//! Dependency-free TLS client transport loaded from the operating system's OpenSSL installation.
//!
//! OpenSSL is discovered at runtime with `dlopen`; it is not linked and is not a Cargo dependency.
//! The backend verifies the platform trust store and hostname and refuses to connect if either
//! verification facility is unavailable.

#![cfg(unix)]

use std::ffi::CString;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::raw::{c_char, c_int, c_long, c_void};

type SslMethod = c_void;
type SslCtx = c_void;
type Ssl = c_void;

type InitSsl = unsafe extern "C" fn(u64, *const c_void) -> c_int;
type ClientMethod = unsafe extern "C" fn() -> *const SslMethod;
type CtxNew = unsafe extern "C" fn(*const SslMethod) -> *mut SslCtx;
type CtxFree = unsafe extern "C" fn(*mut SslCtx);
type CtxDefaultPaths = unsafe extern "C" fn(*mut SslCtx) -> c_int;
type CtxSetVerify = unsafe extern "C" fn(*mut SslCtx, c_int, *const c_void);
type SslNew = unsafe extern "C" fn(*mut SslCtx) -> *mut Ssl;
type SslFree = unsafe extern "C" fn(*mut Ssl);
type SslSetFd = unsafe extern "C" fn(*mut Ssl, c_int) -> c_int;
type SslCtrl = unsafe extern "C" fn(*mut Ssl, c_int, c_long, *mut c_void) -> c_long;
type SslSetHost = unsafe extern "C" fn(*mut Ssl, *const c_char) -> c_int;
type SslConnect = unsafe extern "C" fn(*mut Ssl) -> c_int;
type SslRead = unsafe extern "C" fn(*mut Ssl, *mut c_void, c_int) -> c_int;
type SslWrite = unsafe extern "C" fn(*mut Ssl, *const c_void, c_int) -> c_int;
type SslShutdown = unsafe extern "C" fn(*mut Ssl) -> c_int;
type SslGetError = unsafe extern "C" fn(*const Ssl, c_int) -> c_int;
type VerifyResult = unsafe extern "C" fn(*const Ssl) -> c_long;
type ErrGetError = unsafe extern "C" fn() -> u64;
type ErrErrorString = unsafe extern "C" fn(u64, *mut c_char, usize);

struct Api {
    _ssl_lib: Library,
    _crypto_lib: Library,
    ctx_new: CtxNew,
    ctx_free: CtxFree,
    ctx_default_paths: CtxDefaultPaths,
    ctx_set_verify: CtxSetVerify,
    ssl_new: SslNew,
    ssl_free: SslFree,
    ssl_set_fd: SslSetFd,
    ssl_ctrl: SslCtrl,
    ssl_set_host: SslSetHost,
    ssl_connect: SslConnect,
    ssl_read: SslRead,
    ssl_write: SslWrite,
    ssl_shutdown: SslShutdown,
    ssl_get_error: SslGetError,
    verify_result: VerifyResult,
    err_get_error: ErrGetError,
    err_error_string: ErrErrorString,
}

impl Api {
    fn load() -> Result<Self, String> {
        let crypto = Library::open_candidates(crypto_candidates())?;
        let ssl = Library::open_candidates(ssl_candidates())?;
        unsafe {
            let init: InitSsl = ssl.function("OPENSSL_init_ssl")?;
            if init(0, std::ptr::null()) != 1 { return Err("OPENSSL_init_ssl failed".into()); }
            let method: ClientMethod = ssl.function("TLS_client_method")?;
            let ctx_new: CtxNew = ssl.function("SSL_CTX_new")?;
            // Ensure the method symbol is callable before returning the table.
            if method().is_null() { return Err("TLS_client_method returned null".into()); }
            Ok(Self {
                ctx_new,
                ctx_free: ssl.function("SSL_CTX_free")?,
                ctx_default_paths: ssl.function("SSL_CTX_set_default_verify_paths")?,
                ctx_set_verify: ssl.function("SSL_CTX_set_verify")?,
                ssl_new: ssl.function("SSL_new")?,
                ssl_free: ssl.function("SSL_free")?,
                ssl_set_fd: ssl.function("SSL_set_fd")?,
                ssl_ctrl: ssl.function("SSL_ctrl")?,
                ssl_set_host: ssl.function("SSL_set1_host")?,
                ssl_connect: ssl.function("SSL_connect")?,
                ssl_read: ssl.function("SSL_read")?,
                ssl_write: ssl.function("SSL_write")?,
                ssl_shutdown: ssl.function("SSL_shutdown")?,
                ssl_get_error: ssl.function("SSL_get_error")?,
                verify_result: ssl.function("SSL_get_verify_result")?,
                err_get_error: crypto.function("ERR_get_error")?,
                err_error_string: crypto.function("ERR_error_string_n")?,
                _ssl_lib: ssl,
                _crypto_lib: crypto,
            })
        }
    }
}

pub struct TlsStream {
    stream: TcpStream,
    api: Api,
    context: *mut SslCtx,
    ssl: *mut Ssl,
}

impl TlsStream {
    pub fn connect(stream: TcpStream, hostname: &str) -> Result<Self, String> {
        let api = Api::load()?;
        let method: ClientMethod = unsafe { api._ssl_lib.function("TLS_client_method")? };
        let context = unsafe { (api.ctx_new)(method()) };
        if context.is_null() { return Err("SSL_CTX_new failed".into()); }
        unsafe { (api.ctx_set_verify)(context, 1, std::ptr::null()) };
        if unsafe { (api.ctx_default_paths)(context) } != 1 {
            unsafe { (api.ctx_free)(context) };
            return Err("OpenSSL could not load default CA paths".into());
        }
        let ssl = unsafe { (api.ssl_new)(context) };
        if ssl.is_null() {
            unsafe { (api.ctx_free)(context) };
            return Err("SSL_new failed".into());
        }
        let host = CString::new(hostname).map_err(|_| "TLS hostname contains NUL".to_string())?;
        const SSL_CTRL_SET_TLSEXT_HOSTNAME: c_int = 55;
        const TLSEXT_NAMETYPE_HOST_NAME: c_long = 0;
        if unsafe { (api.ssl_ctrl)(ssl, SSL_CTRL_SET_TLSEXT_HOSTNAME, TLSEXT_NAMETYPE_HOST_NAME, host.as_ptr() as *mut _) } != 1
            || unsafe { (api.ssl_set_host)(ssl, host.as_ptr()) } != 1
            || unsafe { (api.ssl_set_fd)(ssl, stream.as_raw_fd()) } != 1
        {
            unsafe { (api.ssl_free)(ssl); (api.ctx_free)(context); }
            return Err("failed to configure TLS hostname or socket".into());
        }
        let result = unsafe { (api.ssl_connect)(ssl) };
        if result != 1 {
            let code = unsafe { (api.ssl_get_error)(ssl, result) };
            let detail = api.error_queue();
            unsafe { (api.ssl_free)(ssl); (api.ctx_free)(context); }
            return Err(format!("TLS handshake failed (SSL error {code}: {detail})"));
        }
        let verify = unsafe { (api.verify_result)(ssl) };
        if verify != 0 {
            unsafe { (api.ssl_free)(ssl); (api.ctx_free)(context); }
            return Err(format!("TLS certificate verification failed ({verify})"));
        }
        Ok(Self { stream, api, context, ssl })
    }

    fn io_error(&self, result: c_int) -> std::io::Error {
        let code = unsafe { (self.api.ssl_get_error)(self.ssl, result) };
        std::io::Error::other(format!("OpenSSL I/O error {code}: {}", self.api.error_queue()))
    }
}

impl Api {
    fn error_queue(&self) -> String {
        let mut messages = Vec::new();
        loop {
            let code = unsafe { (self.err_get_error)() };
            if code == 0 { break; }
            let mut buffer = [0i8; 256];
            unsafe { (self.err_error_string)(code, buffer.as_mut_ptr(), buffer.len()) };
            messages.push(unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned());
        }
        if messages.is_empty() { "no OpenSSL detail".into() } else { messages.join("; ") }
    }
}

impl Read for TlsStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let length = buffer.len().min(c_int::MAX as usize);
        let result = unsafe { (self.api.ssl_read)(self.ssl, buffer.as_mut_ptr() as *mut _, length as c_int) };
        if result > 0 { Ok(result as usize) } else if result == 0 { Ok(0) } else { Err(self.io_error(result)) }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let length = buffer.len().min(c_int::MAX as usize);
        let result = unsafe { (self.api.ssl_write)(self.ssl, buffer.as_ptr() as *const _, length as c_int) };
        if result > 0 { Ok(result as usize) } else { Err(self.io_error(result)) }
    }
    fn flush(&mut self) -> std::io::Result<()> { self.stream.flush() }
}

impl Drop for TlsStream {
    fn drop(&mut self) {
        unsafe {
            (self.api.ssl_shutdown)(self.ssl);
            (self.api.ssl_free)(self.ssl);
            (self.api.ctx_free)(self.context);
        }
    }
}

fn ssl_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    { &["/opt/homebrew/lib/libssl.3.dylib", "/usr/local/lib/libssl.3.dylib", "libssl.3.dylib"] }
    #[cfg(target_os = "linux")]
    { &["libssl.so.3", "libssl.so"] }
}
fn crypto_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    { &["/opt/homebrew/lib/libcrypto.3.dylib", "/usr/local/lib/libcrypto.3.dylib", "libcrypto.3.dylib"] }
    #[cfg(target_os = "linux")]
    { &["libcrypto.so.3", "libcrypto.so"] }
}

struct Library { handle: *mut c_void }
impl Library {
    fn open_candidates(paths: &[&str]) -> Result<Self, String> {
        let mut errors = Vec::new();
        for path in paths {
            match Self::open(path) { Ok(lib) => return Ok(lib), Err(error) => errors.push(error) }
        }
        Err(format!("no compatible OpenSSL library found: {}", errors.join("; ")))
    }
    fn open(path: &str) -> Result<Self, String> {
        let path = CString::new(path).map_err(|_| "library path contains NUL".to_string())?;
        let handle = unsafe { dlopen(path.as_ptr(), 2) };
        if handle.is_null() { Err(format!("cannot load {}", path.to_string_lossy())) } else { Ok(Self { handle }) }
    }
    unsafe fn function<T: Copy>(&self, name: &str) -> Result<T, String> {
        let name = CString::new(name).map_err(|_| "symbol contains NUL".to_string())?;
        let symbol = dlsym(self.handle, name.as_ptr());
        if symbol.is_null() { return Err(format!("missing OpenSSL symbol {}", name.to_string_lossy())); }
        Ok(std::mem::transmute_copy(&symbol))
    }
}
impl Drop for Library { fn drop(&mut self) { unsafe { dlclose(self.handle); } } }

extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loads_verified_client_api() { Api::load().expect("system OpenSSL should load"); }
}
