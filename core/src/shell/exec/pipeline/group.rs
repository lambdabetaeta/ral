//! Process-group lifecycle for a multi-stage pipeline, on every platform.
//!
//! An owning group spawns [`Role::PipelineAnchor`](crate::Role::PipelineAnchor),
//! outliving every stage so the pgid stays joinable; a group joining
//! an enclosing stage's pgid has no anchor.  The shell is not a member, so a
//! signal the tty delivers to the group reaches it only as the anchor's
//! `Event::Heard` — the kernel having already given that signal to every
//! other member — which only the pipeline's
//! terminal hold reads as a key.  On
//! Windows a group a pipeline or a lone external owns is a `Job`.

use super::collect::Event;
#[cfg(windows)]
use crate::process::Pgid;
use crate::process::{
    Group, Leader, Membership, PreparedTerminalLoan, Scope, Spawned, TerminalHold, TerminalInput,
    Unstarted,
};
use crate::shell::runtime::{Break, Error, Settled};
use std::sync::mpsc::Sender;

/// Pgid lifecycle for one pipeline.  It drops after every stage handle has
/// gone, `PipeNode`'s field order guaranteeing it.
pub(super) enum PipelineGroup {
    /// Its own, led by its anchor, whose process pins the pgid until this
    /// drops: alive, then a zombie once ended, through every kill the fold
    /// makes.  Field order is teardown order: the job outlives the anchor.
    Owns {
        anchor: AnchorProcess,
        #[cfg(windows)]
        #[expect(dead_code, reason = "held for its drop alone")]
        job: Job,
    },
    /// An enclosing stage's, which a pipeline launched inside its thread
    /// joins rather than owning one.
    Joins(Membership),
}

impl PipelineGroup {
    /// An owning group: the anchor, spawned and witnessed at once, so no
    /// signal it swallows can land before someone is reading for it.
    pub(super) fn prepare(
        tx: Sender<Event>,
        prepared: Option<PreparedTerminalLoan>,
    ) -> Settled<(Self, TerminalHold, TerminalInput)> {
        // spawn returns only after the anchor's handlers and readiness byte.
        let anchor = AnchorProcess::spawn(tx)?;
        let (terminal, input) = match prepared {
            None => (TerminalHold::default(), TerminalInput::Withheld),
            Some(prepared) => match prepared.lend(&anchor.leader) {
                Ok(terminal) => (terminal, TerminalInput::Readable),
                Err((prepared, error)) => {
                    crate::dbg_trace!("pipeline terminal handoff", "{error}");
                    // Only the anchor exists: it reads its release pipe and
                    // reports signals, never changes modes. No stage needs
                    // tearing down before this restoration and fallback.
                    drop(prepared);
                    (TerminalHold::default(), TerminalInput::Withheld)
                }
            },
        };
        let group = Self::Owns {
            #[cfg(windows)]
            job: Job::owning(anchor.leader.pgid()),
            anchor,
        };
        Ok((group, terminal, input))
    }

    pub(super) const fn joining(membership: Membership) -> Self {
        Self::Joins(membership)
    }

    /// What a stage running under `stage` joins: an owner's leader paired
    /// with that scope, or a joiner's own membership, so a nested stage
    /// answers to the outermost owner's stage scope.
    pub(super) fn membership(&self, stage: &Scope) -> Membership {
        match self {
            Self::Owns { anchor, .. } => Membership::new(anchor.leader.pgid(), stage.clone()),
            Self::Joins(membership) => membership.clone(),
        }
    }

    pub(super) fn group(&self) -> Group {
        match self {
            Self::Owns { anchor, .. } => Group::Owns(anchor.leader.pgid()),
            Self::Joins(membership) => Group::Joins(membership.clone()),
        }
    }

    /// End the anchor, after which every report it made is on the channel.
    pub(super) fn end_anchor(&mut self) {
        if let Self::Owns { anchor, .. } = self {
            anchor.end();
        }
    }
}

/// The Job Object and `GROUPS` entry of a Windows group, owned: dropping it
/// releases both, killing whatever member is left.
#[cfg(windows)]
pub(super) struct Job(Pgid);

