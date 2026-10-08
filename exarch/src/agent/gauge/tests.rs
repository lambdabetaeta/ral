use super::*;

fn over() -> Pressure {
    Pressure::Over {
        detail: "400 of 500 tokens".into(),
        planned: Some(vec![1, 2, 3, 4, 5, 6, 7]),
    }
}

#[test]
fn pressure_line_sits_a_full_reserve_before_the_trigger() {
    let w = 200_000;
    let trigger = eviction_trigger(w);
    assert!(!eviction_due(trigger, w) && eviction_due(trigger + 1, w));
    assert!(
        pressure_due(trigger, w),
        "eviction due implies pressure due"
    );
    assert!(!pressure_due(w / 2, w), "half a window is not pressure");
}

/// The reminder carries the reading and the cut the next boundary would
/// make; the wording is the nudge module's.
#[test]
fn pressure_hands_the_model_its_reading_and_the_planned_cut() {
    let Some(Warning::Model(Reminder::Pressure { detail, planned })) =
        Gauges::default().pressure(over())
    else {
        panic!("pressure due should remind the model");
    };
    assert_eq!(detail, "400 of 500 tokens");
    assert_eq!(planned, Some(vec![1, 2, 3, 4, 5, 6, 7]));
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
