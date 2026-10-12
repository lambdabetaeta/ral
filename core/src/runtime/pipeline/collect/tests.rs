use super::super::group::PipelineGroup;
use super::*;
use crate::types::Escape;

fn error_break(status: i32, msg: &str) -> Break {
    Error::raised(msg, status).into()
}

/// The four laws of the join: an escape outranks an error, and within a
/// rank the earlier stage wins.
#[test]
fn breaks_join_by_rank_earlier_stage_breaking_ties() {
    match stronger(error_break(7, "first"), error_break(9, "second")) {
        Break::Error(error) => assert_eq!(error.message, "first"),
        Break::Escape(_) => panic!("an error must not displace an earlier error"),
    }
    assert!(matches!(
        stronger(
            Break::Escape(Escape::Exit(1)),
            Break::Escape(Escape::Exit(2))
        ),
        Break::Escape(Escape::Exit(1))
    ));
    assert!(matches!(
        stronger(
            error_break(7, "early failure"),
            Break::Escape(Escape::Exit(3))
        ),
        Break::Escape(Escape::Exit(3))
    ));
    assert!(matches!(
        stronger(
            Break::Escape(Escape::Exit(3)),
            error_break(7, "later failure")
        ),
        Break::Escape(Escape::Exit(3))
    ));
}

#[test]
fn protocol_error_does_not_supersede_earlier_failure() {
    match stronger(
        error_break(7, "stage one boom"),
        error_break(1, "report pipe: broken"),
    ) {
        Break::Error(error) => assert_eq!(error.message, "stage one boom"),
        Break::Escape(_) => panic!("expected the first stage failure to win"),
    }
}

/// An owning group and a collector wired onto the same channel, as
/// `PipeNode::launch` builds them.
fn owning_pipeline(
    shell: &Shell,
    mooring: &Mooring,
) -> (PipelineGroup, CollectState, Sender<Event>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let group = PipelineGroup::prepare(shell, tx.clone()).expect("anchor spawns");
    let collect = CollectState::new(rx, &tx, group.group(), None, mooring, Instant::now());
    (group, collect, tx)
}

/// `drive` over two real children folds what they settled.
#[test]
fn drive_folds_two_settling_stages_to_done() {
    let mut shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let (group, mut collect, tx) = owning_pipeline(&shell, &mooring);

    for (ix, code) in [0, 1].into_iter().enumerate() {
        let slot = Slot { ix, tx: tx.clone() };
        collect.push(StageHandle::for_test(slot, spawn_exiting(code)));
    }
    drop(tx);

    collect.drive();
    match collect.fold(&mooring, &mut shell) {
        Err(Break::Error(error)) => assert_ne!(error.code(), 0),
        other => panic!("expected the final stage's exit to fold in, got {other:?}"),
    }
    drop(group);
}

/// An external that exits with `code`: the host's shell, asked for
/// nothing but an exit status.
///
/// `/bin/sh` on Unix rather than `true`/`false`, which are not in `/bin`
/// on macOS; `cmd.exe` on Windows, where neither `/bin` nor those two
/// exist at all.  What the test needs is a real child that settles with
/// the code it was given, and both spell that.
#[cfg(unix)]
const EXIT_SHELL: (&str, &str) = ("/bin/sh", "-c");
#[cfg(windows)]
const EXIT_SHELL: (&str, &str) = ("cmd.exe", "/c");

/// See [`EXIT_SHELL`].
fn spawn_exiting(code: u8) -> crate::process::ChildHandle {
    let (shell, flag) = EXIT_SHELL;
    let child = std::process::Command::new(shell)
        .args([flag, &format!("exit {code}")])
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {shell}: {e}"));
    crate::process::ChildHandle::from_std(child)
}

