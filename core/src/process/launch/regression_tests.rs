//! Spawn-boundary report attribution, retained requests, and surrendered survivors.

use super::{Launch, Unstarted};
use crate::authority::sandbox::ProgramLaunch;
use crate::fact::FailureCode as _;
use crate::process::report::{Report, Witness};
use crate::process::{CancelCause, DurableRoot, Placement, Scope, ScopeNode};
use crate::test_helper::terminal::{self, Mode, PrivatePty};
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::mpsc;

fn report() -> (Report, Witness) {
    let (reader, writer) = crate::process::cloexec_socketpair().unwrap();
    let witness = Witness::new(writer.try_clone().unwrap().into()).unwrap();
    (Report::new(reader, writer.into()), witness)
}

/// A synthetic envelope naming `payload` as its payload's group.
#[cfg(target_os = "linux")]
fn enveloped(
    cmd: std::process::Command,
    report: Report,
    payload: Option<crate::process::Pgid>,
) -> ProgramLaunch {
    ProgramLaunch::Enveloped(super::EnvelopedLaunch::synthetic(
        cmd,
        crate::fact::LauncherRole::Bwrap,
        payload.map(|payload| Box::new(move || Ok(Some(payload))) as _),
        report,
    ))
}

fn reported(cmd: std::process::Command, report: Report, enveloped: bool) -> ProgramLaunch {
    #[cfg(target_os = "linux")]
    if enveloped {
        return self::enveloped(cmd, report, None);
    }
    assert!(!enveloped);
    ProgramLaunch::Direct(Launch::trampolined(cmd, report))
}

fn kept(launch: ProgramLaunch, placement: Placement, scope: &Scope) -> Unstarted {
    launch
        .spawn(placement, scope)
        .err()
        .expect("the report must prevent a kept spawn from succeeding")
}

fn detached(launch: ProgramLaunch, scope: &Scope) -> Unstarted {
    launch
        .spawn_detached(scope)
        .expect_err("the report must prevent a detached spawn from succeeding")
}

