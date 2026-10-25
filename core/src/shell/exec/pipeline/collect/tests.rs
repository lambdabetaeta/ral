use super::super::group::PipelineGroup;
use super::*;
#[cfg(unix)]
use crate::process::Membership;
use crate::process::{DurableRoot, Scope};
use crate::shell::runtime::Escape;
use crate::source::{FileId, Span};
use std::time::Duration;

fn error_break(status: i32, msg: &str) -> Break {
    Error::raised(msg, status).into()
}

fn stopped(cause: CancelCause) -> Break {
    Break::stopped(cause, None)
}

fn stopped_at(cause: CancelCause, at: u32) -> Break {
    Break::Escape(Escape::Stopped {
        cause,
        owner: None,
        at: Some(Span::new(FileId::DUMMY, at, at)),
    })
}

/// A stage's reader-gone stop, naming its own node.
fn cut(stage: &ScopeId) -> Break {
    Break::stopped(CancelCause::ReaderGone, Some(stage.clone()))
}

fn stop_of(br: &Break) -> Option<(CancelCause, Option<u32>)> {
    match br {
        Break::Escape(Escape::Stopped { cause, at, .. }) => {
            Some((*cause, at.as_ref().map(|s| s.start)))
        }
        _ => None,
    }
}

fn is_error(br: &Break, code: i32) -> bool {
    matches!(br, Break::Error(e) if e.code() == code)
}

/// The laws of the join: `Error 1 < Exit 2 < Stopped c (3 + c)`, a stage's
/// reader-gone stop under everything, and within a rank the earlier stage wins.
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
    let (interrupted, timed_out) = (CancelCause::Interrupted, CancelCause::TimedOut);
    let exit = || Break::Escape(Escape::Exit(1));
    assert_eq!(
        stop_of(&stronger(exit(), stopped(interrupted))),
        Some((interrupted, None))
    );
    assert_eq!(
        stop_of(&stronger(stopped(interrupted), exit())),
        Some((interrupted, None))
    );
    assert_eq!(
        stop_of(&stronger(stopped(interrupted), stopped(timed_out))),
        Some((timed_out, None))
    );
    assert_eq!(
        stop_of(&stronger(
            stopped_at(interrupted, 1),
            stopped_at(interrupted, 2)
        )),
        Some((interrupted, Some(1))),
        "equal stops keep the earlier"
    );
    let root = DurableRoot::default().worker();
    assert!(is_error(&stronger(cut(&root.id()), error_break(7, "x")), 7));
    assert!(is_error(&stronger(error_break(7, "x"), cut(&root.id())), 7));
    assert!(matches!(
        stronger(cut(&root.id()), Break::Escape(Escape::Exit(1))),
        Break::Escape(Escape::Exit(1))
    ));
    assert_eq!(
        stop_of(&stronger(
            stopped(CancelCause::ReaderGone),
            error_break(7, "x")
        )),
        Some((CancelCause::ReaderGone, None)),
        "the closed-stdout stop is a stop"
    );
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
/// `PipeNode::run` builds them.
fn owning_pipeline(
    shell: &Shell,
    mooring: &Mooring,
) -> (PipelineGroup, CollectState, Sender<Event>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let group = PipelineGroup::prepare(shell, tx.clone()).expect("anchor spawns");
    let collect = CollectState::new(
        rx,
        group.group(),
        None,
        shell.scope.clone(),
        mooring.cleanup_grace,
        Instant::now(),
    );
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

/// A collector dropped short of observation finishes each live external's
/// jail.  A plain directory stands in for the cgroup, keeping the finish's
/// `cgroup.kill`.
#[cfg(target_os = "linux")]
#[test]
fn a_dropped_collector_finishes_its_live_stages_jail() {
    use super::super::stage::{ExternalStage, StageKind};
    let dir = tempfile::tempdir().unwrap();
    let (mut state, tx) = CollectState::for_step_test();
    let slot = Slot { ix: 0, tx };
    let child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn /bin/sleep");
    state.push(StageHandle::new(
        StageKind::External(ExternalStage {
            watch: slot.watch(crate::process::ChildHandle::from_std(child)),
            name: "sleep".to_string(),
            args: Vec::new(),
            jail: Some(crate::process::jail::JailCgroup::at(
                dir.path().to_path_buf(),
            )),
            pumps: command::Pumps::default(),
            envelope: None,
        }),
        None,
        slot,
    ));
    state.kill_live();
    assert!(
        dir.path().join("cgroup.kill").exists(),
        "kill_live must kill the live stage's jail before any wait"
    );
    drop(state);
    assert!(
        dir.path().join("cgroup.kill").exists(),
        "the live stage's jail must be finished"
    );
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
    let joining =
        PipelineGroup::joining(owner.membership(&crate::process::DurableRoot::default().worker()));

    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", "sleep 30"]);
    let (child, _pgid) =
        crate::process::spawn_with_pgid(&mut cmd, crate::process::PgidPolicy::Join(pgid))
            .expect("spawn /bin/sh under the owner's pgid");

    let (tx, rx) = std::sync::mpsc::channel();
    let mut collect = CollectState::new(
        rx,
        joining.group(),
        None,
        Scope::clone(&DurableRoot::default().worker()),
        Duration::ZERO,
        Instant::now(),
    );
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
            collect.cancel_all(CancelCause::TimedOut);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("cancel_all must not hang on a joining group's own external");
    });

    assert!(
        matches!(
            collect.fold(&mooring, &mut shell),
            Err(Break::Escape(Escape::Stopped {
                cause: CancelCause::TimedOut,
                ..
            }))
        ),
        "a torn-down stage must fold in the teardown's stop, not a quiet ok"
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
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a fake stage's child");
        let _ = child.kill();
        let slot = Slot { ix, tx: tx.clone() };
        state.push(StageHandle::fake_external_for_step_test(slot, child));
    }
    state
}

