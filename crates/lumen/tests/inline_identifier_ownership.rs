use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn identifier_names_survive_source_reuse_and_snapshot_decode() {
    let names: Vec<String> = (1..=48)
        .map(|n| "a".repeat(n))
        .chain((1..=16).map(|n| "π".repeat(n)))
        .chain((1..=10).map(|n| "𐐀".repeat(n)))
        .collect();
    let mut source = String::from("function savedNames() { let sum = 0; ");
    for (index, name) in names.iter().enumerate() {
        source.push_str(&format!(
            "let {name}={index}; sum += {name}; {{ let {name} = 1; sum += {name}; }}"
        ));
    }
    source.push_str(
        "let \\u0065scaped = 7; escaped += 2; sum += escaped; return sum; } savedNames();",
    );
    let expected = (names.len() * (names.len() - 1) / 2 + names.len() + 9).to_string();
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for threshold in [0, 8] {
            for cached in [false, true] {
                let mut engine = Engine::new();
                engine.set_tier(tier);
                engine.set_tier_threshold(threshold);
                let mut text = source.clone();
                let result = if cached {
                    let mut snapshot = lumen::compile_snapshot(&text).unwrap();
                    let result = engine.eval_snapshot(&snapshot, false);
                    snapshot.fill(0xff);
                    result
                } else {
                    engine.eval(&text, false)
                };
                match result.unwrap() {
                    Completion::Value(value) => {
                        assert_eq!(value, expected, "{tier:?}, {threshold}, {cached}")
                    }
                    Completion::Throw { name, message } => panic!("{name}: {message}"),
                }
                text.clear();
                text.push_str("reused source");
                drop(text);
                match engine.eval("savedNames()", false).unwrap() {
                    Completion::Value(value) => {
                        assert_eq!(value, expected, "{tier:?}, {threshold}, {cached}")
                    }
                    Completion::Throw { name, message } => panic!("{name}: {message}"),
                }
            }
        }
    }
}
