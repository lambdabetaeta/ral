#![allow(clippy::disallowed_methods, reason = "test fixtures")]
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::pty::PtySession;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(15);

fn send(session: &mut PtySession, text: &str) {
    for byte in text.bytes() {
        session.send_byte(byte).unwrap();
    }
}

fn prompt(session: &mut PtySession, count: usize) {
    assert!(
        session.wait_until(WAIT, |text| text.matches("\x1b[?2004h").count() >= count),
        "prompt {count} did not return: {:?}",
        session.text()
    );
}

#[derive(Clone, Copy)]
enum EditorExit {
    Success,
    RapidSuccess,
    Failure,
    Interrupt,
}

fn editor_handoff(gesture: &str, exit: EditorExit) {
    let succeeds = matches!(exit, EditorExit::Success | EditorExit::RapidSuccess);
    let dir = tempfile::tempdir().unwrap();
    let editor = dir.path().join("my editor");
    std::fs::write(
        &editor,
        format!(
            "#!/bin/sh\n{}[ \"$1\" = --wait ] || exit 9\n[ -t 0 ] && [ -t 1 ] || exit 10\nprintf '^touch submitted\\n' > \"$2\"\nstty -onlcr intr '^]' || exit 11\n{}\n",
            if matches!(exit, EditorExit::RapidSuccess) {
                "trap '' TTOU\nstty -onlcr intr '^]' || exit 12\n"
            } else {
                ""
            },
            match exit {
                EditorExit::Success | EditorExit::RapidSuccess => "exit 0",
                EditorExit::Failure => "exit 7",
                EditorExit::Interrupt => "printf EDITOR_MODES_CHANGED; exec sleep 60",
            }
        ),
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new(common::ral_bin());
    command
        .args(["-i", "--norc", "--surface", "readline"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path())
        .env("XDG_STATE_HOME", dir.path())
        .env("VISUAL", format!("'{}' --wait", editor.display()))
        .env("TERM", "xterm-256color")
        .env("RAL_INTERACTIVE_MODE", "full");
    let mut session = PtySession::spawn_command(command).unwrap();
    prompt(&mut session, 1);
    let before = session.termios();
    send(&mut session, "^touch original");
    send(&mut session, gesture);
    if matches!(exit, EditorExit::Interrupt) {
        assert!(session.wait_until(WAIT, |text| text.contains("EDITOR_MODES_CHANGED")));
        assert_ne!(session.termios().output_modes, before.output_modes);
        session.send_byte(0x1d).unwrap();
    }
    prompt(&mut session, 2);
    let after = session.termios();
    assert_eq!(after.output_modes, before.output_modes);
    let intr = rustix::termios::SpecialCodeIndex::VINTR;
    assert_eq!(after.special_codes[intr], before.special_codes[intr]);
    assert!(!dir.path().join("original").exists());
    assert!(!dir.path().join("submitted").exists());
    if matches!(exit, EditorExit::Failure) {
        assert!(session.text().contains("external editor"));
    }
    send(&mut session, "\r");
    prompt(&mut session, 3);
    let expected = if succeeds { "submitted" } else { "original" };
    assert!(
        dir.path().join(expected).exists(),
        "explicit Enter did not execute restored draft: {:?}",
        session.text()
    );
    assert!(
        !dir.path()
            .join(if succeeds { "original" } else { "submitted" })
            .exists()
    );
}

#[test]
fn ctrl_g_edits_without_submitting() {
    editor_handoff("\x07", EditorExit::Success);
}

#[test]
fn rapid_editor_ignoring_sigttou_restores_pre_spawn_modes() {
    // Exercise immediate mutation without the kernel stopping a background editor.
    for _ in 0..16 {
        editor_handoff("\x07", EditorExit::RapidSuccess);
    }
}

#[test]
fn ctrl_x_ctrl_e_edits_without_submitting() {
    editor_handoff("\x18\x05", EditorExit::Success);
}

#[test]
fn editor_failure_restores_original_without_submitting() {
    editor_handoff("\x07", EditorExit::Failure);
}

#[test]
fn editor_interrupt_restores_modes_and_original_without_submitting() {
    editor_handoff("\x07", EditorExit::Interrupt);
}