/// `n` fake thread stages, their scopes, and the sender their slots address.
fn threads_with(n: usize) -> (CollectState, Vec<Scope>, Sender<Event>) {
    let (mut state, tx) = CollectState::for_step_test();
    let scopes = (0..n)
        .map(|ix| {
            let (handle, scope) =
                StageHandle::fake_thread_for_step_test(Slot { ix, tx: tx.clone() });
            state.push(handle);
            scope
        })
        .collect();
    (state, scopes, tx)
}

fn halted(br: Break) -> StageObservation {
    StageObservation {
        end: ThreadEnd::Settled(Err(br)),
        audit: AuditFragment::empty(),
    }
}

fn panicked(text: &str) -> StageObservation {
    StageObservation {
        end: ThreadEnd::Panicked(text.to_string()),
        audit: AuditFragment::empty(),
    }
}

/// Send each event `after` its millisecond offset from now, from a helper thread.
fn sending(tx: &Sender<Event>, events: Vec<(u64, Event)>) -> std::thread::JoinHandle<()> {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        for (ms, ev) in events {
            std::thread::sleep(
                (t0 + Duration::from_millis(ms)).saturating_duration_since(Instant::now()),
            );
            let _ = tx.send(ev);
        }
    })
}

fn returned(ix: usize, obs: StageObservation) -> Event {
    Event::Returned(ix, obs)
}

fn fold_of(state: CollectState) -> Settled<Value> {
    let mut shell = crate::test_helper::core_shell();
    state.fold(&Mooring::adrift(), &mut shell)
}

fn sent_to(state: &CollectState, ix: usize) -> Option<CancelCause> {
    state.stages[ix].as_ref().expect("a live stage").sent
}

