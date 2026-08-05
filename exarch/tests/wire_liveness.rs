// The whole file is Unix-only: it stands a `socketpair` in for the vsock
// stream, and both the stand-in and the [`WireTransport`] under test exist
// only there.  An empty test binary on Windows is the honest outcome — the
// laws below are laws about a transport that platform does not have.
#![cfg(unix)]
#![allow(clippy::disallowed_methods)]

//! §3 frame protocol over vsock — the heartbeat and run-durability laws
//! (`dev/docs/VM/SYNOD.md` §3), proven end to end against a *real* engine
//! child.
//!
//! A `socketpair` stands in for the vsock stream: the codec is
//! transport-agnostic by design, so the engine cannot tell whether fd 3 is
//! one end of a same-host `socketpair` or a connection to the host over
//! `AF_VSOCK`. The guest side of the stand-in mirrors ral-daemon's spawn
//! contract (`ral-daemon/src/engine.rs::spawn`): the engine is this multicall
//! binary re-exec'd with `--engine`, its protocol socket on fd 3. The test
//! binary serves `--engine` itself through the shared pre-`main` `#[ctor]`
//! ([`exarch::pre_main_ctor!`]), so `std::env::current_exe()` is a genuine
//! engine.

use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use ral_core::io::TerminalState;
use ral_core::transport::{
    Control, DispatchId, EnquiryError, Event, Liveness, Program, Report, Run, TerminalEndpoint,
    Transport, WireTransport, dispatch_to_report,
};
use ral_core::types::Capabilities;
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};

// Mirror the binary's pre-`main` re-exec dispatch: the `--engine` re-exec is
// served here, before libtest sees a flag it would reject, so a re-exec'd
// child becomes the engine rather than a second copy of the test harness.
// See [`exarch::dispatch_pre_main`].
exarch::pre_main_ctor!();

/// Kills and reaps the engine child on drop, so no engine — and no `sleep`
/// it spawned — outlives a failed assertion.
struct EngineChild(std::process::Child);

impl Drop for EngineChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn a real engine child over one end of a `socketpair` and adopt the
/// host end into a `WireTransport` driving the §3 frame protocol under
/// `liveness`.
///
/// The `pre_exec` is the canonical one from `WireTransport::new` and
/// ral-daemon's `spawn`: `dup2` the guest end onto fd 3, close the original
/// if it landed elsewhere. The guest end lives only in the child afterwards;
/// the host end is the parent's sole handle to the seam.
fn engine_over_socketpair(liveness: Liveness) -> (WireTransport, EngineChild) {
    let (host, guest) = UnixStream::pair().expect("socketpair");
    let guest_fd = guest.as_raw_fd();

    let mut cmd = std::process::Command::new(std::env::current_exe().expect("current exe"));
    cmd.arg("--engine");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    // SAFETY: the closure runs between fork and exec and calls only
    // async-signal-safe syscalls (`dup2`, `close`) with no allocation and no
    // locking — the canonical pre_exec of `WireTransport::new`.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if libc::dup2(guest_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if guest_fd != 3 {
                libc::close(guest_fd);
            }
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawn engine child");
    // The guest end has crossed into the child (as fd 3); the parent must not
    // hold it, or the engine would never see EOF when the host end shuts down.
    drop(guest);

    let transport = WireTransport::adopt(host, liveness).expect("adopt host stream");
    (transport, EngineChild(child))
}

/// Attach the session to a fresh tempdir as cwd/home, tagging the engine's
/// builtin installer with exarch's own [`INSTALLER_TAG`].
///
/// [`INSTALLER_TAG`]: exarch::shell_eval::builtins::INSTALLER_TAG
fn attach(transport: &WireTransport) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    transport.attach(
        TerminalEndpoint {
            lease: None,
            state: TerminalState::default(),
        },
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        None,
        exarch::shell_eval::builtins::INSTALLER_TAG.to_string(),
    );
    dir
}

/// One capturing run under the ⊤ capability ceiling, uncapped and
/// stdin-less — the shape `core/src/transport.rs`'s own `capture_req` mints.
fn source_run(src: &str) -> Run {
    Run {
        program: Program::Source(src.into()),
        script_name: "<test>".into(),
        caps: Capabilities::root(),
        wall: None,
        deferred_lease: None,
        worker_cap: None,
        io: RunIo::Capture,
        terminal: RequestedTerminalAccess::Denied,
        stdin: RunStdin::Empty,
        trail: None,
    }
}

