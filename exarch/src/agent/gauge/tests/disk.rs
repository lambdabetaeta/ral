//! The disk ceiling's walk and latch, and the window arithmetic's floor.

use super::*;
use crate::provider::Selected;
use crate::record::Voice;

const CEILING: u64 = 512 * 1024;
const MIB: usize = 1024 * 1024;

fn warned(session: &mut Agent) -> bool {
    session
        .disk_warning()
        .expect("an identity engine never severs")
        .is_some()
}

fn under_ceiling() -> Agent {
    Agent::for_test_with(TestLaunch {
        disk_warn_bytes: Some(CEILING),
        ..TestLaunch::new("system")
    })
    .expect("a trunk under a disk-warn ceiling")
}

#[test]
fn a_fall_below_the_ceiling_rearms_the_disk_warning() {
    let mut gauges = Gauges::default();
    let told = |gauges: &mut Gauges, total| gauges.disk(total, CEILING).is_some();
    assert!(told(&mut gauges, CEILING + 1));
    assert!(!told(&mut gauges, CEILING + 1), "told once per excursion");
    assert!(!told(&mut gauges, CEILING));
    assert!(told(&mut gauges, CEILING + 1), "the fall re-armed it");
}

/// A walk within the interval is skipped, even over the ceiling; one past it
/// is taken.
#[test]
fn the_disk_walk_is_throttled_to_its_interval() {
    let mut session = under_ceiling();
    assert!(
        !warned(&mut session),
        "a fresh session sits under the ceiling"
    );
    std::fs::write(session.log_dir().path().join("big.bin"), vec![0u8; 2 * MIB]).unwrap();
    assert!(
        !warned(&mut session),
        "the second call within the interval walks nothing"
    );
    session.body.readings.gauges.disk_walked = Instant::now().checked_sub(DISK_WALK_INTERVAL * 2);
    assert!(
        warned(&mut session),
        "a call past the interval walks and sees the file"
    );
}

#[test]
fn scratch_bytes_count_toward_the_disk_total() {
    let mut session = under_ceiling();
    let log_bytes = ral_core::path::tree::bytes(session.log_dir().path());
    let (scratch, _) = session
        .scratch_bytes()
        .expect("an identity engine never severs")
        .expect("a test trunk names its scratch");
    std::fs::write(Path::new(&scratch).join("big.bin"), vec![0u8; 2 * MIB]).unwrap();
    assert!(
        log_bytes < CEILING,
        "the log directory alone is under the ceiling"
    );
    assert!(warned(&mut session), "the scratch tips the total over it");
}

/// Below the floor the reserve outgrows the window.
#[test]
fn a_window_under_the_reserve_floor_is_always_due_for_eviction() {
    for window in [1, 8_192, 16_383] {
        assert_eq!(eviction_trigger(window), 0, "window {window}");
        assert!(forced_eviction_due(0, window), "window {window}");
    }
    assert_eq!(reserve_tokens(32_768), 16_384, "the floor holds up to 109k");
    assert_eq!(
        reserve_tokens(200_000),
        30_000,
        "past it, 15% of the window"
    );
}

fn with_window(session: &Agent, window: Option<u64>) {
    session
        .mailbox
        .try_transact(|txn| {
            txn.change_model(Selected {
                context_window: window,
                ..Selected::for_test("test-model")
            })
        })
        .expect("a fixture's model change records");
}

/// A window of zero is no window: pressure falls back to serialised bytes,
/// where a real window with no measure yet is `Unknown`.
#[test]
fn a_zero_window_reads_pressure_in_bytes() {
    let mut session = Agent::for_test("system").unwrap();
    with_window(&session, Some(1_000_000));
    assert!(matches!(session.pressure(), Ok(Pressure::Unknown)));
    with_window(&session, Some(0));
    assert!(matches!(session.pressure(), Ok(Pressure::Under)));
    session
        .body
        .log
        .append_user("x".repeat(400 * 1024), None, Voice::Human)
        .unwrap();
    let Ok(Pressure::Over(load)) = session.pressure() else {
        panic!("a heavy log is over the byte line");
    };
    assert_eq!((load.total, load.unit), (None, Unit::Bytes));
    assert_eq!(load.trigger, EVICT_THRESHOLD as u64);
}