/// A stage in the group's pgid over a background `sleep`, whose pid comes
/// back on the stage's stdout.  `wait_on_child` says whether the stage
/// outlives the grandchild or orphans it into the group.
#[cfg(unix)]
fn spawn_stage_with_grandchild(
    group: &PipelineGroup,
    wait_on_child: bool,
) -> (crate::process::ChildHandle, i32) {
    use std::io::Read;
    let script = if wait_on_child {
        "sleep 30 & echo $!; wait"
    } else {
        "sleep 30 & echo $!"
    };
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", script]);
    cmd.stdout(std::process::Stdio::piped());
    let (mut child, _pgid) = crate::process::spawn_with_pgid(
        &mut cmd,
        crate::process::PgidPolicy::Join(group.leader_pgid()),
    )
    .expect("spawn /bin/sh under the group's pgid");
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut pid_line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stdout
            .read_exact(&mut byte)
            .expect("read the grandchild pid");
        if byte[0] == b'\n' {
            break;
        }
        pid_line.push(byte[0]);
    }
    let pid: i32 = String::from_utf8(pid_line)
        .expect("ascii pid")
        .trim()
        .parse()
        .expect("a pid");
    (crate::process::ChildHandle::from_std(child), pid)
}

#[cfg(unix)]
fn assert_dead_within_2s(pid: i32) {
    let dead = crate::test_helper::eventually(std::time::Duration::from_secs(2), || {
        (unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
        .then_some(())
    });
    if dead.is_none() {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        panic!("grandchild pid {pid} outlived the collector's forced end");
    }
}

/// A grandchild no per-pid kill reaches dies of the group kill a forced
/// end fires.
#[cfg(unix)]
#[test]
fn a_collector_dropped_short_of_observation_kills_the_group() {
    let shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let (group, mut collect, tx) = owning_pipeline(&shell, &mooring);
    let (child, pid) = spawn_stage_with_grandchild(&group, true);

    collect.push(StageHandle::for_test(Slot { ix: 0, tx }, child));
    drop(collect);

    assert_dead_within_2s(pid);
    drop(group);
}

/// The ordinary end kills nothing: a member that outlives its stage — a
/// descendant the stage forked — outlives the pipeline too.
#[cfg(unix)]
#[test]
fn a_collector_that_observed_every_stage_kills_nothing() {
    let shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let (group, mut collect, tx) = owning_pipeline(&shell, &mooring);
    let (child, pid) = spawn_stage_with_grandchild(&group, false);

    collect.push(StageHandle::for_test(Slot { ix: 0, tx }, child));
    collect.drive();
    drop(collect);

    assert!(
        unsafe { libc::kill(pid, 0) } == 0,
        "the orphaned grandchild must survive the collector's ordinary end"
    );
    unsafe { libc::kill(pid, libc::SIGKILL) };
    drop(group);
}

/// A joining collector has no pgid to kill, so `cancel_all` must reach its
/// external's pid itself rather than hang in the final drain.
#[cfg(unix)]
#[test]
fn a_joining_collector_cancel_all_kills_its_externals_and_returns() {
    let mut shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let owner =
        PipelineGroup::prepare(&shell, std::sync::mpsc::channel().0).expect("anchor spawns");
    let pgid = owner.leader_pgid();
    let joining = PipelineGroup::joining(owner.membership(&crate::process::CancelScope::root()));

    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", "sleep 30"]);
    let (child, _pgid) =
        crate::process::spawn_with_pgid(&mut cmd, crate::process::PgidPolicy::Join(pgid))
            .expect("spawn /bin/sh under the owner's pgid");

    let (tx, rx) = std::sync::mpsc::channel();
    let mut collect = CollectState::new(rx, &tx, joining.group(), None, &mooring, Instant::now());
    collect.push(StageHandle::for_test(
        Slot {
            ix: 0,
            tx: tx.clone(),
        },
        crate::process::ChildHandle::from_std(child),
    ));
    drop(tx);

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            collect.cancel_all(CancelCause::TimedOut, false);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("cancel_all must not hang on a joining group's own external");
    });

    assert!(
        matches!(collect.fold(&mooring, &mut shell), Err(Break::Error(_))),
        "a killed stage must fold in an error, not a quiet ok"
    );
    drop(owner);
}

// ── step: the transition table ─────────────────────────────────────────
//
// Every case is a sequence of `Event`s fed to `step` and an assertion on
// the `Effect` it returns: no sleeps, no retry loops.

fn state_with(n: usize) -> CollectState {
    let (mut state, tx) = CollectState::for_step_test();
    for ix in 0..n {
        state.push(StageHandle::fake_external_for_step_test(Slot {
            ix,
            tx: tx.clone(),
        }));
    }
    state
}

fn threads_with(n: usize) -> CollectState {
    let (mut state, tx) = CollectState::for_step_test();
    for ix in 0..n {
        state.push(StageHandle::fake_thread_for_step_test(Slot {
            ix,
            tx: tx.clone(),
        }));
    }
    state
}

