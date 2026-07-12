// node:tty stream classes over lumen's process streams. The runtime is pipe-backed, so terminal
// capability methods are functional but consistently report a non-TTY environment.
{
  const { Readable, Writable } = __builtins.get("stream");

  class ReadStream extends Readable {
    constructor(fd = 0) {
      super({});
      this.fd = Number(fd);
      this.isTTY = false;
      this.isRaw = false;
      const input = process.stdin;
      if (input && typeof input.on === "function") {
        input.on("data", chunk => this.push(chunk));
        input.on("end", () => this.push(null));
        input.on("error", error => this.destroy(error));
      }
    }
    setRawMode(mode) { this.isRaw = !!mode; return this; }
  }

  class WriteStream extends Writable {
    constructor(fd = 1) {
      super({});
      this.fd = Number(fd);
      this.isTTY = false;
      this.columns = 80;
      this.rows = 24;
      this._target = this.fd === 2 ? process.stderr : process.stdout;
    }
    _write(chunk, _encoding, callback) {
      try { this._target.write(chunk); callback(); } catch (error) { callback(error); }
    }
    clearLine(_direction, callback) { if (callback) queueMicrotask(callback); return true; }
    clearScreenDown(callback) { if (callback) queueMicrotask(callback); return true; }
    cursorTo(_x, _y, callback) { if (typeof _y === "function") callback = _y; if (callback) queueMicrotask(callback); return true; }
    moveCursor(_dx, _dy, callback) { if (callback) queueMicrotask(callback); return true; }
    getColorDepth() { return 1; }
    hasColors() { return false; }
    getWindowSize() { return [this.columns, this.rows]; }
  }

  __builtins.set("tty", { isatty: () => false, ReadStream, WriteStream });
}
