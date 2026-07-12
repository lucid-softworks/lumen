// MySQL protocol 4.1 transport for Bun.SQL. Operations are serialized on one connection and
// template values are converted to escaped SQL literals before COM_QUERY is sent.
{
  const net = __builtins.get("net"), crypto = __builtins.get("crypto");
  const u16 = (b, o = 0) => b[o] + b[o + 1] * 256;
  const u24 = (b, o = 0) => b[o] + b[o + 1] * 256 + b[o + 2] * 65536;
  const u32 = (b, o = 0) => (b[o] + b[o + 1] * 256 + b[o + 2] * 65536 + b[o + 3] * 0x1000000) >>> 0;
  const le16 = n => Buffer.from([n & 255, (n >>> 8) & 255]);
  const le24 = n => Buffer.from([n & 255, (n >>> 8) & 255, (n >>> 16) & 255]);
  const le32 = n => Buffer.from([n & 255, (n >>> 8) & 255, (n >>> 16) & 255, (n >>> 24) & 255]);
  const cstring = value => Buffer.concat([Buffer.from(String(value)), Buffer.from([0])]);
  const xor = (a, b) => Buffer.from(a.map((value, index) => value ^ b[index % b.length]));
  function hash(name, value) { return Buffer.from(crypto.createHash(name).update(value).digest()); }
  function packet(sequence, payload) { payload = Buffer.from(payload); return Buffer.concat([le24(payload.length), Buffer.from([sequence & 255]), payload]); }
  function nativePassword(password, scramble) {
    if (!password) return Buffer.alloc(0);
    const first = hash("sha1", password), second = hash("sha1", first);
    return xor(first, hash("sha1", Buffer.concat([scramble, second])));
  }
  function cachingPassword(password, scramble) {
    if (!password) return Buffer.alloc(0);
    const first = hash("sha256", password), second = hash("sha256", first);
    return xor(first, hash("sha256", Buffer.concat([second, scramble])));
  }
  function auth(plugin, password, scramble) {
    if (plugin === "mysql_native_password") return nativePassword(password, scramble);
    if (plugin === "caching_sha2_password") return cachingPassword(password, scramble);
    const error = new Error(`Unsupported MySQL authentication plugin '${plugin}'`);
    error.code = "ERR_MYSQL_UNSUPPORTED_AUTHENTICATION_PLUGIN"; throw error;
  }
  function lenenc(bytes, offset = 0) {
    const first = bytes[offset++];
    if (first < 0xfb) return [first, offset];
    if (first === 0xfb) return [null, offset];
    if (first === 0xfc) return [u16(bytes, offset), offset + 2];
    if (first === 0xfd) return [u24(bytes, offset), offset + 3];
    if (first === 0xfe) {
      const low = u32(bytes, offset), high = u32(bytes, offset + 4);
      return [high ? BigInt(high) * 0x100000000n + BigInt(low) : low, offset + 8];
    }
    throw new Error("Invalid MySQL length-encoded integer");
  }
  function lenstr(bytes, offset) {
    const [length, next] = lenenc(bytes, offset);
    if (length === null) return [null, next];
    const size = Number(length); return [bytes.toString("utf8", next, next + size), next + size];
  }
  function mysqlError(payload) {
    const code = u16(payload, 1), hasState = payload[3] === 35;
    const message = payload.toString("utf8", hasState ? 9 : 3);
    const error = new Error(message); error.name = "MySQLError"; error.errno = code;
    error.sqlState = hasState ? payload.toString("ascii", 4, 9) : undefined;
    error.code = `ER_${code}`; return error;
  }
  function quote(value) {
    if (value === null || value === undefined) return "NULL";
    if (typeof value === "number") { if (!Number.isFinite(value)) throw new TypeError("MySQL parameters must be finite"); return String(value); }
    if (typeof value === "bigint") return String(value);
    if (typeof value === "boolean") return value ? "TRUE" : "FALSE";
    if (value instanceof Date) value = value.toISOString().slice(0, 23).replace("T", " ");
    if (value instanceof Uint8Array) return `X'${Buffer.from(value).toString("hex")}'`;
    if (typeof value === "object") value = JSON.stringify(value);
    return `'${String(value).replace(/\\/g, "\\\\").replace(/\0/g, "\\0").replace(/\n/g, "\\n").replace(/\r/g, "\\r").replace(/\x1a/g, "\\Z").replace(/'/g, "\\'")}'`;
  }
  function bind(text, params) {
    let output = "", index = 0, quoteMark = null, escaped = false;
    for (const character of text) {
      if (escaped) { output += character; escaped = false; continue; }
      if (quoteMark && character === "\\") { output += character; escaped = true; continue; }
      if (character === "'" || character === '"' || character === "`") {
        if (quoteMark === character) quoteMark = null; else if (!quoteMark) quoteMark = character;
        output += character; continue;
      }
      if (!quoteMark && (character === "\x01" || (character === "?" && !text.includes("\x01")))) {
        if (index >= params.length) throw new Error("Not enough MySQL query parameters");
        output += quote(params[index++]);
      } else output += character;
    }
    if (index !== params.length) throw new Error("Too many MySQL query parameters");
    return output;
  }
  function decode(value, column) {
    if (value === null) return null;
    if ([1, 2, 3, 8, 9, 13].includes(column.type)) {
      const number = Number(value); return Number.isSafeInteger(number) ? number : BigInt(value);
    }
    if ([4, 5, 246].includes(column.type)) return Number(value);
    if (column.type === 245) { try { return JSON.parse(value); } catch (_) {} }
    return value;
  }

  class MySqlConnection {
    constructor(config) {
      this.config = config; this.socket = null; this.buffer = Buffer.alloc(0); this.packets = [];
      this.waiter = null; this.connected = false; this.connecting = null; this.tail = null;
    }
    connect() {
      if (this.connected) return Promise.resolve(this);
      if (this.connecting) return this.connecting;
      this.connecting = new Promise((resolve, reject) => {
        const socket = this.socket = new net.Socket();
        socket.on("data", chunk => this._data(chunk)); socket.once("error", reject);
        socket.connect(this.config.port, this.config.host, () => this._handshake().then(() => { this.connected = true; resolve(this); }, reject));
      });
      return this.connecting;
    }
    _data(chunk) {
      this.buffer = Buffer.concat([this.buffer, Buffer.from(chunk)]);
      while (this.buffer.length >= 4) {
        const length = u24(this.buffer);
        if (this.buffer.length < length + 4) break;
        const item = { sequence: this.buffer[3], payload: Buffer.from(this.buffer.subarray(4, length + 4)) };
        this.buffer = this.buffer.subarray(length + 4);
        if (this.waiter) { const waiter = this.waiter; this.waiter = null; waiter(item); } else this.packets.push(item);
      }
    }
    _next() { if (this.packets.length) return Promise.resolve(this.packets.shift()); return new Promise(resolve => { this.waiter = resolve; }); }
    _write(sequence, payload) { return new Promise((resolve, reject) => this.socket.write(packet(sequence, payload), error => error ? reject(error) : resolve())); }
    async _handshake() {
      const greeting = await this._next(), bytes = greeting.payload;
      if (bytes[0] === 0xff) throw mysqlError(bytes);
      let offset = bytes.indexOf(0, 1) + 1 + 4;
      const first = bytes.subarray(offset, offset + 8); offset += 9;
      const low = u16(bytes, offset); offset += 2;
      offset += 1 + 2;
      const capabilities = low | (u16(bytes, offset) << 16); offset += 2;
      const authLength = bytes[offset++]; offset += 10;
      const secondLength = Math.max(12, authLength - 8), second = bytes.subarray(offset, Math.min(bytes.length, offset + secondLength));
      offset += secondLength;
      const scramble = Buffer.concat([Buffer.from(first), Buffer.from(second)]).subarray(0, Math.max(0, authLength - 1));
      let plugin = offset < bytes.length ? bytes.toString("utf8", offset, Math.max(offset, bytes.indexOf(0, offset))) : "mysql_native_password";
      if (!plugin) plugin = "mysql_native_password";
      const flags = (1 | 4 | 8 | 0x200 | 0x2000 | 0x8000 | 0x20000 | 0x80000) & capabilities;
      const response = auth(plugin, this.config.password, scramble);
      const payload = Buffer.concat([le32(flags), le32(0x1000000), Buffer.from([45]), Buffer.alloc(23), cstring(this.config.user), Buffer.from([response.length]), response, cstring(this.config.database), cstring(plugin)]);
      await this._write(1, payload);
      let result = await this._next();
      if (result.payload[0] === 0xfe && result.payload.length > 1) {
        const end = result.payload.indexOf(0, 1), switchedPlugin = result.payload.toString("utf8", 1, end);
        await this._write(result.sequence + 1, auth(switchedPlugin, this.config.password, result.payload.subarray(end + 1)));
        result = await this._next();
      }
      if (result.payload[0] === 0x01 && result.payload[1] === 0x03) result = await this._next();
      if (result.payload[0] === 0x01 && result.payload[1] === 0x04) {
        const error = new Error("MySQL caching_sha2_password full authentication requires TLS"); error.code = "ERR_MYSQL_TLS_REQUIRED"; throw error;
      }
      if (result.payload[0] === 0xff) throw mysqlError(result.payload);
      if (result.payload[0] !== 0x00) throw new Error("Unexpected MySQL authentication response");
    }
    query(text, params, mode) {
      const run = async () => { await this.connect(); return this._query(bind(text, params), mode); };
      const result = this.tail ? this.tail.then(run, run) : run(); this.tail = result.then(() => {}, () => {}); return result;
    }
    async _query(text, mode) {
      await this._write(0, Buffer.concat([Buffer.from([3]), Buffer.from(text)]));
      let current = await this._next(), payload = current.payload;
      if (payload[0] === 0xff) throw mysqlError(payload);
      if (payload[0] === 0x00) {
        let [affected, offset] = lenenc(payload, 1), [insertId] = lenenc(payload, offset);
        const rows = []; rows.count = Number(affected); rows.changes = Number(affected); rows.lastInsertRowid = insertId; return rows;
      }
      const [count] = lenenc(payload), columns = [];
      for (let index = 0; index < Number(count); index++) {
        payload = (await this._next()).payload; let offset = 0, value;
        let name;
        for (let part = 0; part < 6; part++) { [value, offset] = lenstr(payload, offset); if (part === 4) name = value; }
        [value, offset] = lenenc(payload, offset);
        offset += 2 + 4; const type = payload[offset++], flags = u16(payload, offset);
        columns.push({ name, type, flags });
      }
      payload = (await this._next()).payload;
      const rows = [];
      for (;;) {
        payload = (await this._next()).payload;
        if (payload[0] === 0xfe && payload.length < 9) break;
        if (payload[0] === 0xff) throw mysqlError(payload);
        let offset = 0, values = [];
        for (let index = 0; index < columns.length; index++) { let value; [value, offset] = lenstr(payload, offset); values.push(decode(value, columns[index])); }
        if (mode === "values") rows.push(values); else { const row = {}; columns.forEach((column, index) => { row[column.name] = values[index]; }); rows.push(row); }
      }
      rows.count = rows.length; rows.command = text.trim().split(/\s+/, 1)[0].toUpperCase(); return rows;
    }
    async close() { if (this.socket) { await this._write(0, Buffer.from([1])); this.socket.end(); } this.connected = false; }
  }
  function mysqlConfig(url, options = {}) {
    if (!/^mysql:/i.test(String(url))) return null;
    const target = new URL(String(url));
    if (options.tls || target.searchParams.get("ssl") === "true") { const error = new Error("MySQL TLS negotiation is not supported in lumen yet"); error.code = "ERR_MYSQL_TLS_UNSUPPORTED"; throw error; }
    return { host: target.hostname || "localhost", port: Number(target.port || 3306), user: decodeURIComponent(target.username || "root"), password: decodeURIComponent(target.password || ""), database: decodeURIComponent(target.pathname.slice(1) || "") };
  }
  Object.defineProperty(globalThis, "__lumenMySQL", { value: { MySqlConnection, mysqlConfig }, configurable: true });
}
