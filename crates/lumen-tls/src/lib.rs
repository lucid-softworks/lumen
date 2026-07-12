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
use std::time::Duration;

type SslMethod = c_void;
type SslCtx = c_void;
type Ssl = c_void;

type InitSsl = unsafe extern "C" fn(u64, *const c_void) -> c_int;
type ClientMethod = unsafe extern "C" fn() -> *const SslMethod;
type ServerMethod = unsafe extern "C" fn() -> *const SslMethod;
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
type SslAccept = unsafe extern "C" fn(*mut Ssl) -> c_int;
type BioNewMem = unsafe extern "C" fn(*const c_void, c_int) -> *mut c_void;
type BioFree = unsafe extern "C" fn(*mut c_void) -> c_int;
type PemRead = unsafe extern "C" fn(*mut c_void, *mut *mut c_void, *const c_void, *mut c_void) -> *mut c_void;
type CtxUseObject = unsafe extern "C" fn(*mut SslCtx, *mut c_void) -> c_int;
type CtxCheckKey = unsafe extern "C" fn(*const SslCtx) -> c_int;
type ObjectFree = unsafe extern "C" fn(*mut c_void);
type SslGetVersion = unsafe extern "C" fn(*const Ssl) -> *const c_char;
type SslGetCipher = unsafe extern "C" fn(*const Ssl) -> *const c_void;
type CipherGetName = unsafe extern "C" fn(*const c_void) -> *const c_char;
type SslSetAlpn = unsafe extern "C" fn(*mut Ssl, *const u8, u32) -> c_int;
type SslGetAlpn = unsafe extern "C" fn(*const Ssl, *mut *const u8, *mut u32);
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
    ssl_accept: SslAccept,
    ssl_read: SslRead,
    ssl_write: SslWrite,
    ssl_shutdown: SslShutdown,
    ssl_get_error: SslGetError,
    verify_result: VerifyResult,
    ssl_get_version: SslGetVersion,
    ssl_get_cipher: SslGetCipher,
    cipher_get_name: CipherGetName,
    ssl_set_alpn: SslSetAlpn,
    ssl_get_alpn: SslGetAlpn,
    bio_new_mem: BioNewMem,
    bio_free: BioFree,
    pem_read_cert: PemRead,
    pem_read_key: PemRead,
    ctx_use_cert: CtxUseObject,
    ctx_use_key: CtxUseObject,
    ctx_check_key: CtxCheckKey,
    cert_free: ObjectFree,
    key_free: ObjectFree,
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
                ssl_accept: ssl.function("SSL_accept")?,
                ssl_read: ssl.function("SSL_read")?,
                ssl_write: ssl.function("SSL_write")?,
                ssl_shutdown: ssl.function("SSL_shutdown")?,
                ssl_get_error: ssl.function("SSL_get_error")?,
                verify_result: ssl.function("SSL_get_verify_result")?,
                ssl_get_version: ssl.function("SSL_get_version")?,
                ssl_get_cipher: ssl.function("SSL_get_current_cipher")?,
                cipher_get_name: ssl.function("SSL_CIPHER_get_name")?,
                ssl_set_alpn: ssl.function("SSL_set_alpn_protos")?,
                ssl_get_alpn: ssl.function("SSL_get0_alpn_selected")?,
                bio_new_mem: crypto.function("BIO_new_mem_buf")?,
                bio_free: crypto.function("BIO_free")?,
                pem_read_cert: crypto.function("PEM_read_bio_X509")?,
                pem_read_key: crypto.function("PEM_read_bio_PrivateKey")?,
                ctx_use_cert: ssl.function("SSL_CTX_use_certificate")?,
                ctx_use_key: ssl.function("SSL_CTX_use_PrivateKey")?,
                ctx_check_key: ssl.function("SSL_CTX_check_private_key")?,
                cert_free: crypto.function("X509_free")?,
                key_free: crypto.function("EVP_PKEY_free")?,
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

// The stream owns its OpenSSL objects and is moved as one unit between blocking worker tasks. It
// is never accessed concurrently; OpenSSL permits an SSL connection to move between OS threads.
unsafe impl Send for TlsStream {}

impl TlsStream {
    pub fn connect(stream: TcpStream, hostname: &str) -> Result<Self, String> {
        Self::connect_with_options(stream, hostname, &[], true)
    }

