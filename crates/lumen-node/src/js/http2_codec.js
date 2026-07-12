// HTTP/2 binary framing and HPACK without transport concerns. The session layer in http2.js uses
// this codec over net/TLS streams.
{
  const huffman = globalThis.__lumenHpackHuffman;
  const STATIC = [null,
    [":authority", ""], [":method", "GET"], [":method", "POST"], [":path", "/"], [":path", "/index.html"],
    [":scheme", "http"], [":scheme", "https"], [":status", "200"], [":status", "204"], [":status", "206"],
    [":status", "304"], [":status", "400"], [":status", "404"], [":status", "500"], ["accept-charset", ""],
    ["accept-encoding", "gzip, deflate"], ["accept-language", ""], ["accept-ranges", ""], ["accept", ""],
    ["access-control-allow-origin", ""], ["age", ""], ["allow", ""], ["authorization", ""], ["cache-control", ""],
    ["content-disposition", ""], ["content-encoding", ""], ["content-language", ""], ["content-length", ""],
    ["content-location", ""], ["content-range", ""], ["content-type", ""], ["cookie", ""], ["date", ""], ["etag", ""],
    ["expect", ""], ["expires", ""], ["from", ""], ["host", ""], ["if-match", ""], ["if-modified-since", ""],
    ["if-none-match", ""], ["if-range", ""], ["if-unmodified-since", ""], ["last-modified", ""], ["link", ""],
    ["location", ""], ["max-forwards", ""], ["proxy-authenticate", ""], ["proxy-authorization", ""], ["range", ""],
    ["referer", ""], ["refresh", ""], ["retry-after", ""], ["server", ""], ["set-cookie", ""],
    ["strict-transport-security", ""], ["transfer-encoding", ""], ["user-agent", ""], ["vary", ""],
    ["via", ""], ["www-authenticate", ""],
  ];

  function encodeFrame(type, flags, streamId, payload) {
    payload = Buffer.from(payload || []);
    if (payload.length > 0xffffff) throw new RangeError("HTTP/2 frame payload exceeds 24-bit length");
    streamId = Number(streamId) >>> 0;
    if (streamId > 0x7fffffff) throw new RangeError("HTTP/2 stream ID exceeds 31 bits");
    const frame = Buffer.alloc(9 + payload.length);
    frame[0] = payload.length >>> 16; frame[1] = payload.length >>> 8; frame[2] = payload.length;
    frame[3] = type & 0xff; frame[4] = flags & 0xff;
    frame[5] = streamId >>> 24; frame[6] = streamId >>> 16; frame[7] = streamId >>> 8; frame[8] = streamId;
    frame.set(payload, 9);
    return frame;
  }

  class FrameDecoder {
    constructor(maxFrameSize = 16384) { this.buffer = Buffer.alloc(0); this.maxFrameSize = maxFrameSize; }
    push(chunk) {
      this.buffer = Buffer.concat([this.buffer, Buffer.from(chunk)]);
      const frames = [];
      while (this.buffer.length >= 9) {
        const length = this.buffer[0] * 65536 + this.buffer[1] * 256 + this.buffer[2];
        if (length > this.maxFrameSize) { const error = new Error("HTTP/2 frame exceeds maximum size"); error.code = "ERR_HTTP2_FRAME_SIZE_ERROR"; throw error; }
        if (this.buffer.length < 9 + length) break;
        const streamId = ((this.buffer[5] & 0x7f) * 0x1000000 + this.buffer[6] * 65536 + this.buffer[7] * 256 + this.buffer[8]) >>> 0;
        frames.push({ length, type: this.buffer[3], flags: this.buffer[4], streamId, payload: Buffer.from(this.buffer.subarray(9, 9 + length)) });
        this.buffer = this.buffer.subarray(9 + length);
      }
      return frames;
    }
  }

  function encodeInteger(value, prefixBits, first = 0) {
    value = Number(value);
    const maximum = (1 << prefixBits) - 1;
    if (!Number.isSafeInteger(value) || value < 0) throw new RangeError("invalid HPACK integer");
    if (value < maximum) return Buffer.from([first | value]);
    const bytes = [first | maximum];
    value -= maximum;
    while (value >= 128) { bytes.push((value & 127) | 128); value = Math.floor(value / 128); }
    bytes.push(value);
    return Buffer.from(bytes);
  }

  function decodeInteger(bytes, offset, prefixBits) {
    const maximum = (1 << prefixBits) - 1;
    let value = bytes[offset] & maximum;
    if (value < maximum) return [value, offset + 1];
    let shift = 0;
    for (offset++; offset < bytes.length; offset++) {
      const byte = bytes[offset];
      value += (byte & 127) * Math.pow(2, shift);
      if (!(byte & 128)) return [value, offset + 1];
      shift += 7;
      if (shift > 49) throw new RangeError("HPACK integer overflow");
    }
    throw new Error("truncated HPACK integer");
  }

  function encodeString(value) {
    const bytes = Buffer.from(String(value), "utf8");
    const encoded = huffman.encode(bytes);
    if (encoded.length < bytes.length) return Buffer.concat([encodeInteger(encoded.length, 7, 0x80), encoded]);
    return Buffer.concat([encodeInteger(bytes.length, 7), bytes]);
  }
  function decodeString(bytes, offset) {
    const compressed = !!(bytes[offset] & 0x80);
    const [length, start] = decodeInteger(bytes, offset, 7);
    if (start + length > bytes.length) throw new Error("truncated HPACK string");
    const value = bytes.subarray(start, start + length);
    return [(compressed ? huffman.decode(value) : value).toString("utf8"), start + length];
  }

  class Hpack {
    constructor(maxSize = 4096) { this.dynamic = []; this.dynamicSize = 0; this.maxSize = maxSize; }
    entry(index) {
      if (index <= 0) throw new Error("invalid HPACK index 0");
      return index < STATIC.length ? STATIC[index] : this.dynamic[index - STATIC.length];
    }
    add(name, value) {
      const size = Buffer.byteLength(name) + Buffer.byteLength(value) + 32;
      if (size > this.maxSize) { this.dynamic = []; this.dynamicSize = 0; return; }
      this.dynamic.unshift([name, value]); this.dynamicSize += size;
      while (this.dynamicSize > this.maxSize) { const item = this.dynamic.pop(); this.dynamicSize -= Buffer.byteLength(item[0]) + Buffer.byteLength(item[1]) + 32; }
    }
    encode(headers) {
      const chunks = [];
      for (const [rawName, rawValue] of Object.entries(headers)) {
        const name = rawName.toLowerCase(), value = String(rawValue);
        let exact = 0, named = 0;
        for (let i = 1; i < STATIC.length; i++) { if (STATIC[i][0] === name) { if (!named) named = i; if (STATIC[i][1] === value) { exact = i; break; } } }
        if (exact) chunks.push(encodeInteger(exact, 7, 0x80));
        else chunks.push(Buffer.concat([encodeInteger(named, 4), named ? Buffer.alloc(0) : encodeString(name), encodeString(value)]));
      }
      return Buffer.concat(chunks);
    }
    decode(input) {
      const bytes = Buffer.from(input), headers = [], result = {};
      for (let offset = 0; offset < bytes.length;) {
        const first = bytes[offset];
        if (first & 0x80) {
          const decoded = decodeInteger(bytes, offset, 7); offset = decoded[1];
          const entry = this.entry(decoded[0]); if (!entry) throw new Error("invalid HPACK index"); headers.push(entry);
        } else if ((first & 0xe0) === 0x20) {
          const decoded = decodeInteger(bytes, offset, 5); offset = decoded[1]; this.maxSize = decoded[0];
          while (this.dynamicSize > this.maxSize) { const item = this.dynamic.pop(); this.dynamicSize -= Buffer.byteLength(item[0]) + Buffer.byteLength(item[1]) + 32; }
          continue;
        } else {
          const indexed = !!(first & 0x40), prefix = indexed ? 6 : 4;
          let decoded = decodeInteger(bytes, offset, prefix); offset = decoded[1];
          let name;
          if (decoded[0]) { const entry = this.entry(decoded[0]); if (!entry) throw new Error("invalid HPACK name index"); name = entry[0]; }
          else { decoded = decodeString(bytes, offset); name = decoded[0]; offset = decoded[1]; }
          decoded = decodeString(bytes, offset); const value = decoded[0]; offset = decoded[1];
          headers.push([name, value]); if (indexed) this.add(name, value);
        }
      }
      for (const [name, value] of headers) {
        if (name in result) result[name] = Array.isArray(result[name]) ? [...result[name], value] : [result[name], value];
        else result[name] = value;
      }
      return result;
    }
  }

  Object.defineProperty(globalThis, "__lumenHttp2Codec", { value: { encodeFrame, FrameDecoder, encodeInteger, decodeInteger, Hpack }, configurable: true });
}