/// The §3 heartbeat keeps an *idle* session alive across a real child: adopt
/// → attach → a run settles to a `Report::Ran { Ok }`, and then the
/// session, left silent for wall-clock time well past its read deadline,
/// stays live — because the engine's `Pong` echoes of the host's `Ping`s are
/// the traffic that resets the deadline.
#[test]
fn an_idle_session_stays_alive_on_the_heartbeat_alone() {
    let (transport, _child) = engine_over_socketpair(Liveness {
        interval: Duration::from_millis(50),
        deadline: Duration::from_secs(2),
    });
    let _dir = attach(&transport);

    let report = dispatch_to_report(
        &transport,
        source_run("$[1 + 1]"),
        |_| {},
        |_| -> Result<_, EnquiryError> { unreachable!("this run raises no enquiry") },
    )
    .expect("the engine must answer the dispatch with a Report");

    assert!(
        matches!(
            report,
            Report::Ran {
                ending: ral_core::transport::Ending::Settled { .. },
                ..
            }
        ),
        "`$[1 + 1]` must settle to Report::Ran {{ Ok }}, got {report:?}"
    );

    // Idle comfortably past the 2s deadline. Nothing but the heartbeat crosses
    // the seam in this window; if the Pong traffic did not reset the deadline,
    // the engine would be declared dead here.
    std::thread::sleep(Duration::from_secs(3));

    assert!(
        !transport.dead(),
        "an idle session must stay alive on the heartbeat past its deadline"
    );
}

/// A `Cancel` that overtakes the `Dispatch` it names still stops that run.
///
/// The host stamps the id it is about to send before it takes the write lock,
/// so a cancel raised in that window reaches the socket first.  Written by
/// hand here in that order — no thread race to lose — the engine must hold it
/// and spend it on the run's scope when the dispatch lands, not drop it for
/// naming a dispatch it has not yet seen.
#[test]
fn a_cancel_that_overtakes_its_dispatch_still_stops_the_run() {
    let (transport, _child) = engine_over_socketpair(Liveness::default());
    let _dir = attach(&transport);

    let id = DispatchId(7);
    transport.control().send(Control::Cancel(id));
    let started = Instant::now();
    transport.dispatch(id, source_run("sleep 30"));

    let report = loop {
        let (did, event) = transport.events().recv().expect("the engine must answer");
        if did == id
            && let Event::Report(report) = event
        {
            break report;
        }
    };
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "the overtaking cancel was dropped: the run slept {elapsed:?} of its 30 s"
    );
    let Report::Ran { ending, .. } = report else {
        panic!("the run must reach evaluation, got {report:?}");
    };
    // 130, not the 143 of a child torn down mid-run: the scope is cancelled
    // before the run reads it, so `sleep` is never spawned and the eval unwinds
    // at its first check point.
    assert_eq!(
        ending.status(),
        130,
        "a run born under a cancelled scope unwinds without spawning"
    );
}

/// A dead peer fails the in-flight run as cancelled: a run that would take
/// far longer than the test (`sleep 30`) is dispatched and left running;
/// dropping the transport shuts the socket, so the engine sees EOF, cancels
/// the in-flight run, waits for it to settle, and exits — well within a
/// generous bound, rather than running the sleep to term or hanging on a
/// peer that will never speak again.
#[test]
fn a_dead_peer_fails_the_in_flight_run_as_cancelled() {
    let (transport, mut child) = engine_over_socketpair(Liveness::default());
    let _dir = attach(&transport);

    // Fire-and-forget: write the Dispatch frame and do not drain its Report.
    // A literal id suffices — nothing here correlates a reply.
    transport.dispatch(DispatchId(1), source_run("sleep 30"));

    // Give the engine time to actually enter the sleep, so what the dropped
    // peer interrupts is a genuinely in-flight run, not a race with dispatch.
    std::thread::sleep(Duration::from_secs(1));

    // The engine sees EOF on fd 3 once the transport drops.
    drop(transport);

    // The engine must cancel the in-flight `sleep 30` and exit well within a
    // bound far shorter than the sleep itself. Poll `try_wait` rather than
    // block, so a hang fails the test by timeout instead of wedging it.
    let deadline = Instant::now() + Duration::from_secs(20);
    let exited = loop {
        match child.0.try_wait().expect("try_wait the engine child") {
            Some(_) => break true,
            None if Instant::now() >= deadline => break false,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };

    assert!(
        exited,
        "a dead peer must fail the in-flight run as cancelled and exit, not run `sleep 30` to \
         term or hang"
    );
}