fn sent_to(state: &CollectState, ix: usize) -> Option<CancelCause> {
    state.stages[ix].as_ref().expect("a live stage").sent
}

/// A settled reader arms the edge its still-running writer holds — and an
/// already-finished writer has none left to arm, exactly the honesty
/// `!{ echo a; exit 3 } | head -1` needs.
#[test]
fn settling_a_stage_arms_its_still_running_writers_edge_but_spares_a_finished_one() {
    let mut state = state_with(4);
    // Stage 0 settles first — nothing upstream of it to arm.
    assert_eq!(
        step(&mut state, Event::Ended(0, WaitOutcome::Exited(0))),
        None
    );
    assert!(
        state.live(),
        "one of four stages settling must not end the pipeline"
    );
    // Stage 1 settles: its writer (0) already settled, so it keeps its
    // outcome and nothing is armed against it.
    let effect = step(&mut state, Event::Ended(1, WaitOutcome::Exited(0)));
    assert_eq!(
        effect, None,
        "a writer that already settled must keep its outcome, not be armed against"
    );
    // Stage 3 settles while its writer (2) is still running: that edge is
    // armed — and stage 2 has not settled, so the pipeline is not done.
    let effect = step(&mut state, Event::Ended(3, WaitOutcome::Exited(0)));
    assert_eq!(effect, Some(Effect::ArmEdge(2)));
    assert!(
        state.live(),
        "an unobserved writer means the pipeline is not done"
    );
    // Stage 2 now settles too: every stage is observed.
    let _ = step(&mut state, Event::Ended(2, WaitOutcome::Exited(0)));
    assert!(
        !state.live(),
        "the last stage settling must end the pipeline"
    );
}

/// The sentinel's news is what cuts a stage: `Wrote` emits the one kill,
/// which records the cause against it.
#[test]
fn a_write_to_a_dead_edge_kills_its_writer() {
    let mut state = state_with(2);
    assert_eq!(
        step(&mut state, Event::Wrote(0)),
        Some(Effect::KillStage(0))
    );
    state.run(Effect::KillStage(0));
    assert_eq!(sent_to(&state, 0), Some(CancelCause::ReaderGone));
}

/// A stage that ended on its own account before the sentinel was heard
/// keeps its outcome: the news is dropped.
#[test]
fn a_write_heard_after_its_writer_was_filed_is_ignored() {
    let mut state = state_with(2);
    let _ = step(&mut state, Event::Ended(0, WaitOutcome::Exited(3)));
    assert_eq!(
        step(&mut state, Event::Wrote(0)),
        None,
        "a filed stage must not be cut"
    );
}

/// A cancel always tears down, whatever the cause or its source, and
/// teardown must deliver it, nothing else having.
#[test]
fn a_cancelled_event_cancels_all() {
    let mut state = state_with(1);
    assert_eq!(
        step(&mut state, Event::Cancelled(CancelCause::TimedOut)),
        Some(Effect::CancelAll {
            cause: CancelCause::TimedOut,
            delivered: false
        })
    );
}

/// A key the anchor heard reached every member of the group by the
/// kernel's own hand, so teardown must not send a second copy of it; a
/// signal no terminal sends is inert.
#[cfg(unix)]
#[test]
fn a_heard_key_tears_down_without_resending_it() {
    let frame = crate::process::DurableRoot::new().worker();
    let mut state = state_with(1);
    state.loan = Some(TerminalLoan::for_test(&frame));
    let heard = |n| Event::Heard(crate::process::Signal::new(n));
    assert_eq!(step(&mut state, heard(libc::SIGTERM)), None);
    assert_eq!(
        step(&mut state, heard(libc::SIGINT)),
        Some(Effect::CancelAll {
            cause: CancelCause::Interrupted,
            delivered: true
        })
    );
}

/// With no terminal lent, nothing the anchor hears is a key.
#[cfg(unix)]
#[test]
fn with_no_loan_a_heard_signal_is_inert() {
    let mut state = state_with(1);
    assert_eq!(
        step(
            &mut state,
            Event::Heard(crate::process::Signal::new(libc::SIGINT))
        ),
        None
    );
}