/// A settled reader arms the edge its still-running writer holds — and an
/// already-finished writer has none left to arm, exactly the honesty
/// `!{ echo a; exit 3 } | head -1` needs.
#[test]
fn settling_a_stage_arms_its_still_running_writers_edge_but_spares_a_finished_one() {
    let abandoned = |s: &CollectState| s.abandoned().collect::<Vec<_>>();
    let mut state = state_with(4);
    // Only the final stage, which writes to no edge, has no reader.
    assert_eq!(abandoned(&state), [3]);
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
    let _ = step(&mut state, Event::Ended(1, WaitOutcome::Exited(0)));
    assert_eq!(
        abandoned(&state),
        [3],
        "a writer that already settled must keep its outcome, not be armed against"
    );
    // Stage 3 settles while its writer (2) is still running: that edge is
    // armed — and stage 2 has not settled, so the pipeline is not done.
    let _ = step(&mut state, Event::Ended(3, WaitOutcome::Exited(0)));
    assert_eq!(abandoned(&state), [2]);
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

/// A cause the scope read between slices always tears down.
#[test]
fn a_cancelled_event_cancels_all() {
    let mut state = state_with(1);
    assert_eq!(
        step(&mut state, Event::Cancelled(CancelCause::TimedOut)),
        Some(Effect::CancelAll {
            cause: CancelCause::TimedOut
        })
    );
}

/// A key the anchor heard is struck on the node and read by the scope at
/// the next slice, never turned into a teardown by the hearing itself; a
/// signal no terminal sends is inert.
#[cfg(unix)]
#[test]
fn a_heard_key_strikes_the_node_and_tears_nothing_down() {
    let frame = crate::process::DurableRoot::new().worker();
    let mut state = state_with(1);
    state.loan = Some(TerminalLoan::for_test(&frame));
    let heard = |n| Event::Heard(crate::process::Signal::new(n));
    assert_eq!(step(&mut state, heard(libc::SIGTERM)), None);
    assert_eq!(frame.cause(), None);
    assert_eq!(step(&mut state, heard(libc::SIGINT)), None);
    assert_eq!(frame.cause(), Some(CancelCause::Interrupted));
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

/// A stage dead of SIGINT nobody sent is the key's stop only if the current
/// scope carries the key, and its own signal otherwise.
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
    let group = Group::Joins(Membership::new(
        Pgid::from_raw(1).expect("1 is positive"),
        Scope::clone(&DurableRoot::default().worker()),
    ));
    let settled = |scope: &Scope| match end().settle(&shell, Instant::now(), scope, &group).end {
        ThreadEnd::Settled(s) => s,
        ThreadEnd::Panicked(t) => panic!("{t}"),
    };
    let pressed = DurableRoot::default().worker();
    pressed.cancel(CancelCause::Interrupted);
    match settled(&pressed) {
        Err(Break::Escape(Escape::Stopped { cause, .. })) => {
            assert_eq!(cause, CancelCause::Interrupted);
        }
        other => panic!("expected the key's stop, got {other:?}"),
    }
    match settled(&DurableRoot::default().worker()) {
        Err(Break::Error(e)) => assert!(
            matches!(
                e.status,
                crate::shell::runtime::Status::Process(crate::process::CommandFailure::Signal(s))
                    if s == crate::process::Signal::new(libc::SIGINT)
            ),
            "{e:?}"
        ),
        other => panic!("expected the stage's own signal death, got {other:?}"),
    }
}

/// A stage this collector tore down is consumed, and the pipeline ends
/// stopped with the cause the teardown carried, not the signal that carried it.
#[cfg(unix)]
#[test]
fn a_torn_down_externals_death_names_the_cause_it_was_sent() {
    let mut shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let mut state = state_with(1);
    state.escalate(CancelCause::Cancelled);
    let _ = step(
        &mut state,
        Event::Ended(
            0,
            WaitOutcome::Signaled(crate::process::Signal::new(libc::SIGTERM)),
        ),
    );

    match state.fold(&mooring, &mut shell) {
        Err(Break::Escape(Escape::Stopped { cause, .. })) => {
            assert_eq!(cause, CancelCause::Cancelled, "the cause, not the signal");
        }
        other => panic!("expected the teardown's stop, got {other:?}"),
    }
}

/// A `try` inside a stage body its reader cut cannot see the reader-gone
/// stop: it passes `try`, the handler never runs, and the collector
/// consumes it.
#[cfg(unix)]
#[test]
fn a_try_in_a_stage_cannot_see_its_reader_gone_stop() {
    let src = "let spew = { |n| to-line y\n spew $n }\n\
               !{ try { spew 0 } { |err| fail [status: 7, message: x] } } | /usr/bin/head -n 1";
    let mut shell = crate::test_helper::core_shell();
    let report = shell.run(crate::protocol::Run::captured(
        src,
        crate::source::SourceOrigin::named("<test>"),
    ));
    match report {
        crate::shell::run::RunReport::Ran {
            ending: crate::shell::run::Ending::Settled { .. },
            captured,
            ..
        } => {
            let out = captured.expect("a captured run returns buffers").stdout;
            assert_eq!(String::from_utf8_lossy(&out).lines().count(), 1, "{out:?}");
        }
        _ => panic!("the handler must never run, so the pipeline settles"),
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

    // An ancestor's reader-gone stop forgives nothing this collector did not send.
    let mut ancestors = state_with(1);
    let above = DurableRoot::default().worker();
    ancestors.scope = Scope::clone(&above);
    above.cancel(CancelCause::ReaderGone);
    let _ = step(&mut ancestors, Event::Ended(0, kill));
    match ancestors.fold(&mooring, &mut shell) {
        Err(Break::Error(e)) => assert!(
            matches!(
                e.status,
                crate::shell::runtime::Status::Process(crate::process::CommandFailure::Signal(s))
                    if s == crate::process::Signal::new(libc::SIGKILL)
            ),
            "{e:?}"
        ),
        other => panic!("expected `signaled 9`, got {other:?}"),
    }

    // A death by what was sent is the external's stop naming its stage.
    let mut sent = state_with(1);
    sent.stages[0]
        .as_mut()
        .expect("a live stage")
        .cancel(CancelCause::Cancelled);
    let term = WaitOutcome::Signaled(crate::process::Signal::new(libc::SIGTERM));
    let _ = step(&mut sent, Event::Ended(0, term));
    assert!(
        sent.fold(&mooring, &mut shell).is_ok(),
        "a death by the sent cause is consumed"
    );
}

/// The reader's `Returned` reaches the collector before the writer's own
/// honest exit does, so its edge is armed — but the writer never wrote
/// again, and its break is its own `exit 3`, which `fold` must keep.
#[test]
fn a_thread_writers_honest_exit_survives_its_readers_earlier_end() {
    let mut shell = crate::test_helper::core_shell();
    let mooring = Mooring::adrift();
    let (mut state, _scopes, _tx) = threads_with(2);

    let _ = step(&mut state, Event::Returned(1, StageObservation::ok()));
    assert_eq!(
        state.abandoned().collect::<Vec<_>>(),
        [0],
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

/// A reader-gone stop naming the stage is the collector's own: consumed,
/// whatever it was raised by.
#[test]
fn a_reader_gone_writer_is_forgiven_by_its_name() {
    let (mut state, scopes, _tx) = threads_with(1);
    assert_eq!(sent_to(&state, 0), None, "nothing was sent to this stage");
    let _ = step(&mut state, returned(0, halted(cut(&scopes[0].id()))));
    let folded = fold_of(state);
    assert!(folded.is_ok(), "{folded:?}");
}

/// The same stop naming another node is some collector's above, so it is
/// forwarded.
#[test]
fn a_foreign_reader_gone_is_forwarded() {
    let (mut state, _scopes, _tx) = threads_with(1);
    let _ = step(
        &mut state,
        returned(0, halted(cut(&DurableRoot::default().worker().id()))),
    );
    match fold_of(state) {
        Err(Break::Escape(Escape::Stopped { cause, .. })) => {
            assert_eq!(cause, CancelCause::ReaderGone);
        }
        other => panic!("expected the foreign stop, got {other:?}"),
    }
}

/// A key the root heard names the root, not the stage, so it is kept.
#[test]
fn an_armed_stage_that_ends_interrupted_is_kept() {
    let (mut state, _scopes, _tx) = threads_with(1);
    let root = DurableRoot::default().worker().id();
    let stop = Break::stopped(CancelCause::Interrupted, Some(root));
    let _ = step(&mut state, returned(0, halted(stop)));
    match fold_of(state) {
        Err(Break::Escape(Escape::Stopped { cause, .. })) => {
            assert_eq!(cause, CancelCause::Interrupted);
        }
        other => panic!("expected the stop, got {other:?}"),
    }
}

/// A reader-gone stage whose cleanup failed forwards the failure.
#[test]
fn a_reader_gone_stages_cleanup_halt_is_forwarded() {
    let (mut state, _scopes, _tx) = threads_with(1);
    let _ = step(
        &mut state,
        returned(0, StageObservation::failure(Error::raised("x", 7))),
    );
    assert!(matches!(fold_of(state), Err(Break::Error(e)) if e.code() == 7));
}

/// Stage 0's error and stage 1's nested reader-gone stop: the stop ranks
/// under the error.
#[test]
fn a_nested_reader_gone_stop_ranks_under_a_siblings_error() {
    let (mut state, _scopes, _tx) = threads_with(2);
    let _ = step(
        &mut state,
        returned(0, StageObservation::failure(Error::raised("x", 7))),
    );
    let _ = step(
        &mut state,
        returned(1, halted(cut(&DurableRoot::default().worker().id()))),
    );
    assert!(matches!(fold_of(state), Err(Break::Error(e)) if e.code() == 7));
}

/// An `exit` out of a stage is a top-level effect in flight: the siblings
/// are torn down, and the stops that raises name their own stages.
#[test]
fn an_exit_in_a_stage_tears_the_siblings_down() {
    let (mut state, scopes, tx) = threads_with(2);
    let exit = halted(Break::Escape(Escape::Exit(3)));
    assert_eq!(
        step(&mut state, returned(0, exit)),
        Some(Effect::CancelAll {
            cause: CancelCause::Cancelled
        })
    );
    let sibling = halted(Break::stopped(CancelCause::Cancelled, Some(scopes[1].id())));
    let helper = sending(&tx, vec![(50, returned(1, sibling))]);
    state.cancel_all(CancelCause::Cancelled);
    helper.join().expect("helper");
    assert_eq!(scopes[1].cause(), Some(CancelCause::Cancelled));
    assert!(matches!(
        fold_of(state),
        Err(Break::Escape(Escape::Exit(3)))
    ));
}

/// The closed-stdout stop out of a stage tears the siblings down too, and
/// is the verdict.
#[test]
fn a_closed_stdout_in_a_stage_tears_the_siblings_down() {
    let (mut state, scopes, tx) = threads_with(2);
    assert_eq!(
        step(
            &mut state,
            returned(1, halted(stopped(CancelCause::ReaderGone)))
        ),
        Some(Effect::CancelAll {
            cause: CancelCause::ReaderGone
        })
    );
    let helper = sending(&tx, vec![(50, returned(0, halted(cut(&scopes[0].id()))))]);
    state.cancel_all(CancelCause::ReaderGone);
    helper.join().expect("helper");
    match fold_of(state) {
        Err(Break::Escape(Escape::Stopped {
            cause, owner: None, ..
        })) => {
            assert_eq!(cause, CancelCause::ReaderGone);
        }
        other => panic!("expected the closed-stdout stop, got {other:?}"),
    }
}

/// A stage's own reader-gone stop arms its writer's edge and tears nothing
/// down.
#[test]
fn a_reader_gone_stop_naming_its_own_stage_tears_nothing_down() {
    let (mut state, scopes, _tx) = threads_with(2);
    assert_eq!(
        step(&mut state, returned(1, halted(cut(&scopes[1].id())))),
        None
    );
    assert_eq!(state.abandoned().collect::<Vec<_>>(), [0]);
}

/// A stop naming the stage is the collector's whatever its cause, and the
/// teardown's own observation is the verdict.
#[test]
fn a_stop_naming_the_stage_is_consumed_whatever_its_cause() {
    let (mut state, scopes, tx) = threads_with(2);
    let stop = |i: usize| {
        halted(Break::stopped(
            CancelCause::Terminated,
            Some(scopes[i].id()),
        ))
    };
    let helper = sending(
        &tx,
        vec![(50, returned(0, stop(0))), (50, returned(1, stop(1)))],
    );
    state.cancel_all(CancelCause::Terminated);
    helper.join().expect("helper");
    match fold_of(state) {
        Err(Break::Escape(Escape::Stopped {
            cause, owner: None, ..
        })) => {
            assert_eq!(cause, CancelCause::Terminated);
        }
        other => panic!("expected the teardown's own observation, got {other:?}"),
    }
}

/// A panic tears the group down at `Aborted` and is re-raised at the fold.
#[test]
fn a_stage_panic_tears_the_group_down_and_is_reraised_at_fold() {
    let (mut state, scopes, tx) = threads_with(2);
    assert_eq!(
        step(&mut state, returned(0, panicked("boom"))),
        Some(Effect::CancelAll {
            cause: CancelCause::Aborted
        })
    );
    let helper = sending(&tx, vec![(50, returned(1, StageObservation::ok()))]);
    state.cancel_all(CancelCause::Aborted);
    helper.join().expect("helper");
    assert_eq!(scopes[1].cause(), Some(CancelCause::Aborted));
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fold_of(state)))
        .expect_err("a panic is re-raised at the fold");
    assert_eq!(
        payload.downcast_ref::<String>().map(String::as_str),
        Some("boom")
    );
}

/// The collector is the one writer of a stage's node, for every teardown.
#[test]
fn a_cancel_all_writes_its_cause_on_every_thread_stage() {
    let (mut state, scopes, tx) = threads_with(2);
    let helper = sending(
        &tx,
        vec![
            (50, returned(0, StageObservation::ok())),
            (50, returned(1, StageObservation::ok())),
        ],
    );
    state.cancel_all(CancelCause::Interrupted);
    helper.join().expect("helper");
    for scope in &scopes {
        assert_eq!(scope.cause(), Some(CancelCause::Interrupted));
    }
}

/// A live thread stage is waited for, up to the grace, before the kill.
#[test]
fn a_cancel_all_waits_for_a_live_thread_stage_up_to_its_grace() {
    let (mut state, _scopes, tx) = threads_with(1);
    state.cleanup_grace = Duration::from_millis(200);
    let helper = sending(&tx, vec![(100, returned(0, StageObservation::ok()))]);
    let t0 = Instant::now();
    state.cancel_all(CancelCause::Interrupted);
    helper.join().expect("helper");
    assert!(
        t0.elapsed() < Duration::from_millis(400),
        "{:?}",
        t0.elapsed()
    );
}

/// A fake thread stage whose outbound edge only its hold reads: the edge, and
/// its write end.
fn push_edged_thread(
    state: &mut CollectState,
    tx: &Sender<Event>,
) -> (std::sync::Arc<crate::io::Edge>, os_pipe::PipeWriter) {
    use super::super::route::HeldEdge;
    use super::super::stage::StageKind;
    use super::super::thread::ThreadStage;
    use std::sync::Arc;
    let (r, w) = crate::process::cloexec_pipe().expect("a pipe");
    let edge = crate::io::Edge::new();
    let held = HeldEdge {
        edge: Arc::clone(&edge),
        reader: Arc::new(r),
        listening: std::cell::Cell::default(),
    };
    let slot = Slot {
        ix: state.stages.len(),
        tx: tx.clone(),
    };
    state.push(StageHandle::new(
        StageKind::Thread(ThreadStage::fake().0),
        Some(held),
        slot,
    ));
    (edge, w)
}

/// The sweep's `cut`: outside a teardown an abandoned edge is marked dead.
#[test]
fn a_sweep_marks_an_abandoned_edge_dead() {
    let (mut state, tx) = CollectState::for_step_test();
    let (edge, _w) = push_edged_thread(&mut state, &tx);
    state.sweep_edges();
    assert!(edge.is_dead());
}

/// A teardown cuts nothing at an edge but reads every one a reader has left —
/// here one never launched, as after a launch that failed part-way — so a
/// writer more than a pipe's worth ahead is not blocked.
#[test]
fn a_teardown_reads_an_abandoned_edge_and_cuts_nothing() {
    let (mut state, tx) = CollectState::for_step_test();
    let (edge, w) = push_edged_thread(&mut state, &tx);
    let (done, landed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(std::io::Write::write_all(&mut &w, &vec![0u8; 1 << 20]).is_ok());
    });
    let helper = std::thread::spawn(move || {
        let wrote = landed.recv_timeout(Duration::from_secs(2));
        let _ = tx.send(returned(0, StageObservation::ok()));
        wrote
    });
    state.cancel_all(CancelCause::Interrupted);
    assert_eq!(
        helper.join().expect("helper"),
        Ok(true),
        "the writer blocked on an edge nobody read"
    );
    assert!(!edge.is_dead(), "a teardown marked the edge dead");
}

/// A real external that ignores INT and TERM beside a fake thread stage, and
/// the instant it died, measured from the call to `cancel_all(cause)`.
#[cfg(unix)]
fn death_during_teardown(cause: CancelCause) -> Duration {
    let (mut state, tx) = CollectState::for_step_test();
    state.cleanup_grace = Duration::from_millis(200);
    let (thread, _scope) = StageHandle::fake_thread_for_step_test(Slot {
        ix: 0,
        tx: tx.clone(),
    });
    state.push(thread);
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "trap '' INT TERM; echo; exec sleep 30"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the trapper");
    // The grace signal must not land before the trap does.
    let mut ready = [0u8];
    std::io::Read::read_exact(&mut child.stdout.take().expect("piped"), &mut ready)
        .expect("the trapper is ready");
    let pid = i32::try_from(child.id()).expect("a pid");
    state.push(StageHandle::for_test(
        Slot {
            ix: 1,
            tx: tx.clone(),
        },
        crate::process::ChildHandle::from_std(child),
    ));
    let t0 = Instant::now();
    let prober = std::thread::spawn(move || {
        loop {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return t0.elapsed();
            }
            if t0.elapsed() > Duration::from_secs(5) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                return t0.elapsed();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let helper = sending(&tx, vec![(1500, returned(0, StageObservation::ok()))]);
    state.cancel_all(cause);
    helper.join().expect("helper");
    prober.join().expect("prober")
}

/// The dual measures the kill: the signal grace, then the thread wait to its
/// grace, then the kill.
#[cfg(unix)]
#[test]
fn a_cancel_all_kills_only_after_the_thread_grace() {
    let died_at = death_during_teardown(CancelCause::Interrupted);
    assert!(died_at >= Duration::from_millis(700), "{died_at:?}");
    assert!(died_at < Duration::from_millis(1200), "{died_at:?}");
}

/// No signal phase, no thread wait: a defect waits for nothing.
#[cfg(unix)]
#[test]
fn an_abort_grants_no_grace() {
    let died_at = death_during_teardown(CancelCause::Aborted);
    assert!(died_at < Duration::from_millis(100), "{died_at:?}");
}

/// A panic filed mid-teardown escalates to `Aborted`, cutting the waits.
#[test]
fn a_panic_mid_teardown_escalates_to_abort() {
    let (mut state, scopes, tx) = threads_with(2);
    state.cleanup_grace = Duration::from_secs(1);
    let helper = sending(
        &tx,
        vec![
            (100, returned(0, panicked("boom"))),
            (200, returned(1, StageObservation::ok())),
        ],
    );
    let t0 = Instant::now();
    state.cancel_all(CancelCause::Interrupted);
    helper.join().expect("helper");
    assert!(
        t0.elapsed() < Duration::from_millis(600),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(scopes[1].cause(), Some(CancelCause::Aborted));
    assert_eq!(scopes[0].cause(), Some(CancelCause::Interrupted));
}

/// A stronger cause read on the scope mid-teardown is written over the
/// survivors.
#[test]
fn a_stronger_cause_mid_teardown_is_written_on_the_stages() {
    let (mut state, scopes, tx) = threads_with(2);
    let held = DurableRoot::default().worker();
    state.scope = Scope::clone(&held);
    let helper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        held.cancel(CancelCause::Cancelled);
        std::thread::sleep(Duration::from_millis(50));
        let _ = tx.send(returned(0, StageObservation::ok()));
        let _ = tx.send(returned(1, StageObservation::ok()));
    });
    state.cancel_all(CancelCause::Interrupted);
    helper.join().expect("helper");
    for scope in &scopes {
        assert_eq!(scope.cause(), Some(CancelCause::Cancelled));
    }
}

/// A clean EOF everywhere does not launder a teardown.
#[test]
fn a_teardown_is_the_verdict_when_every_stage_survived_it() {
    let (mut state, _scopes, tx) = threads_with(2);
    let helper = sending(
        &tx,
        vec![
            (50, returned(0, StageObservation::ok())),
            (50, returned(1, StageObservation::ok())),
        ],
    );
    state.cancel_all(CancelCause::Terminated);
    helper.join().expect("helper");
    match fold_of(state) {
        Err(Break::Escape(Escape::Stopped {
            cause, owner: None, ..
        })) => {
            assert_eq!(cause, CancelCause::Terminated);
        }
        other => panic!("expected the teardown's stop, got {other:?}"),
    }
}

/// A stage's own halt outranks the teardown's cause.
#[test]
fn a_stage_halt_outranks_the_teardown_at_the_fold() {
    let (mut state, _scopes, tx) = threads_with(2);
    let helper = sending(
        &tx,
        vec![
            (
                50,
                returned(0, StageObservation::failure(Error::raised("x", 7))),
            ),
            (50, returned(1, StageObservation::ok())),
        ],
    );
    state.cancel_all(CancelCause::Terminated);
    helper.join().expect("helper");
    assert!(matches!(fold_of(state), Err(Break::Error(e)) if e.code() == 7));
}

/// One strike for every cleanup, before the joins.
#[test]
fn a_dropped_collector_writes_terminated_on_its_stages() {
    let (state, scopes, _tx) = threads_with(2);
    drop(state);
    for scope in &scopes {
        assert_eq!(scope.cause(), Some(CancelCause::Terminated));
    }
}

#[test]
fn a_cancelled_scope_is_read_between_slices() {
    let (mut state, _tx) = CollectState::for_step_test();
    let held = DurableRoot::default().worker();
    state.scope = Scope::clone(&held);
    let striker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        held.cancel(CancelCause::Interrupted);
    });
    let t0 = Instant::now();
    let ev = state.recv(None);
    striker.join().expect("striker");
    assert!(matches!(
        ev,
        Some(Event::Cancelled(CancelCause::Interrupted))
    ));
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "{:?}",
        t0.elapsed()
    );
}

