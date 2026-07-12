// RESP2 transport for Bun.RedisClient. Protocol handling lives outside bun.js so the connection
// state machine and incremental parser can be tested and maintained independently.
{
  const net = __builtins.get("net");
  const tls = __builtins.get("tls");
  const INCOMPLETE = Symbol("incomplete RESP value");

  function findLineEnd(buffer, start) {
    for (let i = start; i + 1 < buffer.length; i++) {
      if (buffer[i] === 13 && buffer[i + 1] === 10) return i;
    }
    return -1;
  }

  function parseResp(buffer, offset = 0, binary = false) {
    if (offset >= buffer.length) return INCOMPLETE;
    const type = buffer[offset];
    const lineEnd = findLineEnd(buffer, offset + 1);
    if (lineEnd < 0) return INCOMPLETE;
    const line = buffer.toString("utf8", offset + 1, lineEnd);
    if (type === 43) return [line, lineEnd + 2]; // simple string
    if (type === 45) {
      const error = new Error(line);
      error.code = line.split(/[ :]/, 1)[0] || "ERR_REDIS_RESPONSE";
      return [error, lineEnd + 2, true];
    }
    if (type === 58) return [Number(line), lineEnd + 2];
    if (type === 36) {
      const length = Number(line);
      if (length === -1) return [null, lineEnd + 2];
      if (!Number.isInteger(length) || length < 0) return invalidResp(buffer, "bulk string length");
      const start = lineEnd + 2;
      const end = start + length;
      if (buffer.length < end + 2) return INCOMPLETE;
      if (buffer[end] !== 13 || buffer[end + 1] !== 10) return invalidResp(buffer, "bulk string terminator");
      const bytes = buffer.subarray(start, end);
      return [binary ? Buffer.from(bytes) : bytes.toString("utf8"), end + 2];
    }
    if (type === 42) {
      const length = Number(line);
      if (length === -1) return [null, lineEnd + 2];
      if (!Number.isInteger(length) || length < 0) return invalidResp(buffer, "array length");
      const values = [];
      let cursor = lineEnd + 2;
      for (let i = 0; i < length; i++) {
        const item = parseResp(buffer, cursor, binary);
        if (item === INCOMPLETE) return INCOMPLETE;
        if (item[2]) return item;
        values.push(item[0]);
        cursor = item[1];
      }
      return [values, cursor];
    }
    return invalidResp(buffer, `type byte ${type}`);
  }

  function invalidResp(buffer, detail) {
    const error = new Error(`Invalid Redis response (${detail})`);
    error.code = "ERR_REDIS_INVALID_RESPONSE";
    return [error, buffer.length, true];
  }

  function encodeCommand(command, args) {
    const parts = [String(command), ...args.flat(Infinity).map(String)];
    let output = `*${parts.length}\r\n`;
    for (const part of parts) output += `$${Buffer.byteLength(part)}\r\n${part}\r\n`;
    return output;
  }

  class RedisClient {
    constructor(url, options = {}) {
      this.url = String(url || process.env.REDIS_URL || process.env.VALKEY_URL || "redis://localhost:6379");
      this.options = options || {};
      this.connected = false;
      this.onconnect = null;
      this.onclose = null;
      this._socket = null;
      this._connecting = null;
      this._buffer = Buffer.alloc(0);
      this._pending = [];
    }

    get bufferedAmount() {
      return this._socket ? this._socket.writableLength || 0 : 0;
    }

    connect() {
      if (this.connected) return Promise.resolve(this);
      if (this._connecting) return this._connecting;
      let target;
      try { target = new URL(this.url); } catch (error) { return Promise.reject(error); }
      if (target.protocol !== "redis:" && target.protocol !== "rediss:") return Promise.reject(new TypeError(`Unsupported Redis protocol ${target.protocol}`));
      this._connecting = new Promise((resolve, reject) => {
        const secure = target.protocol === "rediss:";
        const tlsOptions = this.options.tls && typeof this.options.tls === "object" ? this.options.tls : this.options;
        const connectOptions = {
          host: target.hostname,
          port: Number(target.port || (secure ? 6380 : 6379)),
          servername: tlsOptions.servername || target.hostname,
          rejectUnauthorized: tlsOptions.rejectUnauthorized !== false,
        };
        const socket = this._socket = secure ? tls.connect(connectOptions) : net.connect(connectOptions);
        socket.on("data", chunk => {
          this._buffer = Buffer.concat([this._buffer, Buffer.from(chunk)]);
          this._drainResponses();
        });
        socket.once(secure ? "secureConnect" : "connect", async () => {
          this.connected = true;
          try {
            if (target.password) {
              const auth = target.username
                ? [decodeURIComponent(target.username), decodeURIComponent(target.password)]
                : [decodeURIComponent(target.password)];
              await this._write("AUTH", auth);
            }
            if (target.pathname && target.pathname !== "/") await this._write("SELECT", [target.pathname.slice(1)]);
            if (typeof this.onconnect === "function") this.onconnect();
            resolve(this);
          } catch (error) { reject(error); }
        });
        socket.once("error", error => { if (!this.connected) reject(error); });
        socket.on("close", () => this._closed());
      });
      return this._connecting;
    }

    _closed() {
      const notify = this.connected;
      this.connected = false;
      this._connecting = null;
      this._socket = null;
      const error = new Error("Redis connection closed");
      error.code = "ERR_REDIS_CONNECTION_CLOSED";
      for (const pending of this._pending.splice(0)) pending.reject(error);
      if (notify && typeof this.onclose === "function") this.onclose(error);
    }

    _drainResponses() {
      while (this._pending.length) {
        const result = parseResp(this._buffer, 0, this._pending[0].binary);
        if (result === INCOMPLETE) return;
        this._buffer = this._buffer.subarray(result[1]);
        const pending = this._pending.shift();
        if (result[2]) pending.reject(result[0]); else pending.resolve(result[0]);
      }
    }

    _write(command, args, binary = false) {
      return new Promise((resolve, reject) => {
        this._pending.push({ resolve, reject, binary });
        this._socket.write(encodeCommand(command, args));
      });
    }

    async send(command, args = []) {
      if (!Array.isArray(args)) throw new TypeError("RedisClient.send arguments must be an array");
      await this.connect();
      return this._write(command, args);
    }

    async getBuffer(key) {
      await this.connect();
      return this._write("GET", [key], true);
    }

    close() {
      if (this._socket) this._socket.destroy();
    }
  }

  const commands = (
    "append bitcount decr del dump expire expiretime get getdel getex getset hget hincrby " +
    "hincrbyfloat hkeys hlen hmget hmset hstrlen hvals incr keys llen lpop lpush lpushx mget " +
    "persist pexpiretime pfadd ping pttl publish rpop rpush rpushx sadd scard script select set " +
    "setnx smembers smove spop srandmember srem strlen substr ttl zcard zpopmax zpopmin " +
    "zrandmember zrank zrevrank zscore"
  ).split(" ");
  for (const name of commands) {
    RedisClient.prototype[name] = function (...args) { return this.send(name.toUpperCase(), args); };
  }
  for (const name of ["exists", "sismember"]) {
    RedisClient.prototype[name] = async function (...args) {
      return !!(await this.send(name.toUpperCase(), args));
    };
  }
  RedisClient.prototype.hgetall = async function (...args) {
    const values = await this.send("HGETALL", args);
    if (values === null) return null;
    const result = {};
    for (let i = 0; i < values.length; i += 2) result[values[i]] = values[i + 1];
    return result;
  };

  Object.defineProperty(RedisClient.prototype, Symbol.toStringTag, { value: "RedisClient" });
  Object.defineProperty(globalThis, "__lumenRedisClient", { value: RedisClient, configurable: true });
}