#[cfg(windows)]
impl Job {
    /// The job of `group`, which its leader's spawn registered.
    pub(super) const fn owning(group: Pgid) -> Self {
        Self(group)
    }

    /// Wait for every member to leave the job, its leader's descendants
    /// included, a slice at a time, killing the job once under a cause in
    /// force on `scope`.
    pub(super) fn drain(&self, scope: &Scope) {
        let mut kill = Some(|| self.0.kill());
        while matches!(
            crate::process::try_reap_leader(self.0),
            crate::process::ReapStatus::Running
        ) {
            if scope.is_cancelled()
                && let Some(kill) = kill.take()
            {
                kill();
            }
            std::thread::sleep(crate::process::POLL_SLICE);
        }
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        crate::process::release_win_group(self.0.as_raw());
    }
}

pub(super) struct AnchorProcess {
    /// The anchor, then its zombie once ended, reaped as this drops.
    #[cfg(unix)]
    #[expect(dead_code, reason = "held for its drop alone")]
    child: crate::process::Watch,
    #[cfg(windows)]
    child: crate::process::ChildHandle,
    leader: Leader,
    /// The anchor reads this to EOF; closing it is how `end` ends it.
    release: Option<os_pipe::PipeWriter>,
    /// Blocks reading the anchor's stdout — one byte per swallowed signal —
    /// to EOF.
    #[cfg(unix)]
    reporter: Option<std::thread::JoinHandle<()>>,
}

/// The anchor's own death, on its own, ends the pipeline: nothing else in the
/// group has heard of it.
#[cfg(unix)]
fn anchor_death(_: crate::process::WaitOutcome) -> Event {
    Event::AnchorDied
}

#[allow(clippy::needless_pass_by_value, reason = "named for `map_err`")]
fn anchor_error(operation: crate::fact::IoOperation, e: std::io::Error) -> Error {
    Error::io(operation, crate::fact::IoSubject::PipelineAnchor, &e)
}

/// `Child::drop` neither kills nor reaps, so a child not handed on goes here.
#[cfg(unix)]
fn discard(child: &mut crate::process::ChildHandle) {
    let _ = child.kill();
    let _ = child.reap();
}

impl AnchorProcess {
    /// Spawn the anchor and start its witness in one act: the report reader
    /// and the reaper's `Watch` on the anchor's pid are both live on `tx`
    /// before the pgid is handed to any stage.
    // Taken by value because the Unix arm below moves it into the watch; the
    // Windows arm has no reader thread to give it to.
    #[cfg_attr(windows, allow(clippy::needless_pass_by_value))]
    fn spawn(tx: Sender<Event>) -> Settled<Self> {
        #[cfg(windows)]
        let _ = tx;
        let (reader, release) = crate::process::cloexec_pipe()
            .map_err(|e| anchor_error(crate::fact::IoOperation::CreateFile, e))?;
        let mut cmd = crate::authority::sandbox::reexec::launch(crate::Role::PipelineAnchor)
            .map_err(|e| anchor_error(crate::fact::IoOperation::ReadLink, e))?;
        cmd.stdin(crate::process::StdioSpec::from_owned(reader));
        cmd.stderr(crate::process::StdioSpec::Null);
        #[cfg(unix)]
        let mut report = {
            let (report, writer) = crate::process::cloexec_pipe()
                .map_err(|e| anchor_error(crate::fact::IoOperation::CreateFile, e))?;
            cmd.stdout(crate::process::StdioSpec::from_owned(writer));
            report
        };
        #[cfg(windows)]
        cmd.stdout(crate::process::StdioSpec::Null);
        // ral's own process, so no stop ends its birth: each stage's start
        // sees one.
        let unstoppable = crate::process::DurableRoot::default().child();
        let Spawned { child, group, .. } =
            cmd.spawn_leader(&unstoppable)
                .map_err(|unstarted| match unstarted {
                    Unstarted::Program(e) => Break::from(crate::process::launch::prepare_io(
                        crate::fact::PrepareStep::Application,
                        crate::fact::PrepareSubject::Program("pipeline anchor".into()),
                        &e,
                    )),
                    Unstarted::Refused(refusal) => Break::from(refusal),
                    Unstarted::Stopped(stop) => Break::from(stop),
                })?;
        #[cfg(unix)]
        let (child, reporter) = {
            use std::io::Read;

            let mut child = child;
            let mut ready = [0u8; 1];
            let readiness = report
                .read_exact(&mut ready)
                .map_err(|e| {
                    crate::fact::AnchorReadinessFailure::Read(Box::new(crate::io::failure(&e)))
                })
                .and_then(|()| {
                    if ready == [0] {
                        Ok(())
                    } else {
                        Err(crate::fact::AnchorReadinessFailure::UnexpectedByte(
                            ready[0],
                        ))
                    }
                });
            if let Err(cause) = readiness {
                discard(&mut child);
                return Err(
                    crate::process::launch::refused(crate::fact::LaunchFailure::Launcher(
                        crate::fact::LauncherFailure::AnchorReadiness(cause),
                    ))
                    .into(),
                );
            }
            let report_tx = tx.clone();
            let reporter = std::thread::Builder::new()
                .name("ral pipeline anchor report reader".to_string())
                .spawn(move || read_anchor_reports(report, &report_tx))
                .map_err(|e| {
                    discard(&mut child);
                    Error::io(
                        crate::fact::IoOperation::StartThread,
                        crate::fact::IoSubject::Thread("ral pipeline anchor report reader".into()),
                        &e,
                    )
                })?;
            (child.into_watch(tx, anchor_death), reporter)
        };
        Ok(Self {
            child,
            leader: group,
            release: Some(release),
            #[cfg(unix)]
            reporter: Some(reporter),
        })
    }

