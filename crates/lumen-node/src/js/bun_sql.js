// Bun.SQL SQLite adapter over bun:sqlite. PostgreSQL/MySQL transports are added separately.
{
  const { Database, SQLiteError } = __builtins.get("bun:sqlite");

  class SQLQuery {
    constructor(client, strings, values, unsafe = false) {
      this.client = client; this.strings = strings; this._values = values; this.unsafeQuery = unsafe;
      this._promise = null; this._mode = "objects";
    }
    _compile() {
      if (this.unsafeQuery) return { text: String(this.strings), params: this._values || [] };
      let text = "", params = [];
      for (let index = 0; index < this.strings.length; index++) {
        text += this.strings[index];
        if (index >= this._values.length) continue;
        const value = this._values[index];
        if (value && value.__sqlFragment) {
          const fragment = compileFragment(value, /\bSET\s*$/i.test(text));
          text += fragment.text; params.push(...fragment.params);
        } else if (value instanceof SQLQuery) {
          const nested = value._compile(); text += nested.text; params.push(...nested.params);
        } else { text += "\x01"; params.push(value); }
      }
      return { text, params };
    }
    _run() {
      if (!this._promise) this._promise = Promise.resolve().then(() => this.client._execute(this._compile(), this._mode));
      return this._promise;
    }
    execute() { this._run(); return this; }
    cancel() { return false; }
    values() { this._mode = "values"; return this; }
    raw() { return this.values(); }
    simple() { return this; }
    then(resolve, reject) { return this._run().then(resolve, reject); }
    catch(reject) { return this._run().catch(reject); }
    finally(callback) { return this._run().finally(callback); }
    get [Symbol.toStringTag]() { return "SQLQuery"; }
  }

  function quoteIdentifier(value) {
    return String(value).split(".").map(part => `"${part.replace(/"/g, '""')}"`).join(".");
  }
  function fragment(value, columns) { return { __sqlFragment: true, value, columns }; }
  function compileFragment(item, update) {
    const value = item.value;
    if (typeof value === "string") return { text: quoteIdentifier(value), params: [] };
    if (Array.isArray(value)) {
      if (value.length && value[0] && typeof value[0] === "object" && !Array.isArray(value[0])) {
        const columns = item.columns.length ? item.columns : Object.keys(value[0]);
        const params = [], rows = value.map(row => `(${columns.map(column => { params.push(row[column]); return "\x01"; }).join(", ")})`);
        return { text: `(${columns.map(quoteIdentifier).join(", ")}) VALUES ${rows.join(", ")}`, params };
      }
      return { text: `(${value.map(() => "\x01").join(", ")})`, params: value.slice() };
    }
    if (value && typeof value === "object") {
      const columns = item.columns.length ? item.columns : Object.keys(value), params = columns.map(column => value[column]);
      if (update) return { text: columns.map(column => `${quoteIdentifier(column)} = \x01`).join(", "), params };
      return { text: `(${columns.map(quoteIdentifier).join(", ")}) VALUES (${columns.map(() => "\x01").join(", ")})`, params };
    }
    return { text: "\x01", params: [value] };
  }

  function sqliteFilename(url, options) {
    if (options && options.filename) return String(options.filename);
    url = String(url == null ? ":memory:" : url);
    if (url === ":memory:" || url === "sqlite::memory:" || url === "sqlite://:memory:") return ":memory:";
    if (url.startsWith("sqlite://")) return decodeURIComponent(url.slice(9));
    if (url.startsWith("sqlite:")) return decodeURIComponent(url.slice(7));
    return null;
  }

  function makeClient(url, options = {}) {
    if (url && typeof url === "object") { options = url; url = options.url || options.filename; }
    const postgres = url && globalThis.__lumenPostgres.pgConfig(url, options);
    if (postgres) return makePostgresClient(postgres, options);
    const filename = sqliteFilename(url, options);
    if (filename === null || (options.adapter && options.adapter !== "sqlite")) {
      const error = new Error("Bun.SQL MySQL transport is not supported in lumen yet");
      error.code = "ERR_SQL_UNSUPPORTED_ADAPTER";
      throw error;
    }
    const databaseOptions = options.readonly
      ? { readonly: true, safeIntegers: !!options.safeIntegers, strict: !!options.strict }
      : { readwrite: options.readwrite !== false, create: options.create !== false, safeIntegers: !!options.safeIntegers, strict: !!options.strict };
    const database = new Database(filename, databaseOptions);
    function sql(first, ...values) {
      if (Array.isArray(first) && Object.prototype.hasOwnProperty.call(first, "raw")) return new SQLQuery(sql, first, values);
      return fragment(first, values.map(String));
    }
    Object.setPrototypeOf(sql, SQL.prototype);
    sql.options = { ...options, adapter: "sqlite", filename };
    sql._database = database;
    sql._closed = false;
    sql._execute = (compiled, mode) => {
      if (sql._closed) throw new Error("SQL client is closed");
      const statement = database.prepare(compiled.text.replace(/\x01/g, "?"));
      try {
        if (mode === "values") return statement.values(...compiled.params);
        if (/^\s*(?:SELECT|PRAGMA|WITH|EXPLAIN)\b|\bRETURNING\b/i.test(compiled.text)) return statement.all(...compiled.params);
        const info = statement.run(...compiled.params), rows = [];
        rows.count = info.changes; rows.changes = info.changes; rows.lastInsertRowid = info.lastInsertRowid;
        return rows;
      } finally { statement.finalize(); }
    };
    sql.unsafe = (text, params = []) => new SQLQuery(sql, String(text), params, true);
    sql.array = values => fragment(Array.from(values), []);
    sql.begin = async callback => {
      database.run("BEGIN");
      try { const value = await callback(sql); database.run("COMMIT"); return value; }
      catch (error) { database.run("ROLLBACK"); throw error; }
    };
    sql.transaction = sql.begin;
    sql.reserve = async () => { sql.release = () => {}; return sql; };
    sql.close = async () => { if (!sql._closed) { sql._closed = true; database.close(); } };
    sql.flush = async () => {};
    return sql;
  }

  function makePostgresClient(config, options) {
    const connection = new globalThis.__lumenPostgres.PgConnection(config);
    function sql(first, ...values) {
      if (Array.isArray(first) && Object.prototype.hasOwnProperty.call(first, "raw")) return new SQLQuery(sql, first, values);
      return fragment(first, values.map(String));
    }
    Object.setPrototypeOf(sql, SQL.prototype);
    sql.options = { ...options, adapter: "postgres" };
    sql._closed = false;
    sql._execute = (compiled, mode) => {
      if (sql._closed) throw new Error("SQL client is closed");
      let index = 0;
      const text = compiled.text.replace(/\x01/g, () => `$${++index}`);
      return connection.query(text, compiled.params, mode);
    };
    sql.unsafe = (text, params = []) => new SQLQuery(sql, String(text), params, true);
    sql.array = values => fragment(Array.from(values), []);
    sql.begin = async callback => {
      await sql.unsafe("BEGIN");
      try { const value = await callback(sql); await sql.unsafe("COMMIT"); return value; }
      catch (error) { await sql.unsafe("ROLLBACK"); throw error; }
    };
    sql.transaction = sql.begin;
    sql.reserve = async () => { sql.release = () => {}; return sql; };
    sql.close = async () => { sql._closed = true; await connection.close(); };
    sql.flush = async () => {};
    return sql;
  }

  function SQL(url, options) { return makeClient(url, options); }
  SQL.SQLiteError = SQLiteError;
  Object.defineProperty(globalThis, "__lumenSQL", { value: { SQL, makeClient }, configurable: true });
}
