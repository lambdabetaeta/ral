use super::super::value::Value;
use super::*;
use crate::guard::SpawnGrant;
use crate::types::Nursery;

/// A narrower no test here may reach: only a Base grant consults one.
fn no_base(_: &str, _: &std::path::Path) -> Result<crate::capability::Capabilities, String> {
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
    let child = Shell::child_of(shell.runtime(), adopted, no_base).expect("a child");

    assert_eq!(
        child.env.get("parent_binding"),
        Some(&Value::Int(42)),
        "the bequest must carry the parent's lexical scope"
    );
}
