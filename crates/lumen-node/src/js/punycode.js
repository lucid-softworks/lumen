// node:punycode — RFC 3492 bootstring, a port of the punycode.js algorithm Node bundles.
// Pure JS; loads before shims.js so node:url can back domainToASCII/domainToUnicode with it.

const maxInt = 2147483647; // aka 0x7FFFFFFF
const base = 36;
const tMin = 1;
const tMax = 26;
const skew = 38;
const damp = 700;
const initialBias = 72;
const initialN = 128; // 0x80
const delimiter = "-"; // '\x2D'
const baseMinusTMin = base - tMin;

const regexPunycode = /^xn--/;
const regexNonASCII = /[^\x00-\x7F]/;
// RFC 3490 separators: '.', ideographic/fullwidth/halfwidth full stops.
const regexSeparators = /[\x2E。．｡]/g;

const errors = {
  overflow: "Overflow: input needs wider integers to process",
  "not-basic": "Illegal input >= 0x80 (not a basic code point)",
  "invalid-input": "Invalid input",
};

function error(type) {
  throw new RangeError(errors[type]);
}

// Applies `callback` to every label of the domain, passing through a leading `user@` part and
// normalizing the RFC 3490 dot variants to '.'.
function mapDomain(domain, callback) {
  const parts = String(domain).split("@");
  let result = "";
  if (parts.length > 1) {
    result = parts[0] + "@";
    domain = parts[1];
  } else {
    domain = parts[0];
  }
  domain = domain.replace(regexSeparators, "\x2E");
  return result + domain.split(".").map(callback).join(".");
}

// UCS-2 string -> array of Unicode code points (combining surrogate pairs; lone surrogates pass
// through as-is, matching punycode.js).
function ucs2decode(string) {
  const output = [];
  let counter = 0;
  const length = string.length;
  while (counter < length) {
    const value = string.charCodeAt(counter++);
    if (value >= 0xd800 && value <= 0xdbff && counter < length) {
      const extra = string.charCodeAt(counter++);
      if ((extra & 0xfc00) === 0xdc00) {
        output.push(((value & 0x3ff) << 10) + (extra & 0x3ff) + 0x10000);
      } else {
        // Unmatched high surrogate: keep it, and reprocess the next unit.
        output.push(value);
        counter--;
      }
    } else {
      output.push(value);
    }
  }
  return output;
}

const ucs2encode = (codePoints) => String.fromCodePoint(...codePoints);

// Basic code point -> digit value, or `base` if not a valid digit (RFC 3492 §5, case-insensitive).
function basicToDigit(codePoint) {
  if (codePoint >= 0x30 && codePoint < 0x3a) return 26 + (codePoint - 0x30); // '0'..'9'
  if (codePoint >= 0x41 && codePoint < 0x5b) return codePoint - 0x41; // 'A'..'Z'
  if (codePoint >= 0x61 && codePoint < 0x7b) return codePoint - 0x61; // 'a'..'z'
  return base;
}

// Digit -> basic code point; `flag` nonzero selects uppercase (we always pass 0 -> lowercase).
function digitToBasic(digit, flag) {
  return digit + 22 + (digit < 26 ? 75 : 0) - (flag ? 32 : 0);
}

// Bias adaptation (RFC 3492 §3.4).
function adapt(delta, numPoints, firstTime) {
  let k = 0;
  delta = firstTime ? Math.floor(delta / damp) : delta >> 1;
  delta += Math.floor(delta / numPoints);
  for (; delta > (baseMinusTMin * tMax) >> 1; k += base) {
    delta = Math.floor(delta / baseMinusTMin);
  }
  return Math.floor(k + ((baseMinusTMin + 1) * delta) / (delta + skew));
}

