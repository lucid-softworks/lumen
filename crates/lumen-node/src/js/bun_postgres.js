// PostgreSQL v3 wire transport for Bun.SQL. Queries use the extended protocol so interpolated
// values remain protocol parameters rather than escaped SQL text.
{
  const net = __builtins.get("net"), tls = __builtins.get("tls"), crypto = __builtins.get("crypto");
  const cstring = value => Buffer.concat([Buffer.from(String(value)), Buffer.from([0])]);
  const i16 = value => Buffer.from([(value >>> 8) & 255, value & 255]);
  const i32 = value => Buffer.from([(value >>> 24) & 255, (value >>> 16) & 255, (value >>> 8) & 255, value & 255]);
  const u16 = (bytes, offset = 0) => bytes[offset] * 256 + bytes[offset + 1];
  const u32 = (bytes, offset = 0) => (bytes[offset] * 0x1000000) + (bytes[offset + 1] << 16) + (bytes[offset + 2] << 8) + bytes[offset + 3];
  const s32 = (bytes, offset = 0) => u32(bytes, offset) | 0;
  function packet(type, payload) { payload = Buffer.from(payload || []); return Buffer.concat([Buffer.from(type), i32(payload.length + 4), payload]); }

  function postgresError(fields) {
    const error = new Error(fields.M || "PostgreSQL server error");
    error.name = fields.C === "42601" ? "SyntaxError" : "PostgresError";
    error.code = fields.C || "ERR_POSTGRES_SERVER_ERROR";
    error.severity = fields.S; error.detail = fields.D; error.hint = fields.H;
    return error;
  }
  function parseFields(payload) {
    const fields = {}; let offset = 0;
    while (offset < payload.length && payload[offset]) {
      const key = String.fromCharCode(payload[offset++]), end = payload.indexOf(0, offset);
      if (end < 0) break; fields[key] = payload.toString("utf8", offset, end); offset = end + 1;
    }
    return fields;
  }
  function parameter(value) {
    if (value === null || value === undefined) return null;
    if (value instanceof Date) return value.toISOString();
    if (value instanceof Uint8Array) return `\\x${Buffer.from(value).toString("hex")}`;
    if (typeof value === "boolean") return value ? "true" : "false";
    if (typeof value === "object") return JSON.stringify(value);
    return String(value);
  }
  function decode(value, oid) {
    if (value === null) return null;
    if (oid === 16) return value === "t";
    if ([20, 21, 23, 26].includes(oid)) { const number = Number(value); return Number.isSafeInteger(number) ? number : BigInt(value); }
    if ([700, 701, 1700].includes(oid)) return Number(value);
    if (oid === 114 || oid === 3802) return JSON.parse(value);
    if (oid === 17 && value.startsWith("\\x")) return Buffer.from(value.slice(2), "hex");
    if ([1082, 1114, 1184].includes(oid)) return new Date(value);
    return value;
  }
  function scramFields(message) {
    const fields = {};
    for (const part of String(message).split(",")) fields[part.slice(0, 1)] = part.slice(2);
    return fields;
  }
  function scramHmac(key, value) { return Buffer.from(crypto.createHmac("sha256", key).update(value).digest()); }
  function scramXor(left, right) { return Buffer.from(left.map((value, index) => value ^ right[index])); }

  class PgConnection {
    constructor(config) {
      this.config = config; this.socket = null; this.buffer = Buffer.alloc(0); this.messages = [];
      this.waiter = null; this.connected = false; this.connecting = null; this.tail = null;
    }
    connect() {
      if (this.connected) return Promise.resolve(this);
      if (this.connecting) return this.connecting;
      this.connecting = new Promise((resolve, reject) => {
        const raw = new net.Socket({ _deferRead: this.config.tls });
        raw.once("error", reject);
        raw.connect(this.config.port, this.config.host, () => {
          (async () => {
            let socket = raw;
            if (this.config.tls) {
              await new Promise((done, fail) => raw.write(Buffer.concat([i32(8), i32(80877103)]), error => error ? fail(error) : done()));
              const response = await raw._readRaw();
              if (!response || response[0] !== 83) { const error = new Error("PostgreSQL server refused TLS negotiation"); error.code = "ERR_POSTGRES_TLS_NOT_AVAILABLE"; throw error; }
              socket = await new Promise((done, fail) => {
                const secure = tls.connect({ socket: raw, servername: this.config.servername, rejectUnauthorized: this.config.rejectUnauthorized }, () => done(secure));
                secure.once("error", fail);
              });
            }
            this.socket = socket;
            socket.on("data", chunk => this._data(chunk)); socket.once("error", reject);
            const params = Buffer.concat([cstring("user"), cstring(this.config.user), cstring("database"), cstring(this.config.database), cstring("client_encoding"), cstring("UTF8"), Buffer.from([0])]);
            await this._write(Buffer.concat([i32(params.length + 8), i32(196608), params]));
            await this._handshake(); this.connected = true; resolve(this);
          })().catch(reject);
        });
      });
      return this.connecting;
    }
    _data(chunk) {
      this.buffer = Buffer.concat([this.buffer, Buffer.from(chunk)]);
      while (this.buffer.length >= 5) {
        const length = u32(this.buffer, 1);
        if (length < 4 || this.buffer.length < length + 1) break;
        const message = { type: this.buffer.toString("ascii", 0, 1), payload: Buffer.from(this.buffer.subarray(5, length + 1)) };
        this.buffer = this.buffer.subarray(length + 1);
        if (this.waiter) { const waiter = this.waiter; this.waiter = null; waiter(message); } else this.messages.push(message);
      }
    }
    _next() { if (this.messages.length) return Promise.resolve(this.messages.shift()); return new Promise(resolve => { this.waiter = resolve; }); }
    _write(bytes) {
      return new Promise((resolve, reject) => this.socket.write(bytes, error => error ? reject(error) : resolve()));
    }
    async _handshake() {
      let scram = null;
      for (;;) {
        const message = await this._next();
        if (message.type === "R") {
          const method = u32(message.payload, 0);
          if (method === 0) continue;
          if (method === 3) await this._write(packet("p", cstring(this.config.password)));
          else if (method === 5) {
            const first = crypto.createHash("md5").update(this.config.password + this.config.user).digest("hex");
            const response = "md5" + crypto.createHash("md5").update(Buffer.concat([Buffer.from(first), message.payload.subarray(4, 8)])).digest("hex");
            await this._write(packet("p", cstring(response)));
          } else if (method === 10) {
            const mechanisms = message.payload.toString("utf8", 4).split("\0");
            if (!mechanisms.includes("SCRAM-SHA-256")) { const error = new Error("PostgreSQL server does not offer SCRAM-SHA-256"); error.code = "ERR_POSTGRES_UNSUPPORTED_AUTHENTICATION_METHOD"; throw error; }
            const nonce = crypto.randomBytes(18).toString("base64url");
            const user = this.config.user.replace(/=/g, "=3D").replace(/,/g, "=2C");
            const bare = `n=${user},r=${nonce}`, first = `n,,${bare}`;
            scram = { nonce, bare, expectedServerSignature: null };
            await this._write(packet("p", Buffer.concat([cstring("SCRAM-SHA-256"), i32(Buffer.byteLength(first)), Buffer.from(first)])));
          } else if (method === 11) {
            if (!scram) throw new Error("Unexpected PostgreSQL SCRAM continuation");
            const serverFirst = message.payload.toString("utf8", 4), fields = scramFields(serverFirst);
            const iterations = Number(fields.i);
            if (!fields.r || !fields.r.startsWith(scram.nonce) || fields.r.length <= scram.nonce.length) { const error = new Error("PostgreSQL SCRAM server nonce is invalid"); error.code = "ERR_POSTGRES_INVALID_SERVER_NONCE"; throw error; }
            if (!Number.isInteger(iterations) || iterations < 1 || iterations > 10000000) { const error = new Error("PostgreSQL SCRAM iteration count is invalid"); error.code = "ERR_POSTGRES_AUTHENTICATION_FAILED_PBKDF2"; throw error; }
            let salt;
            try { salt = Buffer.from(fields.s, "base64"); } catch (_) { salt = null; }
            if (!salt || !salt.length) { const error = new Error("PostgreSQL SCRAM salt is invalid"); error.code = "ERR_POSTGRES_AUTHENTICATION_FAILED_PBKDF2"; throw error; }
            const finalWithoutProof = `c=biws,r=${fields.r}`;
            const authMessage = `${scram.bare},${serverFirst},${finalWithoutProof}`;
            const salted = crypto.pbkdf2Sync(this.config.password, salt, iterations, 32, "sha256");
            const clientKey = scramHmac(salted, "Client Key");
            const storedKey = crypto.createHash("sha256").update(clientKey).digest();
            const proof = scramXor(clientKey, scramHmac(storedKey, authMessage)).toString("base64");
            const serverKey = scramHmac(salted, "Server Key");
            scram.expectedServerSignature = scramHmac(serverKey, authMessage);
            await this._write(packet("p", Buffer.from(`${finalWithoutProof},p=${proof}`)));
          } else if (method === 12) {
            if (!scram || !scram.expectedServerSignature) throw new Error("Unexpected PostgreSQL SCRAM final response");
            const fields = scramFields(message.payload.toString("utf8", 4));
            if (fields.e) { const error = new Error(`PostgreSQL SCRAM authentication failed: ${fields.e}`); error.code = "ERR_POSTGRES_AUTHENTICATION_FAILED"; throw error; }
            const signature = Buffer.from(fields.v || "", "base64");
            if (signature.length !== scram.expectedServerSignature.length || !crypto.timingSafeEqual(signature, scram.expectedServerSignature)) {
              const error = new Error("PostgreSQL SCRAM server signature mismatch"); error.code = "ERR_POSTGRES_SASL_SIGNATURE_MISMATCH"; throw error;
            }
          } else { const error = new Error(`Unsupported PostgreSQL authentication method ${method}`); error.code = "ERR_POSTGRES_UNSUPPORTED_AUTHENTICATION_METHOD"; throw error; }
        } else if (message.type === "E") throw postgresError(parseFields(message.payload));
        else if (message.type === "Z") return;
      }
    }
    query(text, params, mode) {
      const run = async () => { await this.connect(); return this._query(text, params, mode); };
      const result = this.tail ? this.tail.then(run, run) : run();
      this.tail = result.then(() => {}, () => {}); return result;
    }
    async _query(text, params, mode) {
      const parse = Buffer.concat([Buffer.from([0]), cstring(text), i16(0)]);
      const values = params.map(parameter), bindParts = [Buffer.from([0, 0]), i16(0), i16(values.length)];
      for (const value of values) bindParts.push(value === null ? Buffer.from([0xff, 0xff, 0xff, 0xff]) : Buffer.concat([i32(Buffer.byteLength(value)), Buffer.from(value)]));
      bindParts.push(i16(0));
      const describe = Buffer.from([80, 0]), execute = Buffer.concat([Buffer.from([0]), i32(0)]);
      await this._write(Buffer.concat([packet("P", parse), packet("B", Buffer.concat(bindParts)), packet("D", describe), packet("E", execute), packet("S")]));
      let columns = [], rows = [], command = "", error;
      for (;;) {
        const message = await this._next();
        if (message.type === "T") {
          let offset = 2; columns = [];
          const count = u16(message.payload, 0);
          for (let index = 0; index < count; index++) {
            const end = message.payload.indexOf(0, offset), name = message.payload.toString("utf8", offset, end); offset = end + 1;
            const oid = u32(message.payload, offset + 6); offset += 18; columns.push({ name, oid });
          }
        } else if (message.type === "D") {
          let offset = 2, values = [];
          const count = u16(message.payload, 0);
          for (let index = 0; index < count; index++) {
            const length = s32(message.payload, offset); offset += 4;
            if (length < 0) values.push(null); else { values.push(message.payload.toString("utf8", offset, offset + length)); offset += length; }
          }
          if (mode === "values") rows.push(values.map((value, index) => decode(value, columns[index] && columns[index].oid)));
          else { const row = {}; for (let index = 0; index < values.length; index++) row[columns[index].name] = decode(values[index], columns[index].oid); rows.push(row); }
        } else if (message.type === "C") command = message.payload.toString("utf8", 0, message.payload.length - 1);
        else if (message.type === "E") error = postgresError(parseFields(message.payload));
        else if (message.type === "Z") break;
      }
      if (error) throw error;
      rows.command = command.split(" ", 1)[0];
      const count = Number(command.split(" ").pop()); rows.count = Number.isFinite(count) ? count : rows.length;
      return rows;
    }
    async close() { if (this.socket) { await this._write(packet("X")); this.socket.end(); } this.connected = false; }
  }

  function pgConfig(url, options = {}) {
    if (!/^postgres(?:ql)?:/i.test(String(url))) return null;
    const target = new URL(String(url));
    const sslmode = String(target.searchParams.get("sslmode") || "disable").toLowerCase();
    const tlsOptions = options.tls && typeof options.tls === "object" ? options.tls : {};
    const useTls = !!options.tls || (options.tls !== false && !["disable", "false"].includes(sslmode));
    return { host: target.hostname || "localhost", port: Number(target.port || 5432), user: decodeURIComponent(target.username || process.env.USER || "postgres"), password: decodeURIComponent(target.password || ""), database: decodeURIComponent(target.pathname.slice(1) || target.username || "postgres"), tls: useTls, servername: tlsOptions.servername || target.hostname || "localhost", rejectUnauthorized: tlsOptions.rejectUnauthorized !== false && !["require", "allow", "prefer"].includes(sslmode) };
  }
  Object.defineProperty(globalThis, "__lumenPostgres", { value: { PgConnection, pgConfig }, configurable: true });
}
