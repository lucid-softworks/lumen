use crate::{
    ast::{PropKey, Stmt},
    bytecode,
};

#[test]
fn computed_data_objects_preserve_keys_definitions_and_names() {
    super::logical_assignment::exercise(
        include_str!("../../tests/fixtures/computed-objects.js"),
        7,
        0,
        false,
    );
}

#[test]
fn computed_object_keys_survive_await() {
    super::logical_assignment::exercise(
        include_str!("../../tests/fixtures/computed-objects-async.js"),
        1,
        0,
        true,
    );
}

#[test]
fn rollup_style_path_tracker_compiles() {
    let source = include_str!("../../tests/fixtures/computed-objects.js");
    let body =
        crate::parser::parse_script(source, false).unwrap_or_else(|e| panic!("{}", e.message));
    let mut checked = 0;
    for statement in &body {
        if let Stmt::ClassDecl(class) = statement {
            if class.name.as_deref() == Some("EntityPathTracker") {
                for member in &class.members {
                    if matches!(&member.key, PropKey::Ident(name) if name == "getEntities") {
                        let function = member.func.as_ref().unwrap();
                        let chunk =
                            bytecode::compile(function).expect("the measured tracker must compile");
                        assert!(chunk
                            .jit_ops()
                            .iter()
                            .any(|op| matches!(op, bytecode::Op::DefineLiteralData(_))));
                        checked += 1;
                    }
                }
            }
        }
    }
    assert_eq!(checked, 1);
}
