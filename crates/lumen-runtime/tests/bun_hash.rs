use lumen_runtime::{Completion, Runtime};

#[test]
fn crc32_preserves_bun_input_coercion_and_ignores_seeds() {
    let mut runtime = Runtime::new();
    // Expected values recorded from Bun 1.3.10. In particular, missing and explicit
    // undefined arguments differ, and views hash only their visible bytes.
    let source = r#"
        const bytes = new Uint8Array(256);
        for (let i = 0; i < bytes.length; i++) bytes[i] = i;
        const cases = [
            ['', 0], ['123456789', 3421780262], ['héllo 🌍', 3711469530],
            [undefined, 4275843151], [null, 634125391], [42, 841265288],
            [bytes, 688229491], [bytes.buffer, 688229491],
            [bytes.subarray(17, 43), 2893359834],
            [new DataView(bytes.buffer, 17, 26), 2893359834],
            [Buffer.from(bytes).subarray(17, 43), 2893359834],
            [{toString() { return '123456789'; }}, 3421780262]
        ];
        if (Bun.hash.crc32() !== 0) throw new Error('missing argument');
        // The wrapper uses its private native operation even if the public Node API changes.
        require('node:zlib').crc32 = () => { throw new Error('public crc32 called'); };
        for (const [input, expected] of cases) {
            for (const seed of [undefined, 123, -1, 0xffffffffffffffffn, Symbol('ignored')]) {
                if (Bun.hash.crc32(input, seed) !== expected) throw new Error('crc32 mismatch');
            }
        }
        let threw = false;
        try { Bun.hash.crc32({toString() { throw new Error('coercion'); }}); }
        catch (error) { threw = error.message === 'coercion'; }
        if (!threw) throw new Error('coercion error was swallowed');
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}
