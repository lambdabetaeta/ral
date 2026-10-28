use super::*;
use crate::agent::TestTrunk;
use crate::record::Span;

fn load() -> Load {
    Load {
        used: 400,
        total: Some(500),
        trigger: 450,
        unit: Unit::Tokens,
        planned: Some(Span::new(1, 7).expect("a run")),
    }
}

fn over() -> Pressure {
    Pressure::Over(load())
}

#[test]
fn pressure_line_sits_a_full_reserve_before_the_trigger() {
    let w = 200_000;
    let trigger = eviction_trigger(w);
    assert!(!forced_eviction_due(trigger, w) && forced_eviction_due(trigger + 1, w));
    assert!(
        pressure_due(trigger, w),
        "eviction due implies pressure due"
    );
    assert!(!pressure_due(w / 2, w), "half a window is not pressure");
}

/// The reminder carries the reading and the trigger and the span a forced
/// eviction would take; the wording is the nudge module's.
#[test]
fn pressure_hands_the_model_its_reading_and_the_planned_cut() {
    let Some(Warning::Model(Reminder::Pressure(reminded))) = Gauges::default().pressure(over())
    else {
        panic!("pressure due should remind the model");
    };
    assert_eq!(reminded, load());
}

/// A stale token measure (`Unknown`) neither warns nor re-arms a warning
/// still owed.
#[test]
fn stale_measure_neither_warns_nor_rearms() {
    let mut gauges = Gauges::default();
    assert!(gauges.pressure(over()).is_some());
    for _ in 0..3 {
        assert!(
            gauges.pressure(Pressure::Unknown).is_none(),
            "an unknown reading must neither re-fire nor re-arm"
        );
    }
    assert!(
        gauges.pressure(over()).is_none(),
        "Unknown left the latch told"
    );
    assert!(gauges.pressure(Pressure::Under).is_none());
    assert!(
        gauges.pressure(over()).is_some(),
        "a genuine Under reading re-arms; the next Over then fires"
    );
}

#[test]
fn disk_tells_the_user_alone() {
    let mut gauges = Gauges::default();
    assert!(gauges.disk(10 * 1024, 64 * 1024).is_none());
    let Some(Warning::User(line)) = gauges.disk(2048 * 1024, 64 * 1024) else {
        panic!("a crossing tells the user");
    };
    assert!(line.contains("disk") && line.contains("2048 KiB"), "{line}");
}

/// Unconfigured, the check returns before any bookkeeping: no walk, no
/// warning, no cost.
#[test]
fn disk_warning_unconfigured_never_walks_or_warns() {
    let mut session = Agent::for_test("system").unwrap();
    assert!(session.fleet.launch.disk_warn_bytes.is_none());

    assert!(
        session
            .disk_warning()
            .expect("an identity engine never severs")
            .is_none(),
        "unconfigured: never warns, ever"
    );
    assert!(
        session.readings.gauges.disk_walked.is_none(),
        "the early return never stamps a walk"
    );
}

/// The walk is real: 64 KiB sits above a fresh session's own footprint,
/// and the file alone crosses it.
#[test]
fn disk_warning_walks_the_log_dir() {
    let mut session = Agent::for_test_with(TestTrunk {
        disk_warn_bytes: Some(64 * 1024),
        ..TestTrunk::new("system")
    })
    .expect("a trunk under a disk-warn ceiling");
    std::fs::write(
        session.log_dir().path().join("big.txt"),
        vec![0u8; 1024 * 1024],
    )
    .unwrap();
    assert!(
        session
            .disk_warning()
            .expect("an identity engine never severs")
            .is_some()
    );
}