#[cfg(target_os = "linux")]
#[test]
fn an_envelopes_failed_exec_is_refused_kept_and_detached() {
    for surrender in [false, true] {
        let (report, _witness) = report();
        let mut cmd = std::process::Command::new("/ral-no-such-envelope-exists");
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let launch =
            super::EnvelopedLaunch::synthetic(cmd, crate::fact::LauncherRole::Bwrap, None, report);
        let scope = DurableRoot::default().child();
        let why = if surrender {
            launch.spawn_detached(&scope).err()
        } else {
            launch.spawn(Placement::Inherit, &scope).err()
        };
        let Some(Unstarted::Refused(why)) = why else {
            panic!("the envelope's exec failure must be a refusal: {why:?}");
        };
        assert!(matches!(&why.reason, crate::fact::Reason::Launch(failure)
            if matches!(**failure, crate::fact::LaunchFailure::Launcher(crate::fact::LauncherFailure::Exec(_)))));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn an_envelopes_unstarted_report_preserves_the_programs_errno_kept_and_detached() {
    for surrender in [false, true] {
        for errno in [libc::ENOENT, libc::EACCES, libc::ENOEXEC] {
            let (report, witness) = report();
            witness.unstarted(
                &io::Error::from_raw_os_error(errno),
                crate::fact::LaunchImageSubject::File("test".into()),
            );
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.args(["-c", "exit 0"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let launch = reported(cmd, report, true);
            let scope = DurableRoot::default().child();
            let why = if surrender {
                detached(launch, &scope)
            } else {
                kept(launch, Placement::Inherit, &scope)
            };
            assert!(
                matches!(why, Unstarted::Program(ref e) if e.raw_os_error() == Some(errno)),
                "{why:?}"
            );
        }
    }
}

/// The shell records its own pid after the detached fork, then explicitly
/// blocks on its checkpoint. Readiness proves the pid record is complete.
fn waiting_child(
    pty: Option<&PrivatePty>,
) -> (
    std::process::Command,
    tempfile::NamedTempFile,
    terminal::Checkpoint,
) {
    let pid = tempfile::NamedTempFile::new().unwrap();
    let (observer, child) = terminal::checkpoints().unwrap();
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args([
        "-c",
        if pty.is_some() {
            "set -e; printf '%s\\n' \"$$\" > \"$1\"; stty -echo -icanon; printf R; IFS= read -r release <&1; printf A; IFS= read -r release <&1"
        } else {
            "set -e; printf '%s\\n' \"$$\" > \"$1\"; printf R; IFS= read -r release <&1; printf A; IFS= read -r release <&1"
        },
        "ral-spawn-regression",
    ]).arg(pid.path());
    cmd.stdin(pty.map_or_else(std::process::Stdio::null, |pty| {
        pty.slave().try_clone().unwrap().into()
    }))
    .stdout(child.into_stdio())
    .stderr(std::process::Stdio::null());
    (cmd, pid, observer)
}

fn child_pid(file: &std::fs::File) -> i32 {
    let mut bytes = [0; 32];
    let count = file.read_at(&mut bytes, 0).unwrap();
    std::str::from_utf8(&bytes[..count])
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Whether `pid` is no child of ours any more: reaped, not merely killed.
fn reaped(pid: i32) -> bool {
    // SAFETY: this call only inspects, or reaps, this specific child.
    let waited = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
    waited == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
}

fn assert_reaped(pid: i32) {
    // SAFETY: this call only inspects this specific child's pid.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "the failed child still exists"
    );
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    assert!(reaped(pid), "spawn must reap, not merely kill");
}

fn hear_failure(witness: Witness, scope: &ScopeNode, stop: bool) -> Option<Witness> {
    if stop {
        scope.cancel(CancelCause::Cancelled);
        // Keep the writer open until cancellation is set: EOF is not the cue.
        Some(witness)
    } else {
        witness.refused(&crate::fact::LaunchFailure::Warrant(
            crate::fact::WarrantFailure::Magic,
        ));
        None
    }
}

fn assert_failure(why: &Unstarted, stop: bool) {
    if stop {
        assert!(
            matches!(why, Unstarted::Stopped(c) if c.cause == CancelCause::Cancelled),
            "{why:?}"
        );
    } else {
        assert!(
            matches!(why, Unstarted::Refused(e) if matches!(&e.reason, crate::fact::Reason::Launch(why) if matches!(**why, crate::fact::LaunchFailure::Warrant(crate::fact::WarrantFailure::Magic)))),
            "{why:?}"
        );
    }
}

fn tenant_report_failure(pty: &PrivatePty, enveloped: bool) {
    let before = pty.cooked_modes();
    let (cmd, pid, mut ready) = waiting_child(Some(pty));
    let (report, witness) = report();
    let scope = DurableRoot::default().child();
    let lease = crate::process::TerminalLease::mint_at_startup(true).unwrap();
    let permit = crate::process::TerminalAccess::ExplicitLoan
        .reach(Some(&lease), &scope, false)
        .unwrap();
    let prepared = crate::process::PreparedTerminalLoan::capture(permit);
    let launch = reported(cmd, report, enveloped);
    let (finished, finish) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let why = kept(launch, Placement::Tenant(prepared), &scope);
        finished.send(why).unwrap();
    });
    ready.expect(b'R').unwrap();
    let changed = rustix::termios::tcgetattr(pty.slave()).unwrap();
    assert!(
        !changed
            .local_modes
            .intersects(rustix::termios::LocalModes::ECHO | rustix::termios::LocalModes::ICANON)
    );
    witness.refused(&crate::fact::LaunchFailure::Warrant(
        crate::fact::WarrantFailure::Magic,
    ));
    let why = finish
        .recv_timeout(terminal::WAIT)
        .expect("failed birth must settle");
    worker.join().unwrap();
    assert_failure(&why, false);
    assert_reaped(child_pid(pid.as_file()));
    terminal::assert_modes(&rustix::termios::tcgetattr(pty.slave()).unwrap(), &before);
}

/// A request that records, when it is released, whether its child was
/// reaped by then.
struct Released {
    child: Option<i32>,
    reaped: mpsc::Sender<bool>,
}

impl Drop for Released {
    fn drop(&mut self) {
        let _ = self.reaped.send(self.child.is_some_and(reaped));
    }
}

/// The request holds what teardown must outlive, a tenant's saved terminal
/// modes: a failed birth releases it only once the child is reaped.
#[test]
#[allow(
    clippy::result_large_err,
    reason = "the landing returns its request beside the refusal, as every landing does"
)]
fn a_failed_birth_releases_its_request_only_after_reaping() {
    use crate::fact::{
        LaunchFailure, PrepareCause, PrepareFailure, PrepareStep, PrepareSubject, Reason,
    };

    let (reaped, released) = mpsc::channel();
    let mut launch = Launch::new("/bin/sh");
    launch.args(["-c", "sleep 1000"]);
    let why = launch
        .born_direct(
            crate::process::group::Step::NewLeader,
            Released {
                child: None,
                reaped,
            },
            &DurableRoot::default().child(),
            |mut request, child| {
                request.child = Some(child.as_raw());
                let refusal = super::prepare_io(
                    PrepareStep::TerminalHandover,
                    PrepareSubject::Group(u64::from(child.as_raw().unsigned_abs()).into()),
                    &io::Error::from_raw_os_error(libc::EIO),
                );
                Err::<((), crate::process::TerminalHold), _>((request, Unstarted::Refused(refusal)))
            },
        )
        .err()
        .expect("a refused landing fails the birth");
    assert!(
        matches!(&why, Unstarted::Refused(error)
            if matches!(&error.reason, Reason::Launch(failure)
                if matches!(failure.as_ref(), LaunchFailure::Prepare(PrepareFailure {
                    step: PrepareStep::TerminalHandover,
                    subject: PrepareSubject::Group(_),
                    cause: PrepareCause::Io(cause),
                }) if **cause == crate::io::failure(&io::Error::from_raw_os_error(libc::EIO))))),
        "{why:?}"
    );
    assert_eq!(released.try_recv(), Ok(true), "released before the reap");
}

