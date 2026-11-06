use super::*;
use ral_core::data::FOValue;
use ral_core::data::Finite;
use ral_core::data::datum::Datum as _;
use ral_core::fact::Failure;
use ral_core::process::CancelCause;
use ral_core::protocol::probe::WorkerState;
use ral_core::shell::runtime::{Done, Owner};
use ral_core::shell::runtime::{ErrorRecord, Reason};

fn born(ids: &[u64]) -> HashSet<u64> {
    ids.iter().copied().collect()
}

fn worker_row_state(id: u64, title: &str, state: WorkerState) -> WorkerRow {
    let is_ready = matches!(state, WorkerState::Ready(_));
    WorkerRow {
        id,
        title: title.to_string(),
        owner: Owner::Session,
        state,
        started: 0,
        elapsed: Finite::from(0),
        settled: is_ready.then_some(0),
        retention_left: is_ready.then_some(0),
    }
}

fn worker_row(id: u64, title: &str, running: bool) -> WorkerRow {
    worker_row_state(
        id,
        title,
        if running {
            WorkerState::Running
        } else {
            WorkerState::Ready(Done::Ok(()))
        },
    )
}

const AUDIT: &str = "audit: this call had already replied; that work stands; do not repeat it.\n";

fn settled_ending() -> Ending {
    Ending::Ok(FOValue::Unit)
}

fn failure(message: &str) -> Ending {
    Ending::Err(ErrorRecord::new(
        Reason::Raised(message.into()),
        "<test>".into(),
        ral_core::test_helper::site(),
    ))
}

fn ran(ending: Ending, rendered: &str) -> Ran {
    Ran {
        ending,
        rendered: rendered.into(),
        single_command: true,
        captured: None,
        trail: Vec::new(),
    }
}

#[test]
fn a_returning_call_says_nothing() {
    let (out, exit) = render(&ran(settled_ending(), ""), &HashSet::new(), None, &[], 30);
    assert!(out.is_empty(), "a settled ending composes nothing: {out:?}");
    assert_eq!(exit, 0);
}

/// The status a walled run carries: its deadline cancellation's own.
#[test]
fn wall_composes_rendering_remedy_audit_and_orphan_in_order() {
    let ending = Ending::Stopped(CancelCause::TimedOut);
    let births = born(&[1]);
    let workers = vec![worker_row(1, "sleep 20", true)];
    let (out, exit) = render(
        &ran(ending, "error: sleep 30\n"),
        &births,
        Some(AUDIT),
        &workers,
        5,
    );

    assert_eq!(exit, 124);
    let rendered_at = out.find("error: sleep 30").expect("engine rendering");
    let remedy_at = out.find("recovery:").expect("timeout remedy");
    let audit_at = out.find("audit:").expect("audit sentence");
    let orphan_at = out.find("sleep 20").expect("orphan sentence");
    assert!(
        rendered_at < remedy_at && remedy_at < audit_at && audit_at < orphan_at,
        "composition order must be rendering, remedy, audit, orphan: {out:?}"
    );
}

/// Only a deadline's stop earns the timeout tip; a reader-gone stop says nothing.
#[test]
fn only_a_timed_out_stop_carries_the_tip() {
    let interrupted = Ending::Stopped(CancelCause::Interrupted);
    let (out, exit) = render(
        &ran(interrupted, "error: interrupted\n"),
        &HashSet::new(),
        None,
        &[],
        5,
    );
    assert_eq!((out.as_str(), exit), ("error: interrupted\n", 130));

    let gone = Ending::Stopped(CancelCause::ReaderGone);
    let (out, exit) = render(&ran(gone, ""), &HashSet::new(), None, &[], 5);
    assert_eq!((out.as_str(), exit), ("", 141));
}

#[test]
fn raise_without_command_exit_carries_no_remedy() {
    let ending = failure("boom");
    let (out, exit) = render(&ran(ending, "error: boom\n"), &HashSet::new(), None, &[], 5);
    assert_eq!(exit, 1);
    assert!(out.contains("error: boom"));
    assert!(
        !out.contains("recovery:"),
        "a raise is not a command exit: {out:?}"
    );
}