    /// The reader leaves only on EOF, the anchor's own exit closing the write
    /// end, so the anchor can never `SIGPIPE` on a report.  Idempotent.
    #[cfg(unix)]
    fn end(&mut self) {
        drop(self.release.take());
        if let Some(reporter) = self.reporter.take() {
            let _ = reporter.join();
        }
    }

    #[cfg(windows)]
    fn end(&mut self) {
        if let Some(release) = self.release.take() {
            drop(release);
            let _ = self.child.reap();
        }
    }
}

impl Drop for AnchorProcess {
    /// Ended first: reaping waits for an anchor that only `end` ends.
    fn drop(&mut self) {
        self.end();
    }
}

/// One byte per swallowed signal — one the kernel already delivered to every
/// other member — sent raw: only the loan knows whether it was a key.  EOF
/// means the anchor exited, which its own watch reports instead.
#[cfg(unix)]
fn read_anchor_reports(mut report: os_pipe::PipeReader, tx: &Sender<Event>) {
    use std::io::Read;
    let mut byte = [0u8; 1];
    while report.read_exact(&mut byte).is_ok() {
        let signal = crate::process::Signal::new(i32::from(byte[0]));
        if tx.send(Event::Heard(signal)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::process::CancelCause;
    use crate::process::DurableRoot;

    /// An owning group whose channel nobody reads — enough for every test
    /// that only wants a real pgid.
    fn prepared() -> PipelineGroup {
        PipelineGroup::prepare(std::sync::mpsc::channel().0, None)
            .expect("anchor spawns")
            .0
    }

    /// Anchor readiness settles the loan before any stage exists.
    #[cfg(unix)]
    #[test]
    fn a_refused_anchor_handoff_selects_withheld_before_stage_launch() {
        use crate::io::Source;
        use crate::process::{TerminalAccess, TerminalLease};
        use crate::test_helper::terminal::{self, Mode};
        use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};

        terminal::isolated(
            "shell::exec::pipeline::group::tests::a_refused_anchor_handoff_selects_withheld_before_stage_launch",
            Mode::NonControlling,
            |pty| {
                let mut shell = crate::test_helper::core_shell();
                shell.io.stdin = Source::Terminal;
                let home = DurableRoot::default().child();
                let lease = TerminalLease::mint_at_startup(true).expect("lease");
                let permit = TerminalAccess::Leased
                    .reach(Some(&lease), &home, true)
                    .expect("permit");
                let before = tcgetattr(pty.slave()).expect("original modes");
                let prepared = PreparedTerminalLoan::capture(permit);
                let mut changed = before.clone();
                changed.local_modes.toggle(LocalModes::ECHO);
                tcsetattr(pty.slave(), OptionalActions::Now, &changed)
                    .expect("change modes while preparation holds them");
                let (tx, rx) = std::sync::mpsc::channel();
                let (mut group, terminal, input) = PipelineGroup::prepare(tx, Some(prepared))
                    .expect("anchor readiness and refused handoff settle");
                assert!(matches!(input, TerminalInput::Withheld));
                assert_eq!(terminal.pressed(), None);
                terminal::assert_modes(&tcgetattr(pty.slave()).expect("restored modes"), &before);
                assert!(rx.try_recv().is_err(), "readiness is not a stage event");
                let stdin = super::super::launch::stage_stdin(
                    super::super::route::ByteIn::Parent,
                    &shell,
                    &home,
                    input,
                )
                .expect("first thread stage input");
                assert!(matches!(stdin, Source::Empty));
                group.end_anchor();
                drop(terminal);
                drop(group);
            },
        );
    }

    /// An owner pairs its leader with the scope it is given; a joiner hands
    /// back its own membership whatever scope it is given.
    #[test]
    fn a_nested_stage_answers_to_the_outermost_owners_stage() {
        let owner = prepared();
        let leader = owner.group().owned().expect("owning group");
        let outer = DurableRoot::default().child();
        let membership = owner.membership(&outer);
        assert_eq!(membership.group(), leader);

        let joiner = PipelineGroup::joining(membership);
        let inner = DurableRoot::default().child();
        let nested = joiner.membership(&inner);
        assert_eq!(nested.group(), leader);
        #[cfg(unix)]
        {
            outer.cancel(CancelCause::Interrupted);
            assert!(
                !nested.owes(CancelCause::Interrupted),
                "a nested stage's membership must read the outer stage's scope"
            );
            inner.cancel(CancelCause::TimedOut);
            assert!(
                nested.owes(CancelCause::TimedOut),
                "the inner scope must not stand in for the outer one"
            );
        }
    }

    /// An ended anchor's zombie pins the pgid through every kill the fold
    /// makes; the group's drop reaps it.
    #[cfg(unix)]
    #[test]
    fn an_ended_anchor_pins_its_group_until_the_group_drops() {
        let mut group = prepared();
        let leader = group.group().owned().expect("owning group").as_raw();
        group.end_anchor();
        assert_eq!(
            unsafe { libc::kill(leader, 0) },
            0,
            "the ended anchor must still be a zombie"
        );
        drop(group);
        assert_ne!(
            unsafe { libc::kill(leader, 0) },
            0,
            "the group's drop must reap its anchor"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_completed_group_releases_its_windows_job() {
        let group = prepared();
        let leader = group.group().owned().expect("owning group").as_raw();
        assert!(crate::process::group::is_known_group(leader));
        drop(group);
        assert!(!crate::process::group::is_known_group(leader));
    }

    #[cfg(unix)]
    fn heard_within_2s(rx: &std::sync::mpsc::Receiver<Event>) -> Option<i32> {
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Event::Heard(signal)) => Some(signal.number()),
            Ok(_) | Err(_) => None,
        }
    }

    /// A signal the group swallows comes back raw as `Heard`, key or not; two
    /// in a row prove the anchor survived the first.
    #[cfg(unix)]
    #[test]
    fn a_signalled_anchor_reports_the_signal_and_lives_on() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (group, _, _) = PipelineGroup::prepare(tx, None).expect("anchor spawns");
        for signal in [libc::SIGINT, libc::SIGTERM] {
            group
                .group()
                .owned()
                .expect("owning group")
                .signal_group(crate::process::Signal::new(signal));
            match heard_within_2s(&rx) {
                Some(n) if n == signal => {}
                Some(n) => panic!("signal {signal} heard as {n}"),
                None => panic!("signal {signal} was never reported"),
            }
        }
        assert!(rx.try_recv().is_err(), "readiness is not a signal report");
    }
}