// Decodes a Punycode string of ASCII-only symbols to a string of Unicode symbols (RFC 3492 §6.2).
function decode(input) {
  const output = [];
  const inputLength = input.length;
  let i = 0;
  let n = initialN;
  let bias = initialBias;

  // Handle the basic code points: they're all before the last delimiter, if any.
  let basic = input.lastIndexOf(delimiter);
  if (basic < 0) basic = 0;
  for (let j = 0; j < basic; ++j) {
    if (input.charCodeAt(j) >= 0x80) error("not-basic");
    output.push(input.charCodeAt(j));
  }

  // Decode each delta as a generalized variable-length integer, then insert.
  for (let index = basic > 0 ? basic + 1 : 0; index < inputLength; ) {
    const oldi = i;
    for (let w = 1, k = base; ; k += base) {
      if (index >= inputLength) error("invalid-input");
      const digit = basicToDigit(input.charCodeAt(index++));
      if (digit >= base) error("invalid-input");
      if (digit > Math.floor((maxInt - i) / w)) error("overflow");
      i += digit * w;
      const t = k <= bias ? tMin : k >= bias + tMax ? tMax : k - bias;
      if (digit < t) break;
      const baseMinusT = base - t;
      if (w > Math.floor(maxInt / baseMinusT)) error("overflow");
      w *= baseMinusT;
    }
    const out = output.length + 1;
    bias = adapt(i - oldi, out, oldi === 0);
    if (Math.floor(i / out) > maxInt - n) error("overflow");
    n += Math.floor(i / out);
    i %= out;
    output.splice(i++, 0, n);
  }

  return String.fromCodePoint(...output);
}

// Encodes a string of Unicode symbols to a Punycode string of ASCII-only symbols (RFC 3492 §6.3).
function encode(input) {
  const output = [];
  const codePoints = ucs2decode(String(input));
  const inputLength = codePoints.length;
  let n = initialN;
  let delta = 0;
  let bias = initialBias;

  for (const currentValue of codePoints) {
    if (currentValue < 0x80) output.push(String.fromCharCode(currentValue));
  }
  const basicLength = output.length;
  let handledCPCount = basicLength;
  if (basicLength) output.push(delimiter);

  while (handledCPCount < inputLength) {
    // Find the next larger non-basic code point >= n.
    let m = maxInt;
    for (const currentValue of codePoints) {
      if (currentValue >= n && currentValue < m) m = currentValue;
    }

    // Increase delta to advance the decoder's <n,i> state to <m,0>.
    const handledCPCountPlusOne = handledCPCount + 1;
    if (m - n > Math.floor((maxInt - delta) / handledCPCountPlusOne)) error("overflow");
    delta += (m - n) * handledCPCountPlusOne;
    n = m;

    for (const currentValue of codePoints) {
      if (currentValue < n && ++delta > maxInt) error("overflow");
      if (currentValue === n) {
        // Represent delta as a generalized variable-length integer.
        let q = delta;
        for (let k = base; ; k += base) {
          const t = k <= bias ? tMin : k >= bias + tMax ? tMax : k - bias;
          if (q < t) break;
          const qMinusT = q - t;
          const baseMinusT = base - t;
          output.push(String.fromCharCode(digitToBasic(t + (qMinusT % baseMinusT), 0)));
          q = Math.floor(qMinusT / baseMinusT);
        }
        output.push(String.fromCharCode(digitToBasic(q, 0)));
        bias = adapt(delta, handledCPCountPlusOne, handledCPCount === basicLength);
        delta = 0;
        ++handledCPCount;
      }
    }

    ++delta;
    ++n;
  }
  return output.join("");
}

// Domain-level converters: only labels that need it are decoded/encoded.
const toUnicode = (input) =>
  mapDomain(input, (label) => (regexPunycode.test(label) ? decode(label.slice(4).toLowerCase()) : label));
const toASCII = (input) =>
  mapDomain(input, (label) => (regexNonASCII.test(label) ? "xn--" + encode(label) : label));

__builtins.set("punycode", {
  // The punycode.js version Node v22 bundles; kept for feature-detecting consumers.
  version: "2.1.0",
  ucs2: { decode: ucs2decode, encode: ucs2encode },
  decode,
  encode,
  toASCII,
  toUnicode,
});
