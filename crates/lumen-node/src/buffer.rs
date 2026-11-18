//! Buffer's permissive base64 decoder (distinct from the strict Web `atob` API).
use lumen_host::{string_utf16_units, Ctx, Value};

fn decode(bytes: impl Iterator<Item = u8>, capacity: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(capacity / 4 * 3);
    let mut bits = 0u32;
    let mut count = 0;
    for byte in bytes {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        bits = (bits << 6) | digit as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    out
}

pub fn op_decode_base64(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let source = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?;
    let bytes = if source.is_ascii() {
        decode(source.bytes(), source.len())
    } else {
        // Node decodes the low byte of each UTF-16 unit, including lone surrogates.
        let units = string_utf16_units(&source);
        decode(units.iter().map(|&unit| unit as u8), units.len())
    };
    ctx.make_uint8array(&bytes)
}