#[test]
fn exit_ending_widens_to_name_a_live_orphan() {
    let ending = Ending::Exited(1);
    let births = born(&[9]);
    let workers = vec![worker_row(9, "spawn body", true)];
    let (out, exit) = render(&ran(ending, ""), &births, None, &workers, 5);
    assert_eq!(exit, 1);
    assert!(
        out.contains("#9 (spawn body)"),
        "a non-zero exit names a live orphan too, not just the wall: {out:?}"
    );
}

#[test]
fn a_consumed_worker_is_nobodys_orphan() {
    let ending = Ending::Exited(1);
    let births = born(&[9, 10]);
    let (out, _) = render(&ran(ending.clone(), ""), &births, None, &[], 5);
    assert!(
        !out.contains("spawn body") && !out.contains("claimed"),
        "absent from the probe means already claimed: {out:?}"
    );

    let stopping = [worker_row_state(9, "stopping body", WorkerState::Stopping)];
    let (out, _) = render(&ran(ending, ""), &births, None, &stopping, 5);
    assert!(
        out.contains("#9 (stopping body)"),
        "a stopping worker still outlived the call and is named: {out:?}"
    );
    assert!(
        !out.contains("!{worker"),
        "orphan guidance must not advertise ID-based handle recovery: {out:?}"
    );
}

/// A failed data projection shares the one handle-fate sentence.
#[test]
fn a_non_data_result_explains_which_handles_landed() {
    let ending = failure("the result is a handle, and a run can return only data");
    let births = born(&[4]);
    let workers = vec![worker_row(4, "block at <test>, line 1", true)];
    let (out, exit) = render(
        &ran(
            ending,
            "error: the result is a handle, and a run can return only data\n",
        ),
        &births,
        None,
        &workers,
        5,
    );
    assert_eq!(exit, 1);
    assert!(out.starts_with("error: the result is a handle"), "{out:?}");
    assert!(
        out.contains("or that the call only returned, never landed"),
        "{out:?}"
    );
    assert!(
        out.contains("an ID cannot recover a handle") && !out.contains("!{worker"),
        "{out:?}"
    );
    assert!(
        out.contains("A handle bound by a step that completed is still bound"),
        "{out:?}"
    );
}

#[test]
fn overflow_past_named_is_counted_not_dropped() {
    let births: HashSet<u64> = (0..NAMED as u64 + 2).collect();
    let workers: Vec<WorkerRow> = (0..NAMED as u64 + 2)
        .map(|id| worker_row(id, "job", true))
        .collect();
    let (out, _) = render(&ran(Ending::Exited(1), ""), &births, None, &workers, 5);
    assert!(out.contains("2 more not named here"), "{out:?}");
}

/// The full ending matrix: audit and orphan draw only on an ending whose
/// transcript does not otherwise show what landed, and only when there is
/// something to say.
#[test]
fn ending_matrix_gates_audit_and_orphan_on_unshown_effects() {
    let births = born(&[3]);
    let live = vec![worker_row(3, "job", true)];
    let committed = Some(AUDIT);
    let refused = None;

    let endings: [(&str, Ending, u8); 6] = [
        ("ok", settled_ending(), 0),
        ("raise", failure("boom"), 1),
        ("wall", Ending::Stopped(CancelCause::TimedOut), 124),
        ("non-data", failure("the result is a handle"), 1),
        ("exit", Ending::Exited(3), 3),
        (
            "panic",
            Ending::Panic("boom".into()),
            ral_core::fact::PANIC_CODE,
        ),
    ];

    for (name, ending, want_exit) in &endings {
        let effects_unshown = !matches!(ending, Ending::Ok(_));
        for (births_label, births) in [("present", births.clone()), ("absent", HashSet::new())] {
            for (acts_label, audit) in [("committed", committed), ("refused", refused)] {
                let (out, exit) = render(
                    &ran(ending.clone(), "engine rendering\n"),
                    &births,
                    audit,
                    &live,
                    5,
                );
                assert_eq!(exit, *want_exit, "{name}/{births_label}/{acts_label}");
                assert_eq!(
                    out.contains("audit:"),
                    effects_unshown && acts_label == "committed",
                    "{name}/{births_label}/{acts_label} audit mismatch: {out:?}"
                );
                assert_eq!(
                    out.contains("#3 (job)"),
                    effects_unshown && births_label == "present",
                    "{name}/{births_label}/{acts_label} orphan mismatch: {out:?}"
                );
            }
        }
    }
}