#[test]
fn a_spent_deadline_returns_nothing_even_with_an_event_queued() {
    let (state, tx) = CollectState::for_step_test();
    tx.send(Event::Wrote(0)).expect("queue");
    assert!(state.recv(Some(Instant::now())).is_none());
}

#[cfg(unix)]
#[test]
fn an_anchors_death_on_its_own_ends_the_pipeline() {
    let mut state = state_with(1);
    assert_eq!(
        step(&mut state, Event::AnchorDied),
        Some(Effect::CancelAll {
            cause: CancelCause::Terminated
        })
    );
}

/// Inside a teardown the collector's own kill took the anchor with the group,
/// so an `Interrupted` teardown must not end `Terminated`.
#[cfg(unix)]
#[test]
fn an_anchors_death_inside_a_teardown_does_not_escalate() {
    let (mut state, _scopes, tx) = threads_with(2);
    let helper = sending(
        &tx,
        vec![
            (50, Event::AnchorDied),
            (100, returned(0, StageObservation::ok())),
            (100, returned(1, StageObservation::ok())),
        ],
    );
    state.cancel_all(CancelCause::Interrupted);
    helper.join().expect("helper");
    match fold_of(state) {
        Err(Break::Escape(Escape::Stopped { cause, .. })) => {
            assert_eq!(cause, CancelCause::Interrupted);
        }
        other => panic!("expected the teardown's stop, got {other:?}"),
    }
}

// ── SettleOnDrop: the dead-man's switch ─────────────────────────────────

/// Dropped mid-unwind, the guard settles its index itself, so no index goes
/// silent.
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
        Event::Returned(0, obs) => match obs.end {
            ThreadEnd::Settled(Err(Break::Error(err))) => assert!(
                err.message.contains("without reporting"),
                "expected the one dead-man's message: {}",
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

/// A guard dropped with no send made — a stage thread that returned without
/// filing its own `Returned` — reports a silent end.
#[test]
fn a_guard_dropped_without_a_panic_reports_a_silent_end() {
    let (tx, rx) = std::sync::mpsc::channel();
    drop(SettleOnDrop::new(Slot { ix: 0, tx }));
    match rx.recv().expect("the guard settles on drop") {
        Event::Returned(0, obs) => match obs.end {
            ThreadEnd::Settled(Err(Break::Error(err))) => assert!(
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
    collect.push(StageHandle::fake_thread_for_step_test(slot.clone()).0);

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
