// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

use super::super::tests::with_scoped_state_dir;
use super::super::{
    GrantKind,
    stamp::{record_stamp, stamp_key, stamp_recorded},
};
use super::*;
#[test]
fn ledger_roundtrip() {
    let l = Ledger {
        run_id: "pid-42-deadbeef".into(),
        pid: 42,
        image_name: "ral.exe".into(),
        started_at_filetime: 132_000_000_000_000_000,
        profiles: vec!["ral.sandbox.s42.p0".into()],
    };
    let bytes = serde_json::to_vec(&l).unwrap();
    let parsed: Ledger = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed.run_id, l.run_id);
    assert_eq!(parsed.pid, l.pid);
    assert_eq!(parsed.started_at_filetime, l.started_at_filetime);
    assert_eq!(parsed.profiles, vec!["ral.sandbox.s42.p0".to_string()]);
}

#[test]
fn run_id_format() {
    let id = generate_run_id();
    assert!(id.starts_with("pid-"));
    let parts: Vec<&str> = id.split('-').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[2].len(), 16);
    assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn process_creation_time_is_after_unix_epoch_and_not_in_the_future() {
    let t = process_creation_filetime().expect("GetProcessTimes should succeed");
    const UNIX_EPOCH_AS_FILETIME: u64 = 11_644_473_600 * 10_000_000;
    assert!(
        t > UNIX_EPOCH_AS_FILETIME,
        "process_creation_filetime ({t}) should be > Unix epoch ({UNIX_EPOCH_AS_FILETIME})"
    );
    // "Now" in FILETIME units by arithmetic, so the bound check pulls in
    // no extra Win32 feature.
    let now_ft = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |d| {
            UNIX_EPOCH_AS_FILETIME + u64::try_from(d.as_nanos()).unwrap_or(u64::MAX) / 100
        });
    const TEN_SECONDS_TICKS: u64 = 10 * 10_000_000;
    assert!(
        t <= now_ft.saturating_add(TEN_SECONDS_TICKS),
        "process_creation_filetime ({t}) must not be after now ({now_ft})"
    );
}

#[test]
fn recovery_ignores_active_pid() {
    let pid = std::process::id();
    let img = current_image_basename();
    let start = process_creation_filetime().unwrap();
    assert!(process_alive_with_image(pid, &img, start));
    assert!(!process_alive_with_image(pid, &img, 0));
}

#[test]
fn recovery_detects_dead_pid() {
    assert!(!process_alive_with_image(0, OsStr::new("anything.exe"), 0));
    assert!(!process_alive_with_image(
        0x7FFF_FFFE,
        OsStr::new("ral.exe"),
        0
    ));
}

#[test]
fn recovery_detects_pid_reuse_via_creation_time() {
    let pid = std::process::id();
    let img = current_image_basename();
    const Y2K_FILETIME: u64 = 125_911_584_000_000_000;
    assert!(!process_alive_with_image(pid, &img, Y2K_FILETIME));
}

#[test]
fn recovery_detects_pid_reuse_off_by_one_tick() {
    let pid = std::process::id();
    let img = current_image_basename();
    let real = process_creation_filetime().unwrap();
    assert!(!process_alive_with_image(pid, &img, real.saturating_add(1)));
    assert!(!process_alive_with_image(pid, &img, real.saturating_sub(1)));
}

#[test]
fn state_dir_honors_xdg_override() {
    with_scoped_state_dir(|| {
        let held = ledger_dir().unwrap();
        let dir = held.path();
        let env = std::env::var_os("XDG_STATE_HOME").unwrap();
        assert!(dir.starts_with(PathBuf::from(env)));
        assert!(dir.ends_with("ral/sandbox-dacl") || dir.ends_with(r"ral\sandbox-dacl"));
    });
}

#[test]
fn recovery_leaves_the_stamp_store_unparsed_and_unquarantined() {
    with_scoped_state_dir(|| {
        let dir = ledger_dir().unwrap();
        let key = stamp_key(r"C:\somewhere", GrantKind::ReadWrite);
        record_stamp(&key).unwrap();
        let stamp_path = dir.path().join(STAMP_STORE_FILE_NAME);
        assert!(matches!(stamp_path.try_exists(), Ok(true)));

        let report = recover_orphaned_state().unwrap();

        assert!(
            matches!(stamp_path.try_exists(), Ok(true)),
            "the stamp store must not be renamed away as corrupt"
        );
        assert!(
            !crate::path::with_appended_suffix(&stamp_path, "corrupt")
                .try_exists()
                .unwrap_or(false),
            "the sweep must not quarantine the stamp store"
        );
        assert!(
            report.errors.is_empty(),
            "the stamp store must not surface as a parse error: {:?}",
            report.errors
        );
        assert!(
            stamp_recorded(&key),
            "the recorded stamp must survive the sweep"
        );
    });
}

#[test]
fn record_and_forget_profile_round_trip_the_ledger() {
    with_scoped_state_dir(|| {
        let mut m = DaclManager::new().unwrap();
        m.record_profile("ral.sandbox.s1.p0").unwrap();
        let persisted = read_ledger(&m.dir, &m.ledger_name).unwrap();
        assert_eq!(persisted.profiles, vec!["ral.sandbox.s1.p0".to_string()]);
        m.forget_profile("ral.sandbox.s1.p0").unwrap();
        // Nothing recorded — the ledger file is removed outright.
        assert!(m.dir.stat(&m.ledger_name).is_none());
    });
}

#[test]
fn recovery_deletes_dead_sessions_profiles() {
    with_scoped_state_dir(|| {
        // One profile genuinely registered, one ledgered but never
        // created — the crash window between record_profile and the OS
        // create. Recovery deletes the first and consumes the ledger
        // either way, so the absent one is not retried forever.
        let created = format!("ral.test.recovery.{}", std::process::id());
        let ghost = format!("ral.test.recovery.ghost.{}", std::process::id());
        let profile =
            crate::authority::sandbox::windows::appcontainer::AppContainerProfile::create_or_reuse(
                &created,
            )
            .expect("test profile creation");
        drop(profile); // frees the in-memory SID; the OS profile stays registered

        let dir = ledger_dir().unwrap();
        let synthetic = dir.path().join("pid-2147483646-profiles.json");
        let l = Ledger {
            run_id: "pid-2147483646-profiles".into(),
            pid: 0x7FFF_FFFE,
            image_name: "ral.exe".into(),
            started_at_filetime: 0,
            profiles: vec![created, ghost],
        };
        write_ledger(&dir, "pid-2147483646-profiles.json", &l).unwrap();

        let report = recover_orphaned_state().unwrap();
        assert!(
            report.profiles_deleted >= 1,
            "the registered profile must be deleted: {report:?}"
        );
        assert!(
            matches!(synthetic.try_exists(), Ok(false)),
            "the orphaned ledger must be consumed: {report:?}"
        );
    });
}
