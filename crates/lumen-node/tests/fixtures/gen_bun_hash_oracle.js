// Oracle generator for Bun.hash — run with Bun v1.2.21.
// Emits lines: family|seedHex|inputId|valueHex
// value is u64 hex (16 nibble) for bigint families, u32 hex (8 nibble) for number families.
// Byte inputs are regenerated identically in Rust; see fixture header for the rules.

const NUM_FAMILIES = new Set(["cityHash32","xxHash32","murmur32v3","murmur32v2","crc32","adler32"]);
const FAMILIES = ["wyhash","cityHash32","cityHash64","xxHash32","xxHash64","xxHash3",
                  "murmur32v3","murmur32v2","murmur64v2","rapidhash","crc32","adler32"];

// deterministic pattern byte: (i*31+7) & 0xFF
function pat(n){ const b=new Uint8Array(n); for(let i=0;i<n;i++) b[i]=(i*31+7)&0xFF; return b; }

// UTF-8 multibyte fixed sample.
const UTF8 = new TextEncoder().encode("héllo wörld 你好 🌍 café Ⓐ ∑∫ 𝕳𝖊𝖑𝖑𝖔");

const inputs = [];
inputs.push(["empty", new Uint8Array(0)]);
for(let n=1;n<=130;n++) inputs.push(["len"+n, pat(n)]);
inputs.push(["k1", pat(1024)]);
inputs.push(["k64", pat(65536)]);
inputs.push(["zero64", new Uint8Array(64)]);
{ const f=new Uint8Array(64); f.fill(0xFF); inputs.push(["ff64", f]); }
inputs.push(["utf8", UTF8]);

// seeds: numbers small, bigints for large. Emitted key is 16-nibble hex of the u64 seed.
const seeds = [0n, 1n, 0xdeadbeefn, 0x8000000000000000n, 0x7fffffffffffffffn, 0xffffffffffffffffn];
function seedArg(s){
  // pass small seeds as Number where safe, large as BigInt — Bun coerces both to u64 identically
  if (s <= 0xdeadbeefn) return Number(s);
  return s;
}
function u64hex(v){ return BigInt.asUintN(64, BigInt(v)).toString(16).padStart(16,"0"); }
function u32hex(v){ return (BigInt(v) & 0xffffffffn).toString(16).padStart(8,"0"); }

const out = [];
out.push("# Bun.hash oracle — Bun " + Bun.version);
out.push("# format: family|seedHex(u64)|inputId|valueHex");
out.push("# number-families (u32 result): "+[...NUM_FAMILIES].join(","));
out.push("# byte inputs: empty=[]; lenN = bytes (i*31+7)&0xFF for i in 0..N; k1=len1024; k64=len65536;");
out.push("#   zero64=64x0x00; ff64=64x0xFF; utf8=utf8 of 'héllo wörld 你好 🌍 café Ⓐ ∑∫ 𝕳𝖊𝖑𝖑𝖔'");
for (const fam of FAMILIES) {
  const isNum = NUM_FAMILIES.has(fam);
  const fn = Bun.hash[fam];
  for (const s of seeds) {
    const sk = u64hex(s);
    const arg = seedArg(s);
    for (const [id, bytes] of inputs) {
      let v;
      try { v = fn(bytes, arg); } catch(e){ v = null; }
      if (v === null) continue;
      out.push(fam+"|"+sk+"|"+id+"|"+(isNum?u32hex(v):u64hex(v)));
    }
  }
}
// default Bun.hash(x, seed) == wyhash — sanity subset
for (const s of seeds) {
  const sk = u64hex(s); const arg = seedArg(s);
  for (const [id, bytes] of inputs) {
    out.push("default|"+sk+"|"+id+"|"+u64hex(Bun.hash(bytes, arg)));
  }
}
await Bun.write(new URL("./bun_hash_oracle.txt", import.meta.url).pathname, out.join("\n") + "\n");
console.log("wrote", out.length, "lines");
