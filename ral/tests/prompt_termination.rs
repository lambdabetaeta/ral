#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(any(target_os = "linux", target_os = "macos"))]

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
        .env("XDG_CONFIG_HOME", dir.path())
        .env("XDG_STATE_HOME", dir.path());
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
    session.send_byte(b'\r').unwrap();
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
    session.wait_for_exit(WAIT).unwrap()
}

#[test]
fn terminal_exit_wait_is_bounded_and_drains_output() {
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        "printf ready; read acknowledgement; /usr/bin/head -c 131072 /dev/zero; exit 7",
    ]);
    let mut session = PtySession::spawn_command(cmd).unwrap();
    assert!(session.wait_until(WAIT, |text| text.contains("ready")));
    assert_eq!(
        session
            .wait_for_exit(Duration::from_millis(50))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    session.send_line("continue").unwrap();
    assert_eq!(exit_pty(&mut session).code(), Some(7));
    assert!(session.text().len() >= 131_072);
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
fn termination_survives_an_inherited_sigint_mask() {
    for surface in ["minimal", "readline"] {
        let dir = TempDir::new().unwrap();
        let mut cmd = command(&dir, surface);
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
        assert_eq!(
            tcgetattr(&session.terminal)
                .unwrap()
                .local_modes
                .contains(LocalModes::ICANON),
            surface == "minimal"
        );
        signal(&session.child, libc::SIGTERM);
        assert_eq!(exit_pty(&mut session).code(), Some(143));
    }
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
fn ctrl_c_still_leaves_the_session_usable() {
    for surface in ["minimal", "readline"] {
        let dir = TempDir::new().unwrap();
        let mut session = PtySession::spawn_command(command(&dir, surface)).unwrap();
        prompt(&mut session, 1);
        session.send_ctrl_c().unwrap();
        prompt(&mut session, 2);
        if surface == "readline" {
            paste_line(&mut session, "exit 7");
        } else {
            session.send_line("exit 7").unwrap();
        }
        assert_eq!(exit_pty(&mut session).code(), Some(7));
    }
}

#[test]
fn minimal_ctrl_c_abandons_primary_and_continuation_without_enter() {
    for first in [
        "",
        "echo abandoned-primary",
        "echo abandoned-block; {\necho abandoned-body",
    ] {
        let dir = TempDir::new().unwrap();
        let mut session = PtySession::spawn_command(command(&dir, "minimal")).unwrap();
        prompt(&mut session, 1);
        for byte in first.bytes() {
            session.send_byte(byte).unwrap();
        }
        if first.contains('\n') {
            assert!(
                session.wait_until(WAIT, |text| text.contains("> ")),
                "{}",
                session.text()
            );
        }
        session.send_ctrl_c().unwrap();
        // No Enter: this also races the prompt's flush against read entry.
        prompt(&mut session, 2);
        session.send_line("echo fresh-command").unwrap();
        prompt(&mut session, 3);
        let text = session.text();
        assert_eq!(
            text.lines().filter(|line| *line == "fresh-command").count(),
            1,
            "{text}"
        );
        assert!(
            !text.lines().any(|line| line.starts_with("abandoned-")),
            "{text}"
        );
        session.send_byte(4).unwrap();
        assert_eq!(exit_pty(&mut session).code(), Some(0));
    }
}

#[test]
fn minimal_eof_executes_a_final_unterminated_line_once() {
    let dir = TempDir::new().unwrap();
    let mut child = command(&dir, "minimal")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"echo final-line")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .matches("final-line")
            .count(),
        1
    );
}

#[test]
fn minimal_input_preserves_the_parents_descriptor_flags() {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    for nonblocking in [false, true] {
        let dir = TempDir::new().unwrap();
        let (read, mut write) = ral_core::process::cloexec_pipe().unwrap();
        if nonblocking {
            fcntl_setfl(&read, fcntl_getfl(&read).unwrap() | OFlags::NONBLOCK).unwrap();
        }
        let flags = fcntl_getfl(&read).unwrap();
        let mut child = command(&dir, "minimal")
            .stdin(read.try_clone().unwrap())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let marker = dir.path().join("ready");
        writeln!(write, "echo ready > '{}'", marker.display()).unwrap();
        let start = Instant::now();
        while !marker.exists() {
            if start.elapsed() > WAIT {
                let _ = child.kill();
                let _ = child.wait();
                panic!("minimal REPL never evaluated its first line");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut observed = Vec::new();
        for _ in 0..20 {
            observed.push(fcntl_getfl(&read).unwrap());
            std::thread::sleep(Duration::from_millis(10));
        }
        signal(&child, libc::SIGTERM);
        assert_eq!(exit(&mut child).code(), Some(143));
        assert!(
            observed.iter().all(|&actual| actual == flags),
            "{observed:?}"
        );
        assert_eq!(fcntl_getfl(&read).unwrap(), flags);
    }
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
            exec 3<&0
            "$@" <&3 &
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
        assert_eq!(
            unsafe { libc::kill(pid, libc::SIGTERM) },
            0,
            "{}: {}",
            std::io::Error::last_os_error(),
            session.text()
        );
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
fn minimal_input_leaves_blocking_reads_for_foreground_commands() {
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