/// A report that fails after the envelope named its payload's group kills
/// that group, which the monitor's own death does not reach.
#[cfg(target_os = "linux")]
#[test]
fn an_envelopes_refused_or_stopped_report_kills_the_payload_group_it_named() {
    use std::os::unix::process::ExitStatusExt;
    crate::test_helper::subprocess::isolated(
        "process::launch::regression_tests::an_envelopes_refused_or_stopped_report_kills_the_payload_group_it_named",
        || {
            for stop in [false, true] {
                let mut payload = Launch::new("/bin/sh");
                payload
                    .args(["-c", "sleep 1000"])
                    .stdin(crate::process::StdioSpec::Null)
                    .stdout(crate::process::StdioSpec::Null)
                    .stderr(crate::process::StdioSpec::Null);
                let mut payload = payload
                    .spawn_leader(&DurableRoot::default().child())
                    .expect("spawn the payload's stand-in");
                let (cmd, pid, mut ready) = waiting_child(None);
                let (report, witness) = report();
                let scope = DurableRoot::default().child();
                let worker_scope = Scope::clone(&scope);
                let launch = enveloped(cmd, report, Some(payload.group.pgid()));
                let (finished, finish) = mpsc::channel();
                let worker = std::thread::spawn(move || {
                    finished
                        .send(kept(launch, Placement::Inherit, &worker_scope))
                        .unwrap();
                });
                ready.expect(b'R').unwrap();
                let _silent = hear_failure(witness, &scope, stop);
                let why = finish
                    .recv_timeout(terminal::WAIT)
                    .expect("envelope cleanup must settle");
                worker.join().unwrap();
                assert_failure(&why, stop);
                assert_reaped(child_pid(pid.as_file()));
                let (ended, end) = mpsc::channel();
                std::thread::spawn(move || ended.send(payload.child.reap()));
                let status = end
                    .recv_timeout(terminal::WAIT)
                    .expect("the named payload group must be killed")
                    .expect("reap the payload's stand-in");
                assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
            }
        },
    );
}

#[test]
fn a_direct_tenants_refused_report_reaps_and_restores_modes() {
    terminal::isolated(
        "process::launch::regression_tests::a_direct_tenants_refused_report_reaps_and_restores_modes",
        Mode::NonControlling,
        |pty| tenant_report_failure(pty, false),
    );
}

#[cfg(target_os = "linux")]
#[test]
fn an_enveloped_tenants_refused_report_reaps_and_restores_modes() {
    terminal::isolated(
        "process::launch::regression_tests::an_enveloped_tenants_refused_report_reaps_and_restores_modes",
        Mode::NonControlling,
        |pty| tenant_report_failure(pty, true),
    );
}

#[cfg(target_os = "linux")]
#[test]
fn an_envelopes_refused_or_stopped_report_kills_and_reaps() {
    crate::test_helper::subprocess::isolated(
        "process::launch::regression_tests::an_envelopes_refused_or_stopped_report_kills_and_reaps",
        || {
            for stop in [false, true] {
                let (cmd, pid, mut ready) = waiting_child(None);
                let (report, witness) = report();
                let scope = DurableRoot::default().child();
                let worker_scope = Scope::clone(&scope);
                let launch = reported(cmd, report, true);
                let (finished, finish) = mpsc::channel();
                let worker = std::thread::spawn(move || {
                    finished
                        .send(kept(launch, Placement::Inherit, &worker_scope))
                        .unwrap();
                });
                ready.expect(b'R').unwrap();
                let _silent = hear_failure(witness, &scope, stop);
                let why = finish
                    .recv_timeout(terminal::WAIT)
                    .expect("envelope cleanup must settle");
                worker.join().unwrap();
                assert_failure(&why, stop);
                assert_reaped(child_pid(pid.as_file()));
            }
        },
    );
}

