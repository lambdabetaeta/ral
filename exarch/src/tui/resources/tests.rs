use super::*;

/// Every frontend row wears its policy, and none of them fakes a ceiling:
/// the view fold's row window is what bounds the scrollback, so the rows it
/// bounds name it in their note rather than claim a `cap` of their own.
#[test]
fn frontend_rows_state_decided_policies_and_name_the_one_window() {
    let rows = frontend_rows(
        ScrollbackFigures {
            blocks: 3,
            rows: 120,
            bytes: 4096,
            window: 1000,
        },
        ViewFigures {
            live: 2,
            dead: 1,
            agents: 2,
        },
        BusFigures {
            depth: 5,
            bytes: 777,
        },
    );
    let by_name = |n: &str| {
        rows.iter()
            .find(|r| r.name == n)
            .unwrap_or_else(|| panic!("row {n} must be emitted"))
    };
    assert_eq!(by_name("scrollback.blocks").current, 3);
    assert_eq!(by_name("scrollback.rows").current, 120);
    assert_eq!(by_name("scrollback.bytes").current, 4096);
    assert_eq!(by_name("views.live").current, 2);
    assert_eq!(by_name("views.dead").current, 1);
    assert_eq!(by_name("fleet.agents").current, 2);
    assert_eq!(by_name("bus.depth").current, 5);
    assert_eq!(by_name("bus.bytes").current, 777);
    assert!(
        by_name("scrollback.blocks")
            .note
            .as_deref()
            .is_some_and(|n| n.contains("1000-row window")),
        "the blocks row names the one window that bounds it"
    );
    assert!(
        rows.iter().all(|row| row.cap.is_none()),
        "no frontend row enforces a cap of its own"
    );
    assert_eq!(by_name("scrollback.blocks").policy, Policy::Evict);
    assert_eq!(by_name("bus.depth").policy, Policy::Coalesce);
    assert!(
        by_name("bus.bytes")
            .note
            .as_deref()
            .is_some_and(|n| n.contains("KiB")),
        "the bus bytes row must name the per-run elision cap"
    );
}
