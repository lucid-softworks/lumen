use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn function_source_preserves_unicode_comments_and_template_offsets() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let result = engine
            .eval(
                r#"
            function assert(v) { if (!v) throw new Error('function source mismatch'); }
            const sources = [
                'function plain(x) { return x + 1; }',
                'function café(π) { /* 🌙 */ return π + "λ"; }',
                '(π) => /* é */ π + "🐻"',
                'async function café() { return "é"; }',
                'function* café() { yield "🌙"; }',
                'class Café { method(π) { return "🌙"; } }',
                'function crlf() {\r\n return "é";\r\n}'
            ];
            for (const source of sources) {
                for (const prefix of ['/* ascii */', '/* é🐻 */']) {
                    assert(eval(prefix + '(' + source + ')').toString() === source);
                }
            }
            const inner = `${function café(){return '🌙';}}`;
            assert(inner === "function café(){return '🌙';}");
            function tag(strings, value) { return value.toString(); }
            assert(tag`🐻${function café(){return 'é';}}` === "function café(){return 'é';}");
            'passed'
        "#,
                false,
            )
            .unwrap();
        match result {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
