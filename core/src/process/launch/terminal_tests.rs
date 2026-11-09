//! Real bwrap settlement, not a synthetic Envelope or a guard-only stand-in.

use crate::authority::sandbox::{
    Confinement, ExecProjection, FsProjection, Image, Ownership, SandboxProjection,
};
use crate::process::{
    CancelCause, DurableRoot, Group, Placement, PreparedTerminalLoan, Signal, Spawned, StdioSpec,
    TerminalAccess, TerminalLease,
};
use crate::shell::runtime::{Break, Escape, Stop};
use crate::test_helper::terminal::{self, Mode, PrivatePty};
use rustix::termios::{LocalModes, tcgetattr};
use std::time::Instant;

/// Before the collector takes ownership, even a failed checkpoint assertion
/// must kill the payload group before reaping its monitor.
struct Pending(Option<Spawned>);

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(mut spawned) = self.0.take() {
            if let Some(group) = spawned.group.owned() {
                group.kill();
            }
            let _ = spawned.child.kill();
            let _ = spawned.child.reap();
            // The terminal hold drops only after teardown and reap.
        }
    }
}

fn envelope_restores_modes(pty: &PrivatePty, cancel: bool) {
    // A missing backend is not a verified regression.
    crate::test_helper::confinement::confinement().unwrap_or_else(|why| {
        panic!("real Linux envelope backend unavailable; terminal restoration NOT VERIFIED: {why}")
    });
    let mut shell = crate::test_helper::core_shell();
    let scope = DurableRoot::default().child();
    shell.scope = crate::process::Scope::clone(&scope);
    let original = pty.cooked_modes();
    let foreground = rustix::termios::tcgetpgrp(pty.slave()).unwrap();
    // Keep attribution independent of the collection scope, so cancellation
    // cannot conceal an erroneous strike by this restoration-only hold.
    let home = DurableRoot::default().child();
    let lease = TerminalLease::mint_at_startup(true).unwrap();
    let permit = TerminalAccess::ExplicitLoan
        .reach(Some(&lease), &home, false)
        .unwrap();
    let prepared = PreparedTerminalLoan::capture(permit);
    let confinement = Confinement::Projected(Box::new(SandboxProjection {
        fs: FsProjection::Unrestricted,
        net: None,
        gpu: None,
        exec: ExecProjection::default(),
    }));
    let image = Image::File {
        real: crate::test_helper::real("/bin/sh"),
        arg0: "/bin/sh".into(),
    };
    let mut launch = confinement
        .launch(
            &image,
            &[
                "-c".into(),
                "set -e; stty -echo -icanon; printf M; IFS= read -r release <&1; exit 0".into(),
            ],
            Ownership::Kept,
            shell.context.cwd(),
        )
        .unwrap_or_else(|why| panic!("real Linux envelope launch unavailable: {why}"));
    assert!(
        launch.enveloped(),
        "this regression requires the real envelope"
    );
    let (mut checkpoint, payload) = terminal::checkpoints().unwrap();
    launch
        .stdin(StdioSpec::from_owned(pty.slave().try_clone().unwrap()))
        .stderr(StdioSpec::Null);
    // The payload, not bwrap, owns this checkpoint.
    launch.cmd.stdout(payload.into_stdio());
    let since = Instant::now();
    let spawned = launch
        .spawn(Placement::Tenant(prepared), &shell.scope)
        .unwrap_or_else(|why| panic!("real Linux envelope could not start: {why:?}"));
    let mut pending = Pending(Some(spawned));
    let spawned = pending.0.as_mut().unwrap();
    assert!(matches!(spawned.group, Group::Enveloped(_)));
    checkpoint
        .expect(b'M')
        .expect("payload changed terminal modes");
    let changed = tcgetattr(pty.slave()).unwrap();
    assert!(
        !changed
            .local_modes
            .intersects(LocalModes::ECHO | LocalModes::ICANON)
    );
    // No foreground handoff occurred, despite the requested Tenant placement.
    assert_eq!(rustix::termios::tcgetpgrp(pty.slave()).unwrap(), foreground);
    spawned.terminal.hear(Signal::new(libc::SIGINT));
    spawned.terminal.hear(Signal::new(libc::SIGQUIT));
    assert_eq!(spawned.terminal.pressed(), None);
    assert_eq!(home.cause(), None);
    if cancel {
        // The payload is explicitly blocked after changing modes; cancellation
        // must end it through the production collector, not a test-only kill.
        scope.cancel(CancelCause::Cancelled);
    } else {
        checkpoint.release_line().unwrap();
    }
    let settled =
        crate::shell::exec::pipeline::collect_spawned(pending.0.take().unwrap(), since, &shell);
    if cancel {
        assert!(
            matches!(
                settled,
                Err(Break::Escape(Escape::Stopped(Stop {
                    cause: CancelCause::Cancelled,
                    ..
                })))
            ),
            "{settled:?}"
        );
    } else {
        assert!(settled.is_ok(), "{settled:?}");
    }
    terminal::assert_modes(&tcgetattr(pty.slave()).unwrap(), &original);
    assert_eq!(rustix::termios::tcgetpgrp(pty.slave()).unwrap(), foreground);
    assert_eq!(home.cause(), None);
}

#[test]
fn a_real_linux_envelope_restores_modes_on_normal_settlement_without_attribution() {
    terminal::isolated(
        "process::launch::terminal_tests::a_real_linux_envelope_restores_modes_on_normal_settlement_without_attribution",
        Mode::Controlling,
        |pty| envelope_restores_modes(pty, false),
    );
}

#[test]
fn a_real_linux_envelope_restores_modes_on_cancellation_without_attribution() {
    terminal::isolated(
        "process::launch::terminal_tests::a_real_linux_envelope_restores_modes_on_cancellation_without_attribution",
        Mode::Controlling,
        |pty| envelope_restores_modes(pty, true),
    );
}
