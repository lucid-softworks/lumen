//! End-to-end `bun:ffi` tests: drive the real trampoline through JS against libc and a C fixture
//! compiled at test time. These cover value marshalling, CString/read/toArrayBuffer, mixed
//! int/float argument orders, 8-argument calls, and a JSCallback invoked from native code.
//!
//! The whole suite is skipped (returns early) if no C compiler is available, so it never fails a
//! build that lacks a toolchain.

use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

use lumen_runtime::{ConsoleOut, Runtime};

/// A console sink the test reads back after the loop runs.
#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.borrow().clone()).expect("utf8 console output")
    }
}

/// Locate a C compiler, honoring `$CC`. Returns `None` when none is usable (suite then skips).
fn c_compiler() -> Option<String> {
    let candidates = [std::env::var("CC").ok(), Some("cc".into()), Some("clang".into())];
    for c in candidates.into_iter().flatten() {
        if Command::new(&c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return Some(c);
        }
    }
    None
}

/// Compile the shared C fixture into the test's `OUT`/temp dir; returns its path.
fn build_fixture(cc: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lumen_ffi_fixture_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("fixture.c");
    std::fs::write(&src, FIXTURE_C).unwrap();
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(windows) {
        "dll"
    } else {
        "so"
    };
    let out = dir.join(format!("libfixture.{ext}"));
    let status = Command::new(cc)
        .args(["-shared", "-fPIC", "-O1"])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .expect("run C compiler");
    assert!(status.success(), "fixture failed to compile");
    out
}

const FIXTURE_C: &str = r#"
#include <stdint.h>
double mix_idfi(int a, double b, float c, int d) { return a + b + (double)c + d; }
double mix_ffii(float a, float b, int c, int d) { return (double)a + (double)b + c + d; }
double mix_difd(double a, int b, float c, double d) { return a + b + (double)c + d; }
float  ret_f32(float a, float b) { return a * b; }
int64_t sum8i(int64_t a,int64_t b,int64_t c,int64_t d,int64_t e,int64_t f,int64_t g,int64_t h){return a+b+c+d+e+f+g+h;}
double sum8mix(int a,double b,int c,double d,int e,double f,int g,double h){return a+b+c+d+e+f+g+h;}
int32_t add_via_ptr(const int32_t* p, int32_t k){ return *p + k; }
uint8_t ret_u8(void){ return 250; }
int8_t  ret_i8(void){ return -7; }
uint64_t ret_u64(void){ return 0xFFFFFFFFFFFFFFFFull; }
int apply_sum(int (*fn)(int), int n){ int s=0; for(int i=0;i<n;i++) s+=fn(i); return s; }
"#;

/// Run `src`, drive the loop to quiescence, and return captured stdout.
fn run(src: &str) -> String {
    let mut rt = Runtime::new();
    let out = Captured::default();
    let err = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(err.clone()),
    });
    match rt.eval(src).expect("parses") {
        lumen_runtime::Completion::Value(_) => {}
        lumen_runtime::Completion::Throw { name, message } => {
            panic!("uncaught {name}: {message}\nstderr: {}", err.text())
        }
    }
    rt.run_to_completion();
    out.text()
}

#[test]
fn ffi_libc_calls_and_marshalling() {
    let libc = if cfg!(target_os = "macos") { "libSystem.B.dylib" } else { "libc.so.6" };
    let out = run(&format!(
        r#"
        const {{ dlopen, FFIType: F, ptr, CString, read, toArrayBuffer }} = require("bun:ffi");
        const lib = dlopen("{libc}", {{
          strlen: {{ args: [F.ptr], returns: F.u64 }},
          atoi:   {{ args: [F.ptr], returns: F.i32 }},
          malloc: {{ args: [F.u64], returns: F.ptr }},
          free:   {{ args: [F.ptr], returns: F.void }},
          memcpy: {{ args: [F.ptr, F.ptr, F.u64], returns: F.ptr }},
          getenv: {{ args: [F.cstring], returns: F.cstring }},
          strstr: {{ args: [F.ptr, F.ptr], returns: F.ptr }},
        }});
        const s = lib.symbols, enc = new TextEncoder();
        const buf = enc.encode("hello\0");
        console.log("strlen", s.strlen(ptr(buf)), typeof s.strlen(ptr(buf)));
        console.log("strlenTA", s.strlen(buf));
        console.log("atoi", s.atoi(ptr(enc.encode("42abc\0"))));
        const p = s.malloc(32);
        s.memcpy(p, ptr(enc.encode("hi there\0")), 9);
        const cs = new CString(p);
        console.log("cstring", cs.toString(), cs.length, cs.byteLength, cs.ptr === p);
        console.log("cstring2", new CString(p, 0, 2).toString());
        console.log("toAB", new Uint8Array(toArrayBuffer(p, 0, 8)).join(","));
        console.log("read.u8", read.u8(p, 0), "read.i8", read.i8(p, 1));
        s.free(p);
        console.log("getenvMiss", JSON.stringify(s.getenv(ptr(enc.encode("LUMEN_NO_VAR\0"))).toString()));
        console.log("strstrMiss", s.strstr(ptr(enc.encode("abc\0")), ptr(enc.encode("zz\0"))));
        lib.close();
        "#
    ));
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines.contains(&"strlen 5 bigint"), "got: {out}");
    assert!(lines.contains(&"strlenTA 5"), "got: {out}");
    assert!(lines.contains(&"atoi 42"), "got: {out}");
    assert!(lines.contains(&"cstring hi there 8 undefined true"), "got: {out}");
    assert!(lines.contains(&"cstring2 hi"), "got: {out}");
    assert!(lines.contains(&"toAB 104,105,32,116,104,101,114,101"), "got: {out}");
    assert!(lines.contains(&"read.u8 104 read.i8 105"), "got: {out}");
    assert!(lines.contains(&"getenvMiss \"\""), "got: {out}");
    assert!(lines.contains(&"strstrMiss null"), "got: {out}");
}

