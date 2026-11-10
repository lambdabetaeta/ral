use super::super::value::Value;
use super::*;
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::Nursery;

/// A narrower no test here may reach: only a Base grant consults one.
fn no_base(
    _: &str,
    _: &crate::authority::grant::freeze::FreezeCtx,
) -> Result<crate::authority::policy::GrantLayer, crate::error::Error> {
    panic!("only a Base grant consults the narrower")
}

/// Park then adopt returns a bequest from which a child still carries the
/// parent's whole lexical scope.
#[test]
fn nursery_round_trips_a_forked_session() {
    let mut shell = crate::test_helper::core_shell();
    shell.set_var("parent_binding".to_string(), Value::Int(42));
    let nursery = Nursery::default();
    let id = nursery.park(shell.bequeath(SpawnGrant::Inherit));
    let adopted = nursery
        .adopt(id)
        .expect("the parked bequest must be adoptable");
    let child = Shell::child_of(crate::test_helper::core_runtime().into(), adopted, no_base)
        .expect("a child");

    assert_eq!(
        child.env.get("parent_binding"),
        Some(&Value::Int(42)),
        "the bequest must carry the parent's lexical scope"
    );
}

#[test]
fn refused_host_blocks_install_neither_bindings_nor_hooks() {
    use crate::ir::{CommandName, CommandWord, CompKind, Exec, Val, ValListElem};
    use crate::shell::runtime::{Closure, DefaultPolicy, HookName, HookSig, RegisterError};
    use crate::source::Spanned;
    let mut shell = crate::test_helper::core_shell();
    let body = Spanned::synthetic(CompKind::Exec(Exec {
        head: CommandWord::External(CommandName::Bare("echo".into())),
        argv: vec![ValListElem::Single(Spanned::synthetic(Val::Unit))].into(),
        redirects: crate::ir::Redirects::default(),
        site: None,
    }));
    let value = Value::Thunk(Closure::closed(body));
    shell.bind_value("host".into(), Value::Int(7)).unwrap();
    let error = shell.bind_value("host".into(), value.clone()).unwrap_err();
    assert_eq!(error.kind.code(), "T0057");
    assert_eq!(shell.scope_lookup("host"), Some(&Value::Int(7)));
    let name = HookName::session("refused");
    let error = shell
        .register_hook(name.clone(), value, HookSig::Block, DefaultPolicy::denied())
        .unwrap_err();
    assert!(
        matches!(error, RegisterError::Typecheck { error, .. } if error.kind.code() == "T0057")
    );
    assert!(!shell.has_hook(&name));
}
