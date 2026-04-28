#![cfg(unix)]

use ral_core::{elaborate, parse};

fn infer(src: &str) -> String {
    let ast = parse(src).expect("parse");
    let comp = elaborate(&ast, Default::default());
    ral_core::ty::infer_comp(&comp).to_string()
}

#[test]
fn branch_unify_bytes() {
    let ty = infer("if true { echo a } else { echo b }");
    assert_eq!(ty, "F_{bytes, bytes}");
}

#[test]
fn branch_unify_value() {
    let ty = infer("if true { return 1 } else { return 2 }");
    assert_eq!(ty, "F_{none, none}");
}

#[test]
fn branch_unify_mismatch_is_unknown() {
    let ty = infer("if true { echo a } else { return 2 }");
    assert_eq!(ty, "F_{var, var}");
}

#[test]
fn last_thunk_for_bytes() {
    let ty = infer("for [1, 2] { echo x }");
    assert_eq!(ty, "F_{bytes, bytes}");
}

#[test]
fn last_thunk_for_value() {
    let ty = infer("for [1, 2] { return 1 }");
    assert_eq!(ty, "F_{none, none}");
}

#[test]
fn head_sig_modes() {
    use ral_core::ty::{Mode, ModeUnifier, head_sig};

    let mut unifier = ModeUnifier::new();
    let read = head_sig("from-json", &[], &mut unifier);
    assert_eq!(read.input, Mode::Bytes);
    assert_eq!(read.output, Mode::None);

    let write = head_sig("to-json", &[], &mut unifier);
    assert_eq!(write.input, Mode::None);
    assert_eq!(write.output, Mode::Bytes);

    let ext = head_sig("definitely-not-a-real-command", &[], &mut unifier);
    assert_eq!(ext.to_string(), "F_{bytes, bytes}");
}
