use super::*;
use crate::agent::TestLaunch;
use crate::record::Span;

mod disk;

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