/// A stage dead of SIGINT nobody sent is the key's cancellation only if
/// the key was pressed on the lent terminal, and its own signal otherwise.
#[cfg(unix)]
#[test]
fn a_stage_dead_of_the_key_is_cancelled_only_if_it_was_pressed() {
    let shell = crate::test_helper::core_shell();
    let end = || StageEnd::External {
        name: "sleep".to_string(),
        args: Vec::new(),
        outcome: WaitOutcome::Signaled(crate::process::Signal::new(libc::SIGINT)),
        jail: None,
        pumps: command::Pumps::default(),
        sent: None,
    };
    let settled = |pressed| end().settle(&shell, Instant::now(), pressed).settled;
    match settled(Some(CancelCause::Interrupted)) {
        Err(Break::Error(e)) => assert_eq!(e.cancelled_by(), Some(CancelCause::Interrupted)),
        other => panic!("expected the key's cancellation, got {other:?}"),
    }
    match settled(None) {
        Err(Break::Error(e)) => assert!(
            matches!(
                e.status,
                crate::types::Status::Process(crate::process::CommandFailure::Signal(s))
                    if s == crate::process::Signal::new(libc::SIGINT)
            ),
            "{e:?}"
        ),
        other => panic!("expected the stage's own signal death, got {other:?}"),
    }
}

/// A stage this collector tore down reports the cause it sent, not the
/// signal that carried it.
#[cfg(unix)]
#[test]
fn a_torn_down_externals_death_names_the_cause_it_was_sent() {
    let mut shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let mut state = state_with(1);
    state.stages[0]
        .as_mut()
        .expect("a live stage")
        .cancel(CancelCause::Cancelled);
    let _ = step(
        &mut state,
        Event::Ended(
            0,
            WaitOutcome::Signaled(crate::process::Signal::new(libc::SIGTERM)),
        ),
    );

    match state.fold(&mooring, &mut shell) {
        Err(Break::Error(e)) => assert_eq!(
            e.cancelled_by(),
            Some(CancelCause::Cancelled),
            "expected the cause, not the signal: {e:?}"
        ),
        other => panic!("expected the teardown death to fold in as an error, got {other:?}"),
    }
}

/// A `try` inside a stage body its reader cut sees why: the record reads
/// `` `cancelled `reader-gone `` at 141, which the handler re-raises as
/// its own failure for the run to report.
#[cfg(unix)]
#[test]
fn a_try_in_a_stage_its_reader_cut_sees_reader_gone() {
    let src = "let spew = { |n| to-line y\n spew $n }\n\
               !{ try { spew 0 } { |err| fail [status: $err[status], message: !{str $err[reason]}] } } | head -n 1";
    let mut shell = crate::test_helper::core_shell();
    let report = shell.run(crate::protocol::Run::captured(src, "<test>"));
    match report {
        crate::run::RunReport::Ran {
            ending: crate::run::Ending::Raised { error, .. },
            ..
        } => {
            assert_eq!(error.code(), 141, "{error:?}");
            assert!(
                error.message.contains("`cancelled `reader-gone"),
                "{error:?}"
            );
        }
        _ => panic!("the handler's re-raise must end the run"),
    }
}

/// `Ended(ix, Signaled(KILL))` folds as the collector's own forgiven kill
/// exactly when the stage was sent one, and as an ordinary failure when
/// nothing here ever did.
#[cfg(unix)]
#[test]
fn a_stage_kill_is_forgiven_by_sent_and_kept_without_it() {
    let mut shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let kill = WaitOutcome::Signaled(crate::process::Signal::new(libc::SIGKILL));

    let mut forgiven = state_with(1);
    forgiven.stages[0]
        .as_mut()
        .expect("a live stage")
        .cancel(CancelCause::ReaderGone);
    let _ = step(&mut forgiven, Event::Ended(0, kill));
    assert!(
        forgiven.fold(&mooring, &mut shell).is_ok(),
        "a reader-gone kill must be forgiven, not folded in as a failure"
    );

    let mut kept = state_with(1);
    let _ = step(&mut kept, Event::Ended(0, kill));
    assert!(
        matches!(kept.fold(&mooring, &mut shell), Err(Break::Error(_))),
        "the very same death, unsent, must be kept as a real failure"
    );
}