#[test]
fn a_rejection_keeps_its_diagnostic_and_ends_with_the_tip() {
    let out = rejection("error[T0010]: Integer doesn't match Bool\n");
    assert!(
        out.starts_with("error[T0010]: Integer doesn't match Bool\n"),
        "{out:?}"
    );
    assert!(out.ends_with(REJECTION_TIP), "{out:?}");
    assert!(out.contains("no command ran and no `let` bound"), "{out:?}");
}

#[test]
fn an_oversize_rejection_is_clipped_but_its_tip_survives_whole() {
    let out = rejection(&"x".repeat(OPAQUE_CAP * 4));
    assert!(out.len() < OPAQUE_CAP * 2, "the diagnostic was not clipped");
    assert!(out.ends_with(REJECTION_TIP), "{out:?}");
}

#[test]
fn a_fault_keeps_its_one_liner_and_ends_with_the_tip() {
    let out = fault("error: run panicked: boom\n", None);
    assert!(out.starts_with("error: run panicked: boom\n"), "{out:?}");
    assert!(out.ends_with(FAULT_TIP), "{out:?}");
    assert!(
        out.contains("`let` bindings may or may not stand"),
        "{out:?}"
    );
}

#[test]
fn a_fault_puts_the_audit_after_the_tip() {
    let audit = "\nAudit: 1 file written.\n";
    let out = fault("error: run panicked: boom\n", Some(audit));
    assert!(out.ends_with(&format!("{FAULT_TIP}{audit}")), "{out:?}");
}

#[test]
fn a_panicked_run_reads_with_the_panic_tip() {
    let ending = Ending::Panic("boom".into());
    let births = born(&[9]);
    let workers = [worker_row(9, "job", true)];
    let result = tool_result(
        Ran {
            single_command: false,
            ..ran(ending, "run panicked: boom\n")
        },
        &births,
        Some(AUDIT),
        &workers,
        5,
    );
    assert_eq!(result.exit, ral_core::fact::PANIC_CODE);
    assert!(result.value.is_none());
    let out = String::from_utf8(result.stderr).unwrap();
    assert!(
        out.starts_with(&format!("run panicked: boom\n{PANIC_TIP}{AUDIT}")),
        "{out:?}"
    );
    assert!(out.contains("#9 (job)"), "{out:?}");
    assert!(
        out.contains("newly bound handles were rolled back"),
        "{out:?}"
    );
    assert!(
        out.contains("even those bound by completed steps"),
        "{out:?}"
    );
    assert!(out.contains("an ID cannot recover a handle"), "{out:?}");
    for contradiction in ["is still bound", "are still bound", "`await $h` reaches it"] {
        assert!(!out.contains(contradiction), "{out:?}");
    }
}

#[test]
fn only_an_external_failure_carries_the_exit_tip() {
    let record = FOValue::Map {
        entries: vec![
            ("frame".into(), "tool".to_owned().encode()),
            (
                "reason".into(),
                Reason::process("tool", Failure::Exited(3)).encode(),
            ),
            ("site".into(), ral_core::test_helper::site().encode()),
        ],
    };
    let ending = Ending::Err(ErrorRecord::decode(&record).unwrap());
    for single_command in [true, false] {
        let ran = Ran {
            single_command,
            ..ran(ending.clone(), "tool: exited with status 3\n")
        };
        let (out, exit) = render(&ran, &HashSet::new(), None, &[], 5);
        assert_eq!(exit, 3);
        assert_eq!(
            out,
            format!("tool: exited with status 3\n{}", exit_tip(single_command))
        );
    }
}
