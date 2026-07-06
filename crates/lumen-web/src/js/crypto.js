// crypto.getRandomValues / randomUUID / subtle.digest, plus navigator.

const INTEGER_VIEWS = [
  Int8Array,
  Uint8Array,
  Uint8ClampedArray,
  Int16Array,
  Uint16Array,
  Int32Array,
  Uint32Array,
  BigInt64Array,
  BigUint64Array,
];

class SubtleCrypto {
  async digest(algorithm, data) {
    const name = (typeof algorithm === "string" ? algorithm : algorithm && algorithm.name) || "";
    if (String(name).toUpperCase() !== "SHA-256") {
      throw new DOMException(`unsupported digest algorithm '${name}' (SHA-256 only for now)`, "NotSupportedError");
    }
    let view;
    if (data instanceof ArrayBuffer) view = new Uint8Array(data);
    else if (ArrayBuffer.isView(data)) view = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    else throw new TypeError("digest expects a BufferSource");
    return __crypto.sha256(view).buffer;
  }
}

const subtle = new SubtleCrypto();

class Crypto {
  getRandomValues(view) {
    if (!ArrayBuffer.isView(view) || !INTEGER_VIEWS.some((T) => view instanceof T)) {
      throw new TypeError("getRandomValues expects an integer typed array");
    }
    if (view.byteLength > 65536) {
      throw new DOMException("getRandomValues: quota (65536 bytes) exceeded", "QuotaExceededError");
    }
    // Fill by bytes over a Uint8Array sharing the same buffer region.
    const bytes = new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
    __crypto.fill(bytes);
    return view;
  }
  randomUUID() {
    return __crypto.uuid();
  }
  get subtle() {
    return subtle;
  }
}

globalThis.Crypto = Crypto;
globalThis.SubtleCrypto = SubtleCrypto;
globalThis.crypto = new Crypto();
globalThis.CryptoKey = class CryptoKey {}; // constructor-less placeholder for `instanceof`

if (typeof globalThis.navigator === "undefined") {
  globalThis.navigator = {};
}
Object.defineProperty(globalThis.navigator, "userAgent", {
  value: "lumen",
  enumerable: true,
  configurable: true,
});
