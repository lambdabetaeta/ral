use super::{MERGE_TEXT_CAP, Signal, channel};
use crate::record::{AgentId, Transient};
use std::sync::mpsc::TryRecvError;

const ONE: AgentId = AgentId::new(1);

#[test]
fn bus_queue_token_flood_coalesces_to_one_entry_in_order() {
    let (tx, rx) = channel();
    for i in 0..200 {
        tx.send_signal(Signal::Transient(
            AgentId::new(1),
            Transient::Token(i.to_string()),
        ))
        .unwrap();
    }
    assert_eq!(
        rx.depth(),
        1,
        "an uninterrupted same-agent token run merges into one entry"
    );
    let expected: String = (0..200).map(|i| i.to_string()).collect();
    match rx.try_recv().expect("the merged entry") {
        Signal::Transient(ONE, Transient::Token(text)) => {
            assert_eq!(text, expected, "concatenation keeps arrival order");
        }
        _ => panic!("expected a merged Token entry"),
    }
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

/// The newest tail is the part worth keeping, so elision takes the front.
#[test]
fn bus_queue_flood_past_the_byte_cap_yields_one_overflow_marker() {
    let (tx, rx) = channel();
    let first = "a".repeat(MERGE_TEXT_CAP);
    tx.send_signal(Signal::Transient(AgentId::new(1), Transient::Token(first)))
        .unwrap();
    let overflow = "b".repeat(100);
    tx.send_signal(Signal::Transient(
        AgentId::new(1),
        Transient::Token(overflow.clone()),
    ))
    .unwrap();

    match rx.try_recv().expect("the merged, capped entry") {
        Signal::Transient(ONE, Transient::Token(text)) => {
            assert_eq!(
                text.len(),
                MERGE_TEXT_CAP,
                "elision holds the entry at the cap"
            );
            assert!(text.ends_with(&overflow), "the newest tail survives");
            assert!(
                text.starts_with(&"a".repeat(MERGE_TEXT_CAP - 100)),
                "exactly the 100 oldest bytes were elided from the front, no more"
            );
        }
        _ => panic!("expected the merged Token entry"),
    }

    match rx.try_recv().expect("exactly one overflow marker") {
        Signal::Transient(ONE, Transient::Fault { text }) => {
            assert!(text.contains("100"), "names the elided count: {text}");
            assert!(text.contains("token"), "names the class: {text}");
        }
        _ => panic!("expected a Transient::Fault overflow marker"),
    }
    assert!(
        matches!(rx.try_recv(), Err(TryRecvError::Empty)),
        "exactly one marker, nothing else"
    );
}

/// Two floods either side of a reserved signal stay two entries rather
/// than merging through it, which is what keeps ordering intact.
#[test]
fn bus_queue_lifecycle_events_survive_a_flood_uncrossed() {
    let (tx, rx) = channel();
    tx.send_signal(Signal::Transient(
        AgentId::new(1),
        Transient::Born {
            log_dir: std::path::PathBuf::new(),
            name: "a".into(),
            parent: Some(AgentId::new(0)),
        },
    ))
    .unwrap();
    for _ in 0..50 {
        tx.send_signal(Signal::Transient(
            AgentId::new(1),
            Transient::Token("x".into()),
        ))
        .unwrap();
    }
    tx.send_signal(Signal::Transient(AgentId::new(1), Transient::Boundary))
        .unwrap();
    for _ in 0..50 {
        tx.send_signal(Signal::Transient(
            AgentId::new(1),
            Transient::Token("y".into()),
        ))
        .unwrap();
    }
    tx.send_signal(Signal::Transient(AgentId::new(1), Transient::Died))
        .unwrap();

    assert_eq!(
        rx.depth(),
        5,
        "Born, one merged run, Boundary, one merged run, Died: five entries"
    );
    assert!(matches!(
        rx.try_recv().unwrap(),
        Signal::Transient(ONE, Transient::Born { .. })
    ));
    match rx.try_recv().unwrap() {
        Signal::Transient(ONE, Transient::Token(t)) => assert_eq!(t, "x".repeat(50)),
        _ => panic!("expected the pre-Boundary merged run"),
    }
    assert!(matches!(
        rx.try_recv().unwrap(),
        Signal::Transient(ONE, Transient::Boundary)
    ));
    match rx.try_recv().unwrap() {
        Signal::Transient(ONE, Transient::Token(t)) => assert_eq!(t, "y".repeat(50)),
        _ => panic!("expected the post-Boundary merged run"),
    }
    assert!(matches!(
        rx.try_recv().unwrap(),
        Signal::Transient(ONE, Transient::Died)
    ));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

/// A `State` is superseded by the next one, so the merge rule replaces it
/// in place rather than growing the entry: a frontend that fell behind
/// resumes at the state the agent is in, not the one it was leaving.
#[test]
fn bus_queue_newer_state_replaces_older() {
    let (tx, rx) = channel();
    tx.send_signal(Signal::Transient(
        AgentId::new(1),
        Transient::State(crate::record::AgentState::AwaitingModel),
    ))
    .unwrap();
    tx.send_signal(Signal::Transient(
        AgentId::new(1),
        Transient::State(crate::record::AgentState::Evicting),
    ))
    .unwrap();
    assert_eq!(
        rx.depth(),
        1,
        "a same-agent State run replaces in place rather than growing"
    );
    match rx.try_recv().unwrap() {
        Signal::Transient(ONE, Transient::State(s)) => assert_eq!(
            s,
            crate::record::AgentState::Evicting,
            "the newer state replaced the older"
        ),
        _ => panic!("expected State"),
    }
}

/// The merge rule keys on agent id as well as class.
#[test]
fn bus_queue_never_merges_across_agents() {
    let (tx, rx) = channel();
    tx.send_signal(Signal::Transient(
        AgentId::new(1),
        Transient::Token("a".into()),
    ))
    .unwrap();
    tx.send_signal(Signal::Transient(
        AgentId::new(2),
        Transient::Token("b".into()),
    ))
    .unwrap();
    tx.send_signal(Signal::Transient(
        AgentId::new(1),
        Transient::Token("c".into()),
    ))
    .unwrap();
    assert_eq!(
        rx.depth(),
        3,
        "an interleaving agent id never merges into another agent's tail entry"
    );
    for (want_id, want_text) in [
        (AgentId::new(1), "a"),
        (AgentId::new(2), "b"),
        (AgentId::new(1), "c"),
    ] {
        match rx.try_recv().expect("three separate entries") {
            Signal::Transient(id, Transient::Token(t)) => {
                assert_eq!(id, want_id);
                assert_eq!(t, want_text);
            }
            _ => panic!("expected Token"),
        }
    }
}

#[test]
fn bus_queue_bytes_tracks_resident_merged_text() {
    let (tx, rx) = channel();
    assert_eq!(rx.bytes(), 0);
    tx.send_signal(Signal::Transient(
        AgentId::new(1),
        Transient::Token("abc".into()),
    ))
    .unwrap();
    assert_eq!(rx.bytes(), 3);
    tx.send_signal(Signal::Transient(
        AgentId::new(1),
        Transient::Token("de".into()),
    ))
    .unwrap();
    assert_eq!(rx.bytes(), 5, "the merge grows the byte figure");
    let _ = rx.try_recv().unwrap();
    assert_eq!(rx.bytes(), 0, "draining the entry frees its bytes");
}

/// `Emitter::muted_child` leans on this to swallow a display stream forever
/// without leaking the queue behind it.
#[test]
fn bus_sender_send_past_dropped_receiver_is_rejected_not_grown() {
    let (tx, rx) = channel();
    drop(rx);
    let err = tx
        .send_signal(Signal::Transient(
            AgentId::new(1),
            Transient::Token("x".into()),
        ))
        .unwrap_err();
    assert!(matches!(err.0, Signal::Transient(ONE, Transient::Token(ref s)) if s == "x"));
}