fn detached_report_failure(enveloped: bool) {
    for stop in [false, true] {
        let (cmd, pid, mut ready) = waiting_child(None);
        let (report, witness) = report();
        let scope = DurableRoot::default().child();
        let worker_scope = Scope::clone(&scope);
        let launch = reported(cmd, report, enveloped);
        let (finished, finish) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            finished.send(detached(launch, &worker_scope)).unwrap();
        });
        ready.expect(b'R').unwrap();
        let _silent = hear_failure(witness, &scope, stop);
        let why = finish
            .recv_timeout(terminal::WAIT)
            .expect("detached report wait must settle");
        worker.join().unwrap();
        assert_failure(&why, stop);
        let pid = child_pid(pid.as_file());
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        // Execution after spawn returned proves survival, unlike kill(pid, 0),
        // which can also succeed for a zombie. EOF also releases on failure.
        ready.release_line().unwrap();
        ready
            .expect(b'A')
            .expect("the surrendered survivor must still execute");
        ready.release_line().unwrap();
    }
}

#[test]
fn a_direct_detached_refused_or_stopped_report_leaves_its_survivor_alive() {
    crate::test_helper::subprocess::isolated(
        "process::launch::regression_tests::a_direct_detached_refused_or_stopped_report_leaves_its_survivor_alive",
        || detached_report_failure(false),
    );
}

#[cfg(target_os = "linux")]
#[test]
fn an_enveloped_detached_refused_or_stopped_report_leaves_its_survivor_alive() {
    crate::test_helper::subprocess::isolated(
        "process::launch::regression_tests::an_enveloped_detached_refused_or_stopped_report_leaves_its_survivor_alive",
        || detached_report_failure(true),
    );
}

#[test]
fn a_nonexistent_cwd_is_spawn_before_setup_not_stdio_or_program_failure() {
    let temp = tempfile::tempdir().unwrap();
    let mut command = std::process::Command::new("/bin/true");
    command.current_dir(temp.path().join("absent"));
    let why = kept(
        ProgramLaunch::Direct(Launch::from_command(command)),
        Placement::Inherit,
        &DurableRoot::default().child(),
    );
    let Unstarted::Refused(error) = why else {
        panic!("missing cwd is not a program exec error: {why:?}");
    };
    assert_eq!(error.failure_code().get(), 1);
    assert!(matches!(&error.reason, crate::fact::Reason::Launch(failure)
        if matches!(&**failure, crate::fact::LaunchFailure::Launcher(
            crate::fact::LauncherFailure::SpawnBeforeSetup(crate::fact::LauncherFailureSpawnBeforeSetup {
                subject: crate::fact::LaunchImageSubject::File(image), cause,
            })
        ) if image == "/bin/true" && cause.raw == Some(libc::ENOENT))));
}

#[test]
fn an_unmarked_non_native_failure_keeps_its_io_cause() {
    let error = io::Error::from(io::ErrorKind::InvalidInput);
    let cause = crate::io::failure(&error);
    let why = super::fork::<()>(
        std::process::Command::new("/bin/true"),
        None,
        super::setup::Evidence::new().unwrap(),
        |_, _| Err(crate::process::SpawnError::Prepare(error)),
    )
    .unwrap_err();
    let Unstarted::Refused(error) = why else {
        panic!("request construction is ral's failure");
    };
    assert_eq!(error.failure_code().get(), 1);
    assert!(matches!(&error.reason, crate::fact::Reason::Launch(failure)
        if matches!(&**failure, crate::fact::LaunchFailure::Launcher(
            crate::fact::LauncherFailure::NonNativeExecError(crate::fact::LauncherFailureNonNativeExecError {
                subject: crate::fact::LaunchImageSubject::File(image), cause: actual,
            })
        ) if image == "/bin/true" && **actual == cause)));
}

#[test]
fn a_detached_setup_failure_retains_the_failed_session_limit() {
    let mut launch = Launch::new("/bin/true");
    launch.limit_session(crate::process::ChildLimits {
        open_files: Some(u64::MAX - 1),
        ..Default::default()
    });
    let why = launch
        .spawn_detached(&DurableRoot::default().child())
        .unwrap_err();
    let Unstarted::Refused(error) = why else {
        panic!("failed setup must refuse the launch");
    };
    assert_eq!(error.failure_code().get(), 1);
    assert!(matches!(&error.reason, crate::fact::Reason::Launch(failure)
    if matches!(&**failure, crate::fact::LaunchFailure::Prepare(crate::fact::PrepareFailure {
        step: crate::fact::PrepareStep::SessionLimit,
        subject: crate::fact::PrepareSubject::SessionLimit(crate::fact::SessionLimit::OpenFiles),
        cause: crate::fact::PrepareCause::Io(_),
    }))));
}
