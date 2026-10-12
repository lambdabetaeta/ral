use super::super::value::Value;
use super::*;
use crate::types::Nursery;

/// A `Fork::Listen` run has no pen, and says so rather than reusing the
/// absent-host sentence: the two are different situations.
#[test]
fn fork_into_nursery_refuses_a_listening_run_with_its_own_sentence() {
    let shell = crate::test_helper::core_shell();
    let mooring = Mooring {
        fork: Some(Fork::Listen),
        ..Mooring::adrift()
    };
    match shell
        .fork_into_nursery(&mooring)
        .expect_err("a listening run parks nothing")
    {
        crate::types::Break::Error(e) => {
            assert_eq!(
                e.message,
                "this host's forked sessions leave over a wire, so there is no pen to park one in"
            );
        }
        other @ crate::types::Break::Escape(_) => {
            panic!("expected Break::Error, got {other:?}")
        }
    }
}

/// The nursery twin of `enquire`'s absent-desk contract, error text and all.
#[test]
fn fork_into_nursery_errors_honestly_without_a_nursery() {
    let shell = crate::test_helper::core_shell();
    match shell
        .fork_into_nursery(&Mooring::adrift())
        .expect_err("no nursery is installed")
    {
        crate::types::Break::Error(e) => {
            assert_eq!(e.message, "this host adopts no forked sessions");
        }
        other @ crate::types::Break::Escape(_) => {
            panic!("expected Break::Error, got {other:?}")
        }
    }
}

/// Park then adopt returns a live child `Shell` still carrying the
/// parent's whole lexical scope, as `fork_session` promises.
#[test]
fn nursery_round_trips_a_forked_session() {
    let mut shell = crate::test_helper::core_shell();
    shell.set_var("parent_binding".to_string(), Value::Int(42));
    let nursery = Nursery::default();
    let mooring = Mooring {
        fork: Some(Fork::Park(nursery.clone())),
        ..Mooring::adrift()
    };

    let id = shell
        .fork_into_nursery(&mooring)
        .expect("a nursery is installed");
    let child = nursery
        .adopt(id)
        .expect("the parked fork must be adoptable");

    assert_eq!(
        child.env.get("parent_binding"),
        Some(&Value::Int(42)),
        "the fork must carry the parent's lexical scope"
    );
}
