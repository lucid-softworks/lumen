// ---- node:repl --------------------------------------------------------------------------------
// The interactive REPL lives in the Rust `lumen-repl` crate and drives the terminal directly; it
// is not reachable from this JS glue (no op to hand JS a live line-editing session). So the shape
// and constants are real — `REPL_MODE_*`, `writer` (util.inspect), `builtinModules`, a `Recoverable`
// error class — while the machinery that would spin up an interactive session throws honestly.
{
  const util = __builtins.get("util");

  const REPL_MODE_SLOPPY = Symbol("repl-sloppy");
  const REPL_MODE_STRICT = Symbol("repl-strict");

  // Thrown by an eval hook to signal "input is incomplete, keep reading". Real class so custom
  // eval functions can construct/instanceof it as in Node.
  class Recoverable extends Error {
    constructor(err) {
      super(err && err.message);
      this.name = "Recoverable";
      this.err = err;
    }
  }

  // repl.writer(value) — how the REPL renders results. util.inspect is the real thing.
  const writer = (value) => util.inspect(value, writer.options);
  writer.options = { ...util.inspect.defaultOptions };

  const notImpl = () => {
    throw new Error("node:repl interactive sessions are not supported in lumen");
  };

  class REPLServer {
    constructor() {
      notImpl();
    }
  }

  const start = () => notImpl();

  const repl = {
    REPLServer,
    start,
    writer,
    Recoverable,
    REPL_MODE_SLOPPY,
    REPL_MODE_STRICT,
  };
  // Node exposes `builtinModules` non-enumerably, so it does not widen Object.keys(repl). Read it
  // lazily: `node:module` registers after this file in the glue order.
  Object.defineProperty(repl, "builtinModules", {
    enumerable: false,
    configurable: true,
    get() {
      const m = __builtins.get("module");
      return m ? m.builtinModules : [];
    },
  });

  __builtins.set("repl", repl);
}