    pub fn connect_with_alpn(stream: TcpStream, hostname: &str, protocols: &[String]) -> Result<Self, String> {
        Self::connect_with_options(stream, hostname, protocols, true)
    }

    pub fn connect_with_options(stream: TcpStream, hostname: &str, protocols: &[String], verify_peer: bool) -> Result<Self, String> {
        let api = Api::load()?;
        let method: ClientMethod = unsafe { api._ssl_lib.function("TLS_client_method")? };
        let context = unsafe { (api.ctx_new)(method()) };
        if context.is_null() { return Err("SSL_CTX_new failed".into()); }
        unsafe { (api.ctx_set_verify)(context, if verify_peer { 1 } else { 0 }, std::ptr::null()) };
        if verify_peer && unsafe { (api.ctx_default_paths)(context) } != 1 {
            unsafe { (api.ctx_free)(context) };
            return Err("OpenSSL could not load default CA paths".into());
        }
        let ssl = unsafe { (api.ssl_new)(context) };
        if ssl.is_null() {
            unsafe { (api.ctx_free)(context) };
            return Err("SSL_new failed".into());
        }
        let host = CString::new(hostname).map_err(|_| "TLS hostname contains NUL".to_string())?;
        let mut alpn = Vec::new();
        for protocol in protocols {
            if protocol.is_empty() || protocol.len() > u8::MAX as usize {
                unsafe { (api.ssl_free)(ssl); (api.ctx_free)(context); }
                return Err("TLS ALPN protocol names must contain 1 to 255 bytes".into());
            }
            alpn.push(protocol.len() as u8);
            alpn.extend_from_slice(protocol.as_bytes());
        }
        const SSL_CTRL_SET_TLSEXT_HOSTNAME: c_int = 55;
        const TLSEXT_NAMETYPE_HOST_NAME: c_long = 0;
        if (!alpn.is_empty() && unsafe { (api.ssl_set_alpn)(ssl, alpn.as_ptr(), alpn.len() as u32) } != 0)
            || unsafe { (api.ssl_ctrl)(ssl, SSL_CTRL_SET_TLSEXT_HOSTNAME, TLSEXT_NAMETYPE_HOST_NAME, host.as_ptr() as *mut _) } != 1
            || (verify_peer && unsafe { (api.ssl_set_host)(ssl, host.as_ptr()) } != 1)
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
        if verify_peer && verify != 0 {
            unsafe { (api.ssl_free)(ssl); (api.ctx_free)(context); }
            return Err(format!("TLS certificate verification failed ({verify})"));
        }
        Ok(Self { stream, api, context, ssl })
    }

