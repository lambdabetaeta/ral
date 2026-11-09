use super::*;
use crate::process::{ChildHandle, Group, Placement};
use crate::test_helper::terminal::{self, Checkpoint, Mode, PrivatePty};
use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcgetpgrp, tcsetattr, tcsetpgrp};

/// Own a fresh leader until its last kill; reap only after killing the group.
/// This also tears down mode-changing children on an assertion failure.
struct ModeChild {
    child: Option<ChildHandle>,
    group: Pgid,
}

impl ModeChild {
    fn spawn(pty: &PrivatePty, script: &str) -> (Self, Checkpoint) {
        let (observer, child_end) = terminal::checkpoints().unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(pty.slave().try_clone().unwrap())
            .stdout(child_end.into_stdio())
            .stderr(std::process::Stdio::inherit());
        let (child, group) = crate::process::spawn_with_pgid(&mut command, &Placement::NewLeader)
            .expect("spawn a private mode-changing group");
        drop(command); // Do not keep the child's checkpoint endpoint open.
        let Group::Owns(group) = group else {
            panic!("NewLeader must own its fresh group");
        };
        (
            Self {
                child: Some(ChildHandle::from_std(child)),
                group,
            },
            observer,
        )
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.group.kill();
            let _ = child.kill();
            child.reap().expect("reap the killed mode-changing child");
        }
    }
}

impl Drop for ModeChild {
    fn drop(&mut self) {
        self.stop();
    }
}

fn successful_loan(pty: &PrivatePty, foreign_foreground: bool) {
    let before = pty.cooked_modes();
    let helper_group = rustix::process::getpgrp();
    let mut prior = if foreign_foreground {
        let (child, mut ready) =
            ModeChild::spawn(pty, "set -e; printf R; IFS= read -r release <&1");
        ready.expect(b'R').unwrap();
        // The fixture blocks SIGTTOU on this helper thread. The captured
        // foreground belongs to a live child, not the helper's own group.
        tcsetpgrp(pty.slave(), child.group.as_pid()).unwrap();
        assert_ne!(child.group.as_pid(), helper_group);
        Some((child, ready))
    } else {
        None
    };
    let captured_foreground = tcgetpgrp(pty.slave()).unwrap();
    let home = DurableRoot::default().child();
    let prepared = prepared(&home);
    let terminal; // Drop the child before restoring, including on panic.
    let (mut child, mut checkpoint) = ModeChild::spawn(
        pty,
        "set -e; printf R; IFS= read -r release <&1; \
         stty -echo -icanon; printf M; IFS= read -r release <&1",
    );
    checkpoint.expect(b'R').unwrap();
    terminal = prepared
        .lend(child.group)
        .unwrap_or_else(|(_, error)| panic!("real handoff refused: {error}"));
    assert_eq!(tcgetpgrp(pty.slave()).unwrap(), child.group.as_pid());
    checkpoint.release_line().unwrap();
    checkpoint.expect(b'M').unwrap();
    let changed = tcgetattr(pty.slave()).unwrap();
    assert!(
        !changed
            .local_modes
            .intersects(LocalModes::ECHO | LocalModes::ICANON)
    );
    child.stop();
    terminal.reclaim(None);
    assert_eq!(tcgetpgrp(pty.slave()).unwrap(), captured_foreground);
    terminal::assert_modes(&tcgetattr(pty.slave()).unwrap(), &before);
    assert_eq!(home.cause(), None);
    if let Some((prior, _checkpoint)) = &mut prior {
        // Stop naming the prior group's ID before its leader can be reaped.
        tcsetpgrp(pty.slave(), helper_group).unwrap();
        prior.stop();
    }
}

#[test]
fn a_successful_loan_restores_modes_and_foreground() {
    terminal::isolated(
        "process::terminal::loan::tests::unix::a_successful_loan_restores_modes_and_foreground",
        Mode::Controlling,
        |pty| successful_loan(pty, false),
    );
}

#[test]
fn a_successful_loan_restores_a_foreground_other_than_the_helpers_group() {
    terminal::isolated(
        "process::terminal::loan::tests::unix::a_successful_loan_restores_a_foreground_other_than_the_helpers_group",
        Mode::Controlling,
        |pty| successful_loan(pty, true),
    );
}

#[test]
fn a_refused_handoff_keeps_modes_guarded_until_after_the_child_is_reaped() {
    terminal::isolated(
        "process::terminal::loan::tests::unix::a_refused_handoff_keeps_modes_guarded_until_after_the_child_is_reaped",
        Mode::NonControlling,
        |pty| {
            let before = pty.cooked_modes();
            let home = DurableRoot::default().child();
            let mut retained = Some(prepared(&home));
            let (mut child, mut checkpoint) = ModeChild::spawn(
                pty,
                "set -e; stty -echo; printf A; IFS= read -r release <&1; \
                 stty -icanon; printf B; IFS= read -r release <&1",
            );
            checkpoint.expect(b'A').unwrap();
            let first = tcgetattr(pty.slave()).unwrap();
            assert!(!first.local_modes.contains(LocalModes::ECHO));
            assert!(first.local_modes.contains(LocalModes::ICANON));
            let error = match retained.take().unwrap().lend(child.group) {
                Err((prepared, error)) => {
                    retained = Some(prepared);
                    error
                }
                Ok(terminal) => {
                    child.stop();
                    drop(terminal);
                    panic!("a non-controlling PTY cannot hand off foreground");
                }
            };
            assert_eq!(error.raw_os_error(), Some(libc::ENOTTY));
            terminal::assert_modes(&tcgetattr(pty.slave()).unwrap(), &first);
            checkpoint.release_line().unwrap();
            checkpoint.expect(b'B').unwrap();
            let second = tcgetattr(pty.slave()).unwrap();
            assert!(
                !second
                    .local_modes
                    .intersects(LocalModes::ECHO | LocalModes::ICANON)
            );
            // A further write happened after refusal; nothing restored early.
            child.stop();
            terminal::assert_modes(&tcgetattr(pty.slave()).unwrap(), &second);
            drop(retained);
            terminal::assert_modes(&tcgetattr(pty.slave()).unwrap(), &before);
            assert_eq!(home.cause(), None);
        },
    );
}

#[test]
fn guarded_reclaim_restores_modes_without_hearing_any_gesture() {
    terminal::isolated(
        "process::terminal::loan::tests::unix::guarded_reclaim_restores_modes_without_hearing_any_gesture",
        Mode::Controlling,
        |pty| {
            let before = pty.cooked_modes();
            let foreground = tcgetpgrp(pty.slave()).unwrap();
            let home = DurableRoot::default().child();
            let mut terminal = prepared(&home).guard();
            assert!(matches!(terminal.0, Hold::Guarded(_)));
            let mut changed = before.clone();
            changed.make_raw();
            tcsetattr(pty.slave(), OptionalActions::Now, &changed).unwrap();
            terminal.hear(Signal::new(libc::SIGINT));
            assert_eq!(terminal.pressed(), None);
            assert_eq!(home.cause(), None);
            terminal.reclaim(Some(Signal::new(libc::SIGQUIT)));
            assert_eq!(home.cause(), None);
            assert_eq!(tcgetpgrp(pty.slave()).unwrap(), foreground);
            terminal::assert_modes(&tcgetattr(pty.slave()).unwrap(), &before);
        },
    );
}
