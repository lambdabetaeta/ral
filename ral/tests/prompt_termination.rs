#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(clippy::disallowed_methods)]

mod common;

use common::pty::PtySession;
use common::ral_command;
use rustix::termios::{LocalModes, tcgetattr};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(15);

fn command(dir: &TempDir, surface: &str) -> Command {
    let mut cmd = ral_command();
    cmd.args(["-i", "--norc", "--surface", surface])
        .env("TERM", "xterm-256color")
        .env("RAL_INTERACTIVE_MODE", "full")
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path());
    cmd
}

fn prompt(session: &mut PtySession, count: usize) {
    assert!(
        session.wait_until(WAIT, |text| {
            if text.contains("\x1b[?2004h") {
                text.matches("\x1b[?2004h").count() >= count && text.contains('❯')
            } else {
                text.matches('❯').count() >= count
            }
        }),
        "no prompt: {}",
        session.text()
    );
}

fn paste_line(session: &mut PtySession, line: &str) {
    for byte in format!("\x1b[200~{line}\x1b[201~").bytes() {
        session.send_byte(byte).unwrap();
    }
    assert!(
        session.wait_until(WAIT, |text| text.contains(line)),
        "{}",
        session.text()
    );
    session.send_byte(b'\n').unwrap();
}

fn exit(child: &mut Child) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ral required more input to finish shutting down");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn signal(child: &Child, signal: i32) {
    assert_eq!(unsafe { libc::kill(child.id().cast_signed(), signal) }, 0);
}

fn exit_pty(session: &mut PtySession) -> ExitStatus {
    let start = Instant::now();
    loop {
        session.read_available();
        if let Some(status) = session.child.try_wait().unwrap() {
            return status;
        }
        assert!(
            start.elapsed() < WAIT,
            "ral did not exit: {}",
            session.text()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn idle_termination_saves_history() {
    for surface in ["minimal", "readline"] {
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            let dir = TempDir::new().unwrap();
            let mut session = PtySession::spawn_command(command(&dir, surface)).unwrap();
            prompt(&mut session, 1);
            let editing = tcgetattr(&session.terminal).unwrap();
            assert_eq!(
                editing.local_modes.contains(LocalModes::ICANON),
                surface == "minimal"
            );
            if surface == "readline" {
                paste_line(&mut session, "echo remembered-before-shutdown");
            } else {
                session
                    .send_line("echo remembered-before-shutdown")
                    .unwrap();
            }
            prompt(&mut session, 2);
            signal(&session.child, sig);
            assert_eq!(
                exit_pty(&mut session).code(),
                Some(if sig == libc::SIGQUIT { 131 } else { 143 })
            );
            let history = std::fs::read_to_string(dir.path().join("ral/history")).unwrap();
            assert!(
                history.contains("echo remembered-before-shutdown"),
                "{history}"
            );
        }
    }
}

#[test]
fn readline_termination_survives_an_inherited_sigint_mask() {
    let dir = TempDir::new().unwrap();
    let mut cmd = command(&dir, "readline");
    unsafe {
        cmd.pre_exec(|| {
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, libc::SIGINT);
            let error =
                libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut());
            if error == 0 {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(error))
            }
        });
    }
    let mut session = PtySession::spawn_command(cmd).unwrap();
    prompt(&mut session, 1);
    assert!(
        !tcgetattr(&session.terminal)
            .unwrap()
            .local_modes
            .contains(LocalModes::ICANON)
    );
    signal(&session.child, libc::SIGTERM);
    assert_eq!(exit_pty(&mut session).code(), Some(143));
}

