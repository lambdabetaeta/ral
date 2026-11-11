//! Values a run hands back, read directly: what a `$name` dereference yields
//! and what a `race` returns.

use crate::common::must_succeed;
use ral_core::shell::runtime::Value;

/// `$upper` is a plain env hit on the native — the entry *is* the value.
#[test]
fn return_deref_name_is_bound_value() {
    let v = must_succeed("return $upper");
    match v {
        Value::Native(entry) => assert_eq!(entry.decl.name, "upper"),
        other => panic!("expected the native itself, got {other:?}"),
    }
}

#[test]
fn lexical_non_head_name_uses_deref_to_get_value() {
    let v = must_succeed("let f = { |x| return $x }\nreturn $f");
    assert!(matches!(v, Value::Thunk(_)), "expected thunk, got {v:?}");
}

#[test]
fn list_position_deref_gives_thunk() {
    // $upper in list position is a variable lookup.
    let v = must_succeed("let upper = { |x| return $x }\nreturn [$upper]");
    match v {
        Value::List(items) => {
            assert!(
                matches!(items.get(0).as_deref(), Some(Value::Thunk(_))),
                "expected thunk, got {:?}",
                items.get(0)
            );
        }
        other => panic!("expected list, got {other:?}"),
    }
}

#[test]
fn race_first_wins() {
    // Two spawns: one returns immediately, one sleeps. Race picks the fast one.
    assert_eq!(
        must_succeed(
            r"
            let fast = !{spawn { return winner }}
            let slow = !{spawn { sleep 10; return loser }}
            let r = race [$fast, $slow]
            return $r
        "
        ),
        Value::string("winner")
    );
}
