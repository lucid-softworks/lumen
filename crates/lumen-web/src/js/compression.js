// CompressionStream / DecompressionStream over the native DEFLATE codec. The codec is one-shot, so
// each stream buffers its input and (de)compresses the whole thing on flush — valid output, though
// not incremental. Formats: 'gzip', 'deflate' (zlib-wrapped), 'deflate-raw'.

const COMPRESS = { gzip: __compress.gzip, deflate: __compress.deflate, "deflate-raw": __compress.deflateRaw };
const DECOMPRESS = { gzip: __compress.gunzip, deflate: __compress.inflate, "deflate-raw": __compress.inflateRaw };

function concatChunks(chunks) {
  let size = 0;
  for (const c of chunks) size += c.length;
  const out = new Uint8Array(size);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

function toU8(chunk) {
  if (chunk instanceof Uint8Array) return chunk;
  if (chunk instanceof ArrayBuffer) return new Uint8Array(chunk);
  if (ArrayBuffer.isView(chunk)) return new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
  throw new TypeError("CompressionStream expects BufferSource chunks");
}

function makeCodecStream(format, table, label) {
  const codec = table[format];
  if (!codec) throw new TypeError(`Unsupported ${label} format: '${format}'`);
  const chunks = [];
  return new TransformStream({
    transform(chunk) {
      chunks.push(toU8(chunk));
    },
    flush(controller) {
      const input = concatChunks(chunks);
      const result = codec(input); // throws on malformed compressed input
      if (result.length) controller.enqueue(result);
    },
  });
}

class CompressionStream {
  constructor(format) {
    this._ts = makeCodecStream(format, COMPRESS, "CompressionStream");
  }
  get readable() {
    return this._ts.readable;
  }
  get writable() {
    return this._ts.writable;
  }
}

class DecompressionStream {
  constructor(format) {
    this._ts = makeCodecStream(format, DECOMPRESS, "DecompressionStream");
  }
  get readable() {
    return this._ts.readable;
  }
  get writable() {
    return this._ts.writable;
  }
}

globalThis.CompressionStream = CompressionStream;
globalThis.DecompressionStream = DecompressionStream;