#[test]
fn ffi_fixture_abi_shapes_and_callback() {
    let Some(cc) = c_compiler() else {
        eprintln!("no C compiler available; skipping bun:ffi fixture test");
        return;
    };
    let fixture = build_fixture(&cc);
    let path = fixture.to_string_lossy().replace('\\', "\\\\");
    let out = run(&format!(
        r#"
        const {{ dlopen, FFIType: F, ptr, JSCallback }} = require("bun:ffi");
        const lib = dlopen("{path}", {{
          mix_idfi: {{ args: [F.i32, F.f64, F.f32, F.i32], returns: F.f64 }},
          mix_ffii: {{ args: [F.f32, F.f32, F.i32, F.i32], returns: F.f64 }},
          mix_difd: {{ args: [F.f64, F.i32, F.f32, F.f64], returns: F.f64 }},
          ret_f32:  {{ args: [F.f32, F.f32], returns: F.f32 }},
          sum8i:    {{ args: [F.i64,F.i64,F.i64,F.i64,F.i64,F.i64,F.i64,F.i64], returns: F.i64 }},
          sum8mix:  {{ args: [F.i32,F.f64,F.i32,F.f64,F.i32,F.f64,F.i32,F.f64], returns: F.f64 }},
          add_via_ptr: {{ args: [F.ptr, F.i32], returns: F.i32 }},
          ret_u8: {{ args: [], returns: F.u8 }},
          ret_i8: {{ args: [], returns: F.i8 }},
          ret_u64: {{ args: [], returns: F.u64 }},
          apply_sum: {{ args: [F.function, F.i32], returns: F.i32 }},
        }});
        const s = lib.symbols;
        console.log("mix_idfi", s.mix_idfi(1, 2.5, 3.5, 4));
        console.log("mix_ffii", s.mix_ffii(1.5, 2.5, 3, 4));
        console.log("mix_difd", s.mix_difd(1.5, 2, 3.5, 4.5));
        console.log("ret_f32", s.ret_f32(2.5, 4));
        console.log("sum8i", s.sum8i(1n,2n,3n,4n,5n,6n,7n,8n), typeof s.sum8i(1n,2n,3n,4n,5n,6n,7n,8n));
        console.log("sum8mix", s.sum8mix(1,2,3,4,5,6,7,8));
        console.log("add_via_ptr", s.add_via_ptr(ptr(new Int32Array([100])), 23));
        console.log("ret_u8", s.ret_u8(), "ret_i8", s.ret_i8(), "ret_u64", s.ret_u64());
        const sq = new JSCallback((i) => i * i, {{ args: [F.i32], returns: F.i32 }});
        console.log("apply_sum", s.apply_sum(sq.ptr, 5));
        sq.close();
        lib.close();
        "#
    ));
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines.contains(&"mix_idfi 11"), "got: {out}");
    assert!(lines.contains(&"mix_ffii 11"), "got: {out}");
    assert!(lines.contains(&"mix_difd 11.5"), "got: {out}");
    assert!(lines.contains(&"ret_f32 10"), "got: {out}");
    assert!(lines.contains(&"sum8i 36 bigint"), "got: {out}");
    assert!(lines.contains(&"sum8mix 36"), "got: {out}");
    assert!(lines.contains(&"add_via_ptr 123"), "got: {out}");
    assert!(lines.contains(&"ret_u8 250 ret_i8 -7 ret_u64 18446744073709551615"), "got: {out}");
    assert!(lines.contains(&"apply_sum 30"), "got: {out}");
}

#[test]
fn ffi_honest_throws() {
    let out = run(
        r#"
        const { dlopen, FFIType: F, JSCallback, cc, viewSource } = require("bun:ffi");
        const libc = process.platform === "darwin" ? "libSystem.B.dylib" : "libc.so.6";
        const lib = dlopen(libc, { strlen: { args: [F.ptr], returns: F.u64 } });
        const check = (label, fn) => { try { fn(); console.log(label, "NO-THROW"); } catch (e) { console.log(label, e.message); } };
        // string passed where a pointer is required (Bun parity)
        check("strArg", () => lib.symbols.strlen("hello"));
        // float-typed JSCallback is unsupported
        check("floatCb", () => new JSCallback(() => 0, { args: [F.f64], returns: F.i32 }));
        check("cc", () => cc({}));
        check("viewSource", () => viewSource());
        "#,
    );
    assert!(
        out.contains("To convert a string to a pointer, encode it as a buffer"),
        "got: {out}"
    );
    assert!(out.contains("floatCb") && out.contains("floating-point"), "got: {out}");
    assert!(out.contains("cc") && out.contains("not supported in lumen"), "got: {out}");
}
