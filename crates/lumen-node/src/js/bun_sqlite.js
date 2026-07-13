// bun:sqlite — Bun's embedded SQLite binding, for real.
//
// The engine is the *system* libsqlite3, dlopen'd at runtime by the native `__sqlite` ops (see
// sqlite.rs — no third-party crate, no build-time dependency). This file is the Bun-shaped API
// over those ops: Database / Statement / SQLiteError with Bun v1.2.21's verified semantics —
// query caching, named ($x, :x, @x) and positional bindings, strict mode, nested transactions via
// savepoints, per-statement safeIntegers, class-row mapping with .as(), and serialize/deserialize.
// Every behavior asserted here (error classes and messages, integer/real binding boundaries, type
// mappings) was checked against `bun -e` oracles. What the C API can't honestly provide
// (loadExtension, fileControl, setCustomSQLite) throws rather than pretending.
{
  const S = globalThis.__sqlite;

  // ---- SQLiteError ---------------------------------------------------------------------------
  // A real Error subclass, but constructable only by this module (Bun throws on user construction).
  let constructingError = false;
  class SQLiteError extends Error {
    constructor(message, code, errno) {
      if (!constructingError) {
        throw new Error("SQLiteError can only be constructed by bun:sqlite");
      }
      super(message);
      this.name = "SQLiteError";
      this.code = code;
      this.errno = errno === undefined ? 0 : errno;
    }
  }
  function makeSQLiteError(message, code, errno) {
    constructingError = true;
    try {
      return new SQLiteError(message, code, errno);
    } finally {
      constructingError = false;
    }
  }
  // Rethrow a native-op failure: errors tagged `__sqlite` become real SQLiteErrors (carrying the
  // engine's code/errno); everything else (TypeError, closed-handle Error) passes through.
  function rethrow(e) {
    if (e && e.__sqlite) throw makeSQLiteError(e.message, e.code, e.errno);
    throw e;
  }

  const INTERNAL = Symbol("bun:sqlite internal");

  // ---- parameter binding -----------------------------------------------------------------------

  function isBinary(v) {
    return typeof ArrayBuffer !== "undefined" && ArrayBuffer.isView
      ? ArrayBuffer.isView(v)
      : v instanceof Uint8Array;
  }

  // Bind one call's arguments onto a statement. Positional counts must match exactly; named
  // parameters come from a dict whose keys are bare in strict mode and prefixed-or-bare otherwise
  // (both verified against Bun). Returns true when a fresh parameter set was bound; false when no
  // params were passed (Bun then reuses the previous bindings).
  function bindParams(stmtId, args, strict) {
    // Normalize the call shape: a single array is the positional list; a single non-binary object
    // is a named dict; otherwise the varargs are the positional list.
    let named = null;
    let positional = null;
    if (args.length === 1 && Array.isArray(args[0])) {
      positional = args[0];
    } else if (
      args.length === 1 &&
      args[0] !== null &&
      typeof args[0] === "object" &&
      !isBinary(args[0])
    ) {
      named = args[0];
    } else if (args.length > 0) {
      positional = args;
    } else {
      return false;
    }

    const count = S.bindParameterCount(stmtId);
    if (count === 0) return false; // a parameterless statement ignores any arguments (Bun does)
    try {
      S.reset(stmtId, true); // rewind + clear old bindings before the fresh set
      if (positional !== null) {
        if (positional.length !== count) {
          throw new Error(`SQLite query expected ${count} values, received ${positional.length}`);
        }
        for (let i = 0; i < count; i++) S.bind(stmtId, i + 1, positional[i]);
        return true;
      }
      // Named dict. Strict mode looks up each parameter by its *bare* name ("a" for "$a") and
      // throws when it is missing; non-strict looks up only the exact source name ("$a" / ":a" /
      // "@a") and silently leaves missing parameters NULL. (Both oracle-verified: in non-strict
      // Bun, bare keys do NOT bind.)
      for (let i = 1; i <= count; i++) {
        const name = S.bindParameterName(stmtId, i);
        if (name === null) continue; // a positional ? inside a named statement stays NULL
        if (strict) {
          const bare = name.slice(1);
          if (Object.prototype.hasOwnProperty.call(named, bare)) {
            S.bind(stmtId, i, named[bare]);
          } else {
            throw new Error(`Missing parameter "${bare}"`);
          }
        } else if (Object.prototype.hasOwnProperty.call(named, name)) {
          S.bind(stmtId, i, named[name]);
        }
      }
      return true;
    } catch (e) {
      rethrow(e);
    }
  }

  // ---- Statement -------------------------------------------------------------------------------

  class Statement {
    constructor(token, db, id, sql) {
      if (token !== INTERNAL) {
        throw new TypeError(
          "Statement cannot be constructed directly (use Database.prototype.query)",
        );
      }
      this._db = db;
      this._id = id;
      this._sql = sql;
      this._alive = true;
      this._safeInts = db._safeInts;
      this._class = null;
      this._names = null; // cached column names
    }

    _guard() {
      if (!this._alive) throw new Error("Statement has finalized");
    }

    _rowNames() {
      if (this._names === null) {
        try {
          this._names = S.columnNames(this._id);
        } catch (e) {
          rethrow(e);
        }
      }
      return this._names;
    }

    // Build one row object (or mapped-class instance, for .as()) from the current native row.
    _rowObject(values) {
      const names = this._rowNames();
      const obj = this._class ? Object.create(this._class.prototype) : {};
      for (let i = 0; i < names.length; i++) obj[names[i]] = values[i];
      return obj;
    }

    _prepareRun(args) {
      this._guard();
      const bound = bindParams(this._id, args, this._db._strict);
      if (!bound) {
        // No new params: rewind only, keeping the previous bindings (Bun reuses them).
        try {
          S.reset(this._id, false);
        } catch (e) {
          rethrow(e);
        }
      }
    }

    get(...args) {
      this._prepareRun(args);
      try {
        if (S.step(this._id)) {
          const row = this._rowObject(S.row(this._id, this._safeInts));
          S.reset(this._id, false);
          return row;
        }
        S.reset(this._id, false);
        return null;
      } catch (e) {
        rethrow(e);
      }
    }

    all(...args) {
      this._prepareRun(args);
      try {
        const rows = [];
        while (S.step(this._id)) rows.push(this._rowObject(S.row(this._id, this._safeInts)));
        S.reset(this._id, false);
        return rows;
      } catch (e) {
        rethrow(e);
      }
    }

    values(...args) {
      this._prepareRun(args);
      try {
        const rows = [];
        while (S.step(this._id)) rows.push(S.row(this._id, this._safeInts));
        S.reset(this._id, false);
        return rows;
      } catch (e) {
        rethrow(e);
      }
    }

    run(...args) {
      this._prepareRun(args);
      try {
        // `changes` is the delta of the connection's total change count across this execution,
        // so a read-only statement reports 0 (Bun's semantics) instead of a stale last-write count.
        const before = S.totalChanges(this._db._id);
        while (S.step(this._id)) {
          /* drain (e.g. INSERT ... RETURNING) */
        }
        S.reset(this._id, false);
        return {
          changes: S.totalChanges(this._db._id) - before,
          lastInsertRowid: S.lastInsertRowid(this._db._id, this._safeInts),
        };
      } catch (e) {
        rethrow(e);
      }
    }

    iterate(...args) {
      this._prepareRun(args);
      const self = this;
      let done = false;
      return {
        next() {
          if (done) return { value: undefined, done: true };
          self._guard();
          try {
            if (S.step(self._id)) {
              return { value: self._rowObject(S.row(self._id, self._safeInts)), done: false };
            }
            S.reset(self._id, false);
            done = true;
            return { value: undefined, done: true };
          } catch (e) {
            done = true;
            rethrow(e);
          }
        },
        return(value) {
          if (!done) {
            done = true;
            try {
              S.reset(self._id, false);
            } catch (e) {
              /* statement already finalized */
            }
          }
          return { value, done: true };
        },
        [Symbol.iterator]() {
          return this;
        },
      };
    }

    [Symbol.iterator]() {
      return this.iterate();
    }

    as(Class) {
      this._class = Class;
      return this;
    }

    safeIntegers(on) {
      this._safeInts = on === undefined ? true : !!on;
      return this;
    }

    get columnNames() {
      this._guard();
      return this._rowNames().slice();
    }

    get paramsCount() {
      this._guard();
      try {
        return S.bindParameterCount(this._id);
      } catch (e) {
        rethrow(e);
      }
    }

    get native() {
      return { id: this._id };
    }

    toString() {
      if (!this._alive) return this._sql;
      try {
        return S.expandedSql(this._id);
      } catch (e) {
        return this._sql;
      }
    }

    finalize() {
      if (this._alive) {
        this._alive = false;
        try {
          S.finalize(this._id);
        } catch (e) {
          /* db already closed */
        }
      }
    }
  }

  // ---- Database --------------------------------------------------------------------------------

  const OPEN_READONLY = 1;
  const OPEN_READWRITE = 2;
  const OPEN_CREATE = 4;

  class Database {
    constructor(filename, options) {
      this._open = true;
      this._strict = false;
      this._safeInts = false;
      this._cache = new Map(); // sql -> Statement (the .query() cache)
      this._txDepth = 0;

      // `new Database(serializedBytes)` adopts a serialized image (a Bun-supported shape).
      if (filename !== undefined && filename !== null && isBinary(filename)) {
        try {
          this._id = S.deserialize(filename);
        } catch (e) {
          rethrow(e);
        }
        this.filename = ":memory:";
        return;
      }

      if (filename === undefined || filename === null) filename = ":memory:";
      filename = String(filename);

      // Flag mapping verified against Bun v1.2.21: an options *object* starts from 0 (so `{}`
      // and `{create:false}` reach sqlite with no access mode and fail with SQLITE_MISUSE, as in
      // Bun); truthy readonly/create/readwrite contribute their flags; and the presence of a
      // `strict`/`safeIntegers` key restores the default read-write mode when nothing else set one.
      let flags = OPEN_READWRITE | OPEN_CREATE;
      if (typeof options === "number") {
        flags = options;
      } else if (options !== null && typeof options === "object") {
        flags = 0;
        if (options.readonly) flags = OPEN_READONLY;
        // Assignment, not |=: `{readonly:true, create:true}` opens read-write in Bun.
        if (options.create) flags = OPEN_READWRITE | OPEN_CREATE;
        if (options.readwrite) flags |= OPEN_READWRITE;
        if (options.strict) this._strict = true;
        if (options.safeIntegers) this._safeInts = true;
        if (flags === 0 && ("strict" in options || "safeIntegers" in options)) {
          flags = OPEN_READWRITE | OPEN_CREATE;
        }
      }

      try {
        this._id = S.open(filename, flags);
      } catch (e) {
        rethrow(e);
      }
      this.filename = filename;
    }

    static open(filename, options) {
      return new Database(filename, options);
    }

    static deserialize(bytes, _options) {
      return new Database(bytes);
    }

    static setCustomSQLite(path) {
      return S.setCustomSQLite(String(path));
    }

    query(sql) {
      if (!this._open) throw new RangeError("Cannot use a closed database");
      sql = String(sql);
      let stmt = this._cache.get(sql);
      if (stmt && stmt._alive) return stmt;
      stmt = this.prepare(sql);
      this._cache.set(sql, stmt);
      return stmt;
    }

    prepare(sql) {
      if (!this._open) throw new RangeError("Cannot use a closed database");
      sql = String(sql);
      let id;
      try {
        id = S.prepare(this._id, sql);
      } catch (e) {
        rethrow(e);
      }
      return new Statement(INTERNAL, this, id, sql);
    }

    run(sql, ...params) {
      if (!this._open) throw new Error("Database has closed");
      if (params.length === 0) {
        // No bindings: the whole (possibly multi-statement) string runs through sqlite3_exec.
        try {
          const r = S.exec(this._id, String(sql));
          if (this._safeInts) r.lastInsertRowid = BigInt(r.lastInsertRowid);
          return r;
        } catch (e) {
          rethrow(e);
        }
      }
      const stmt = this.prepare(sql);
      try {
        return stmt.run(...params);
      } finally {
        stmt.finalize();
      }
    }

    exec(sql, ...params) {
      return this.run(sql, ...params);
    }

    transaction(fn) {
      if (typeof fn !== "function") throw new TypeError("Expected a function");
      const db = this;
      const wrap = (mode) =>
        function (...args) {
          // The outermost level is a real BEGIN (of the requested kind); nested calls become
          // savepoints, so an inner failure rolls back only its own work (Bun's semantics).
          const nested = db._txDepth > 0;
          const name = "__lumen_sp_" + db._txDepth;
          db.run(nested ? `SAVEPOINT ${name}` : mode ? `BEGIN ${mode}` : "BEGIN");
          db._txDepth++;
          try {
            const result = fn.apply(this, args);
            db._txDepth--;
            db.run(nested ? `RELEASE ${name}` : "COMMIT");
            return result;
          } catch (e) {
            db._txDepth--;
            db.run(nested ? `ROLLBACK TO ${name}; RELEASE ${name}` : "ROLLBACK");
            throw e;
          }
        };
      const tx = wrap("");
      tx.deferred = wrap("DEFERRED");
      tx.immediate = wrap("IMMEDIATE");
      tx.exclusive = wrap("EXCLUSIVE");
      return tx;
    }

    serialize() {
      if (!this._open) throw new Error("Database has closed");
      let bytes;
      try {
        bytes = S.serialize(this._id);
      } catch (e) {
        rethrow(e);
      }
      // Bun returns a Buffer; fall back to the raw Uint8Array if Buffer isn't installed.
      return typeof Buffer !== "undefined" ? Buffer.from(bytes) : bytes;
    }

    loadExtension(name) {
      if (!this._open) throw new Error("Database has closed");
      try { return S.loadExtension(this._id, String(name)); } catch (e) { rethrow(e); }
    }

    fileControl(command, value) {
      if (!this._open) throw new Error("Database has closed");
      try { return S.fileControl(this._id, Number(command), value); } catch (e) { rethrow(e); }
    }

    close() {
      // (accepts Bun's optional throwOnError argument; close errors surface either way)
      if (!this._open) return;
      this._open = false;
      for (const stmt of this._cache.values()) stmt._alive = false;
      this._cache.clear();
      try {
        S.close(this._id); // finalizes every statement this db still owns, then closes
      } catch (e) {
        rethrow(e);
      }
    }
  }
  Database.MAX_QUERY_CACHE_SIZE = 20;

  // SQLite's real flag values (open/prepare/deserialize/fcntl), copied from bun v1.2.21.
  const constants = {
    SQLITE_OPEN_READONLY: 1,
    SQLITE_OPEN_READWRITE: 2,
    SQLITE_OPEN_CREATE: 4,
    SQLITE_OPEN_DELETEONCLOSE: 8,
    SQLITE_OPEN_EXCLUSIVE: 16,
    SQLITE_OPEN_AUTOPROXY: 32,
    SQLITE_OPEN_URI: 64,
    SQLITE_OPEN_MEMORY: 128,
    SQLITE_OPEN_MAIN_DB: 256,
    SQLITE_OPEN_TEMP_DB: 512,
    SQLITE_OPEN_TRANSIENT_DB: 1024,
    SQLITE_OPEN_MAIN_JOURNAL: 2048,
    SQLITE_OPEN_TEMP_JOURNAL: 4096,
    SQLITE_OPEN_SUBJOURNAL: 8192,
    SQLITE_OPEN_SUPER_JOURNAL: 16384,
    SQLITE_OPEN_NOMUTEX: 32768,
    SQLITE_OPEN_FULLMUTEX: 65536,
    SQLITE_OPEN_SHAREDCACHE: 131072,
    SQLITE_OPEN_PRIVATECACHE: 262144,
    SQLITE_OPEN_WAL: 524288,
    SQLITE_OPEN_NOFOLLOW: 16777216,
    SQLITE_OPEN_EXRESCODE: 33554432,
    SQLITE_PREPARE_PERSISTENT: 1,
    SQLITE_PREPARE_NORMALIZE: 2,
    SQLITE_PREPARE_NO_VTAB: 4,
    SQLITE_DESERIALIZE_READONLY: 4,
    SQLITE_FCNTL_LOCKSTATE: 1,
    SQLITE_FCNTL_GET_LOCKPROXYFILE: 2,
    SQLITE_FCNTL_SET_LOCKPROXYFILE: 3,
    SQLITE_FCNTL_LAST_ERRNO: 4,
    SQLITE_FCNTL_SIZE_HINT: 5,
    SQLITE_FCNTL_CHUNK_SIZE: 6,
    SQLITE_FCNTL_FILE_POINTER: 7,
    SQLITE_FCNTL_SYNC_OMITTED: 8,
    SQLITE_FCNTL_WIN32_AV_RETRY: 9,
    SQLITE_FCNTL_PERSIST_WAL: 10,
    SQLITE_FCNTL_OVERWRITE: 11,
    SQLITE_FCNTL_VFSNAME: 12,
    SQLITE_FCNTL_POWERSAFE_OVERWRITE: 13,
    SQLITE_FCNTL_PRAGMA: 14,
    SQLITE_FCNTL_BUSYHANDLER: 15,
    SQLITE_FCNTL_TEMPFILENAME: 16,
    SQLITE_FCNTL_MMAP_SIZE: 18,
    SQLITE_FCNTL_TRACE: 19,
    SQLITE_FCNTL_HAS_MOVED: 20,
    SQLITE_FCNTL_SYNC: 21,
    SQLITE_FCNTL_COMMIT_PHASETWO: 22,
    SQLITE_FCNTL_WIN32_SET_HANDLE: 23,
    SQLITE_FCNTL_WAL_BLOCK: 24,
    SQLITE_FCNTL_ZIPVFS: 25,
    SQLITE_FCNTL_RBU: 26,
    SQLITE_FCNTL_VFS_POINTER: 27,
    SQLITE_FCNTL_JOURNAL_POINTER: 28,
    SQLITE_FCNTL_WIN32_GET_HANDLE: 29,
    SQLITE_FCNTL_PDB: 30,
    SQLITE_FCNTL_BEGIN_ATOMIC_WRITE: 31,
    SQLITE_FCNTL_COMMIT_ATOMIC_WRITE: 32,
    SQLITE_FCNTL_ROLLBACK_ATOMIC_WRITE: 33,
    SQLITE_FCNTL_LOCK_TIMEOUT: 34,
    SQLITE_FCNTL_DATA_VERSION: 35,
    SQLITE_FCNTL_SIZE_LIMIT: 36,
    SQLITE_FCNTL_CKPT_DONE: 37,
    SQLITE_FCNTL_RESERVE_BYTES: 38,
    SQLITE_FCNTL_CKPT_START: 39,
    SQLITE_FCNTL_EXTERNAL_READER: 40,
    SQLITE_FCNTL_CKSM_FILE: 41,
    SQLITE_FCNTL_RESET_CACHE: 42,
  };

  __builtins.set("bun:sqlite", {
    __esModule: true,
    default: Database,
    Database,
    Statement,
    SQLiteError,
    constants,
  });
}