#[test]
fn termination_interrupts_unfinished_editor_input() {
    for input in [b"\x1b[200~unfinished paste".as_slice(), b"\x16", b"\xc3"] {
        let dir = TempDir::new().unwrap();
        let mut session = PtySession::spawn_command(command(&dir, "readline")).unwrap();
        prompt(&mut session, 1);
        for &byte in input {
            session.send_byte(byte).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        signal(&session.child, libc::SIGTERM);
        assert_eq!(exit_pty(&mut session).code(), Some(143));
    }
}

#[test]
fn minimal_continuation_can_be_terminated() {
    let dir = TempDir::new().unwrap();
    let mut session = PtySession::spawn_command(command(&dir, "minimal")).unwrap();
    prompt(&mut session, 1);
    session.send_line("echo 'unfinished").unwrap();
    assert!(
        session.wait_until(WAIT, |text| text.contains("> ")),
        "{}",
        session.text()
    );
    session.send_byte(0xc3).unwrap();
    signal(&session.child, libc::SIGQUIT);
    assert_eq!(exit_pty(&mut session).code(), Some(131));
    session.read_available();
    assert!(
        !session.text().contains("invalid utf-8"),
        "{}",
        session.text()
    );
}

#[test]
fn ctrl_c_still_leaves_the_readline_session_usable() {
    let dir = TempDir::new().unwrap();
    let mut session = PtySession::spawn_command(command(&dir, "readline")).unwrap();
    prompt(&mut session, 1);
    session.send_ctrl_c().unwrap();
    prompt(&mut session, 2);
    paste_line(&mut session, "exit 7");
    assert_eq!(exit_pty(&mut session).code(), Some(7));
}

#[test]
fn piped_input_can_be_terminated_without_eof() {
    let dir = TempDir::new().unwrap();
    let mut child = command(&dir, "readline")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let marker = dir.path().join("ready");
    writeln!(input, "echo ready > '{}'", marker.display()).unwrap();
    let start = Instant::now();
    while !marker.exists() {
        if start.elapsed() > WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("piped REPL never evaluated its first line");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    signal(&child, libc::SIGTERM);
    assert_eq!(exit(&mut child).code(), Some(143));
    drop(input);
}

#[test]
fn termination_restores_modes_and_hands_the_terminal_back() {
    for surface in ["minimal", "readline"] {
        let dir = TempDir::new().unwrap();
        let mut parent = Command::new("/bin/sh");
        parent
            .args([
                "-c",
                r#"
            "$@" <&0 &
            ral_pid=$!
            printf 'ral-pid:%s\n' "$ral_pid"
            wait "$ral_pid"
            status=$?
            printf 'ral-ended\n'
            read acknowledgement
            exit "$status"
        "#,
                "parent",
            ])
            .arg(common::ral_bin())
            .args(["-i", "--norc", "--surface", surface])
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("RAL_INTERACTIVE_MODE", "full")
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path());
        let mut session = PtySession::spawn_command(parent).unwrap();
        prompt(&mut session, 1);
        let pid: i32 = session
            .text()
            .lines()
            .find_map(|line| line.strip_prefix("ral-pid:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        assert!(
            session.wait_until(WAIT, |text| text.contains("ral-ended")),
            "{}",
            session.text()
        );
        assert_eq!(session.foreground_pgid(), Some(session.pid()));
        let restored = tcgetattr(&session.terminal).unwrap();
        assert!(
            restored
                .local_modes
                .contains(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG)
        );
        session.send_line("done").unwrap();
        assert_eq!(exit_pty(&mut session).code(), Some(143));
    }
}

#[test]
fn minimal_input_restores_blocking_reads_for_foreground_commands() {
    let dir = TempDir::new().unwrap();
    let mut session = PtySession::spawn_command(command(&dir, "minimal")).unwrap();
    prompt(&mut session, 1);
    session.send_line("/bin/cat").unwrap();
    assert!(
        session.wait_for_lent_terminal(WAIT).is_some(),
        "{}",
        session.text()
    );
    session.send_line("foreground-input").unwrap();
    assert!(
        session.wait_until(WAIT, |text| text.matches("foreground-input").count() == 2),
        "{}",
        session.text()
    );
    session.send_byte(4).unwrap();
    prompt(&mut session, 2);
    session.send_line("exit 0").unwrap();
    assert_eq!(exit_pty(&mut session).code(), Some(0));
}
