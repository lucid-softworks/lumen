//! WebAssembly bindings for the lumen playground: a persistent [`Session`] wrapping one
//! [`lumen::Engine`], with the wall clock bridged to the host page's `Date.now()` (the engine's
//! `SystemTime` fallback panics on wasm32-unknown-unknown).

use wasm_bindgen::prelude::*;

/// Bridge the engine's wall clock to the embedding page.
fn js_clock_ms() -> f64 {
    js_sys::Date::now()
}

/// One engine realm, persistent across `eval` calls (a playground "session"). `reset()` on the JS
/// side is just dropping it and constructing a new one.
#[wasm_bindgen]
pub struct Session {
    engine: lumen::Engine,
}

#[wasm_bindgen]
impl Session {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Session {
        lumen::set_host_clock(js_clock_ms);
        let mut engine = lumen::Engine::new();
        // The $262 test harness object is meaningless in a playground, and its agent machinery
        // needs OS threads (a trap on wasm) — drop it from the global.
        let _ = engine.eval("delete globalThis.$262;", false);
        Session { engine }
    }

    /// Evaluate `src` as a script. Returns `{ ok, value?, error?, console: string[] }`:
    /// `value` is the last statement's value rendered to a string, `error` the thrown error or
    /// syntax error, and `console` everything written via `console.*` / `print` during the run.
    pub fn eval(&mut self, src: &str) -> JsValue {
        let result = self.engine.eval(src, false);
        let out = js_sys::Object::new();
        let console = js_sys::Array::new();
        for line in self.engine.take_console() {
            console.push(&JsValue::from_str(&line));
        }
        let set = |k: &str, v: &JsValue| {
            let _ = js_sys::Reflect::set(&out, &JsValue::from_str(k), v);
        };
        match result {
            Ok(lumen::Completion::Value(v)) => {
                set("ok", &JsValue::TRUE);
                set("value", &JsValue::from_str(&v));
            }
            Ok(lumen::Completion::Throw { name, message }) => {
                set("ok", &JsValue::FALSE);
                let text = if name.is_empty() {
                    format!("Uncaught: {message}")
                } else {
                    format!("Uncaught {name}: {message}")
                };
                set("error", &JsValue::from_str(&text));
            }
            Err(e) => {
                set("ok", &JsValue::FALSE);
                set(
                    "error",
                    &JsValue::from_str(&format!("SyntaxError (line {}): {}", e.line, e.message)),
                );
            }
        }
        set("console", &console);
        out.into()
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