/// The reader's `Returned` reaches the collector before the writer's own
/// honest exit does, so its edge is armed — but the writer never wrote
/// again, and its break is its own `exit 3`, which `fold` must keep.
#[test]
fn a_thread_writers_honest_exit_survives_its_readers_earlier_end() {
    let mut shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let mut state = threads_with(2);

    assert_eq!(
        step(&mut state, Event::Returned(1, StageObservation::ok())),
        Some(Effect::ArmEdge(0)),
        "the reader settling first must arm its writer's edge"
    );

    let _ = step(
        &mut state,
        Event::Returned(0, StageObservation::failure(Error::raised("boom", 3))),
    );

    match state.fold(&mooring, &mut shell) {
        Err(Break::Error(e)) => assert_eq!(e.code(), 3),
        other => panic!("expected the writer's own exit 3 to survive, got {other:?}"),
    }
}

/// A `ReaderGone` break — as a dead edge's sink or a `ReaderGone` scope
/// cancel mints it — is forgiven on its own evidence, nothing having been
/// sent to the stage.
#[test]
fn a_thread_writer_the_cancel_ended_is_forgiven() {
    let mut shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let mut state = threads_with(1);
    assert_eq!(sent_to(&state, 0), None, "nothing was sent to this stage");

    let _ = step(
        &mut state,
        Event::Returned(
            0,
            StageObservation::failure(Error::cancelled(CancelCause::ReaderGone)),
        ),
    );

    let folded = state.fold(&mooring, &mut shell);
    assert!(
        folded.is_ok(),
        "a writer the reader-gone cancel actually ended must be forgiven: {folded:?}"
    );
}

// ── SettleOnDrop: the dead-man's switch ─────────────────────────────────

/// Dropped mid-unwind, the guard settles its index itself: the message
/// names the panic, not a silent disconnect.
#[test]
fn a_guard_dropped_while_panicking_settles_its_index() {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _guard = SettleOnDrop::new(Slot { ix: 0, tx });
        panic!("boom");
    });
    let event = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("the guard settles on unwind");
    match event {
        Event::Returned(0, obs) => match obs.settled {
            Err(Break::Error(err)) => assert!(
                err.message.contains("panicked"),
                "expected the panic in the message: {}",
                err.message
            ),
            other => panic!("expected an Error break, got {other:?}"),
        },
        _ => panic!("expected Returned(0, _)"),
    }
    let _ = handle.join();
}

/// A guard that already sent must not send again on drop: exactly one
/// event reaches the channel.
#[test]
fn a_guard_that_sent_does_not_settle_twice() {
    let (tx, rx) = std::sync::mpsc::channel();
    let guard = SettleOnDrop::new(Slot { ix: 0, tx });
    guard.send(StageObservation::ok());
    assert!(matches!(rx.recv(), Ok(Event::Returned(0, _))));
    // The send consumed the guard's sender, so the channel is now closed
    // rather than merely empty: no second event can ever arrive.
    assert!(matches!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    ));
}

/// A guard dropped with no panic in flight and no send made — a stage
/// thread that returned without filing its own `Returned` — reports a
/// silent end, distinct in message from a panic.
#[test]
fn a_guard_dropped_without_a_panic_reports_a_silent_end() {
    let (tx, rx) = std::sync::mpsc::channel();
    drop(SettleOnDrop::new(Slot { ix: 0, tx }));
    match rx.recv().expect("the guard settles on drop") {
        Event::Returned(0, obs) => match obs.settled {
            Err(Break::Error(err)) => assert!(
                err.message.contains("without reporting"),
                "expected the silent-end message: {}",
                err.message
            ),
            other => panic!("expected an Error break, got {other:?}"),
        },
        _ => panic!("expected Returned(0, _)"),
    }
}

/// `drive` over a producer that unwinds still returns rather than blocking
/// in `recv` forever, and the panic folds in as an `Error`.
#[test]
fn drive_settles_a_stage_whose_producer_unwound() {
    let mut shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let (group, mut collect, tx) = owning_pipeline(&shell, &mooring);

    let slot = Slot { ix: 0, tx };
    collect.push(StageHandle::fake_thread_for_step_test(slot.clone()));

    let handle = std::thread::spawn(move || {
        let _guard = SettleOnDrop::new(slot);
        panic!("boom");
    });

    collect.drive();
    let _ = handle.join();

    match collect.fold(&mooring, &mut shell) {
        Err(Break::Error(_)) => {}
        other => panic!("expected the panic to fold in as an Error, got {other:?}"),
    }
    drop(group);
}