    pub fn accept(stream: TcpStream, certificate_pem: &[u8], private_key_pem: &[u8]) -> Result<Self, String> {
        let api = Api::load()?;
        let method: ServerMethod = unsafe { api._ssl_lib.function("TLS_server_method")? };
        let context = unsafe { (api.ctx_new)(method()) };
        if context.is_null() { return Err("SSL_CTX_new failed".into()); }
        let certificate = match api.read_pem(certificate_pem, true) {
            Ok(value) => value,
            Err(error) => { unsafe { (api.ctx_free)(context) }; return Err(error); }
        };
        let private_key = match api.read_pem(private_key_pem, false) {
            Ok(value) => value,
            Err(error) => {
                unsafe { (api.cert_free)(certificate); (api.ctx_free)(context); }
                return Err(error);
            }
        };
        let configured = unsafe {
            let ok = (api.ctx_use_cert)(context, certificate) == 1
                && (api.ctx_use_key)(context, private_key) == 1
                && (api.ctx_check_key)(context) == 1;
            (api.cert_free)(certificate);
            (api.key_free)(private_key);
            ok
        };
        if !configured {
            let detail = api.error_queue();
            unsafe { (api.ctx_free)(context) };
            return Err(format!("TLS certificate/private key configuration failed: {detail}"));
        }
        let ssl = unsafe { (api.ssl_new)(context) };
        if ssl.is_null() {
            unsafe { (api.ctx_free)(context) };
            return Err("SSL_new failed".into());
        }
        if unsafe { (api.ssl_set_fd)(ssl, stream.as_raw_fd()) } != 1 {
            unsafe { (api.ssl_free)(ssl); (api.ctx_free)(context); }
            return Err("failed to configure TLS server socket".into());
        }
        let result = unsafe { (api.ssl_accept)(ssl) };
        if result != 1 {
            let code = unsafe { (api.ssl_get_error)(ssl, result) };
            let detail = api.error_queue();
            unsafe { (api.ssl_free)(ssl); (api.ctx_free)(context); }
            return Err(format!("TLS server handshake failed (SSL error {code}: {detail})"));
        }
        Ok(Self { stream, api, context, ssl })
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    pub fn protocol(&self) -> String {
        let value = unsafe { (self.api.ssl_get_version)(self.ssl) };
        if value.is_null() { String::new() } else { unsafe { std::ffi::CStr::from_ptr(value) }.to_string_lossy().into_owned() }
    }

    pub fn cipher(&self) -> String {
        let cipher = unsafe { (self.api.ssl_get_cipher)(self.ssl) };
        if cipher.is_null() { return String::new(); }
        let value = unsafe { (self.api.cipher_get_name)(cipher) };
        if value.is_null() { String::new() } else { unsafe { std::ffi::CStr::from_ptr(value) }.to_string_lossy().into_owned() }
    }

    pub fn alpn_protocol(&self) -> String {
        let mut data = std::ptr::null();
        let mut length = 0;
        unsafe { (self.api.ssl_get_alpn)(self.ssl, &mut data, &mut length) };
        if data.is_null() || length == 0 { return String::new(); }
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(data, length as usize) }).into_owned()
    }

    fn io_error(&self, result: c_int) -> std::io::Error {
        let code = unsafe { (self.api.ssl_get_error)(self.ssl, result) };
        if matches!(code, 2 | 3 | 5) {
            return std::io::Error::new(std::io::ErrorKind::Interrupted, "TLS operation should retry");
        }
        std::io::Error::other(format!("OpenSSL I/O error {code}: {}", self.api.error_queue()))
    }
}

impl Api {
    fn read_pem(&self, bytes: &[u8], certificate: bool) -> Result<*mut c_void, String> {
        let length = c_int::try_from(bytes.len()).map_err(|_| "PEM input is too large".to_string())?;
        let bio = unsafe { (self.bio_new_mem)(bytes.as_ptr() as *const _, length) };
        if bio.is_null() { return Err("BIO_new_mem_buf failed".into()); }
        let value = unsafe {
            let value = if certificate { (self.pem_read_cert)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut()) }
                else { (self.pem_read_key)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut()) };
            (self.bio_free)(bio);
            value
        };
        if value.is_null() { Err(format!("invalid PEM: {}", self.error_queue())) } else { Ok(value) }
    }

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
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(1);
    #[test]
    fn loads_verified_client_api() { Api::load().expect("system OpenSSL should load"); }

    #[test]
    fn accepts_local_tls_connection() {
        if Command::new("openssl").arg("version").output().is_err() { return; }
        let directory = std::env::temp_dir().join(format!("lumen-tls-server-{}-{}", std::process::id(), NEXT_DIR.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&directory).unwrap();
        let certificate = directory.join("cert.pem");
        let key = directory.join("key.pem");
        let generated = Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-sha256", "-days", "1", "-nodes"])
            .arg("-keyout").arg(&key).arg("-out").arg(&certificate)
            .args(["-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost"])
            .output().unwrap();
        assert!(generated.status.success());
        let cert_bytes = std::fs::read(&certificate).unwrap();
        let key_bytes = std::fs::read(&key).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut tls = TlsStream::accept(tcp, &cert_bytes, &key_bytes).unwrap();
            let mut request = [0u8; 4];
            tls.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"ping");
            tls.write_all(b"pong").unwrap();
        });
        let mut client = Command::new("openssl")
            .args(["s_client", "-quiet", "-connect", &format!("127.0.0.1:{port}"), "-servername", "localhost", "-CAfile"])
            .arg(&certificate).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        client.stdin.as_mut().unwrap().write_all(b"ping").unwrap();
        let output = client.wait_with_output().unwrap();
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(directory);
        assert!(output.status.success());
        assert_eq!(output.stdout, b"pong");
    }
}
