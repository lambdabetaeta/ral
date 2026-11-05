#![cfg(unix)]
#![allow(clippy::disallowed_methods, reason = "test code")]

mod common;

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const TARGET: &str = "mock target.invalid";
const PORT: &str = "43210";
const BUDGET: Duration = Duration::from_secs(15);
const CLEANUP_RESERVE: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(10);

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn fifo(path: &Path) -> File {
    use std::os::unix::ffi::OsStrExt as _;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .unwrap()
}

fn executable(path: &Path, source: &str) {
    std::fs::write(path, source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct PortRun {
    child: Child,
    status: Option<ExitStatus>,
    dir: tempfile::TempDir,
    deadline: Instant,
}

impl PortRun {
    fn pids(&self) -> Vec<i32> {
        std::fs::read_to_string(self.dir.path().join("pids"))
            .unwrap_or_default()
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .filter_map(|line| line.trim().parse().ok())
            .filter(|pid| *pid > 1)
            .collect()
    }

    fn poll(&mut self) {
        if self.status.is_none() {
            self.status = self.child.try_wait().unwrap();
        }
    }

    fn tick(&self) {
        assert!(
            self.deadline.saturating_duration_since(Instant::now()) > CLEANUP_RESERVE,
            "wait-for-port exceeded its harness deadline; stderr: {}",
            std::fs::read_to_string(self.dir.path().join("stderr")).unwrap_or_default()
        );
        std::thread::sleep(TICK);
    }
}

impl Drop for PortRun {
    fn drop(&mut self) {
        // Keep ral alive long enough to reap killed mocks, including late starts.
        if self.status.is_none() {
            unsafe { libc::kill(self.child.id().cast_signed(), libc::SIGINT) };
        }
        loop {
            for pid in self.pids().into_iter().filter(|pid| alive(*pid)) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            if self.status.is_none() {
                self.status = self.child.try_wait().ok().flatten();
            }
            if self.status.is_some() && !self.pids().into_iter().any(alive) {
                break;
            }
            if self.deadline.saturating_duration_since(Instant::now()) <= Duration::from_millis(500)
            {
                let _ = self.child.kill();
            }
            if Instant::now() >= self.deadline {
                break;
            }
            std::thread::sleep(TICK);
        }
        if self.status.is_none() {
            let _ = self.child.kill();
            self.status = self.child.wait().ok();
        }
    }
}

fn run_wait_for_port(mode: &str) -> (i32, String, String, usize) {
    let deadline = Instant::now() + BUDGET;
    let dir = tempfile::tempdir().unwrap();
    let mut gate = fifo(&dir.path().join("alarm-gate"));
    executable(
        &dir.path().join("nc"),
        r#"#!/bin/sh
printf '%s\n' "$$" >> "$RAL_PORT_FIXTURE/pids"
printf '%s\n' "$@" >> "$RAL_PORT_FIXTURE/argv"
printf '%s\n' "$$" > "$RAL_PORT_FIXTURE/nc-pid"
: > "$RAL_PORT_FIXTURE/probe-ready"
case "$RAL_PORT_PROBE" in
    success) exit 0 ;;
    refuse) exit 1 ;;
    block) exec /bin/sleep 300 ;;
    *) exit 99 ;;
esac
"#,
    );
    executable(
        &dir.path().join("sleep"),
        r#"#!/bin/sh
printf '%s\n' "$$" >> "$RAL_PORT_FIXTURE/pids"
if [ "$#" = 1 ] && [ "$1" = 2 ]; then
    : > "$RAL_PORT_FIXTURE/alarm-ready"
    read -r release < "$RAL_PORT_FIXTURE/alarm-gate"
fi
exec /bin/sleep "$@"
"#,
    );
    // A handler reaches the alarm mock even with bundled coreutils.
    let example =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/wait-for-port/wait-for-port.ral");
    let wrapper = dir.path().join("run.ral");
    std::fs::write(
        &wrapper,
        format!(
            "within [handlers: [sleep: {{ |argv| /bin/sh {} ...$argv }}]] {{ let _ = use {} }}\n",
            ral_core::syntax::quote_word(&ral_core::path::shown(dir.path().join("sleep"))),
            ral_core::syntax::quote_word(&ral_core::path::shown(&example)),
        ),
    )
    .unwrap();
    let stdout = File::create(dir.path().join("stdout")).unwrap();
    let stderr = File::create(dir.path().join("stderr")).unwrap();
    let path =
        std::env::join_paths([dir.path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let child = common::ral_command()
        .arg("--norc")
        .arg(wrapper)
        .args([TARGET, PORT, "2"])
        .env("PATH", path)
        .env("RAL_PORT_FIXTURE", dir.path())
        .env("RAL_PORT_PROBE", mode)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .expect("spawn wait-for-port");
    let mut run = PortRun {
        child,
        status: None,
        dir,
        deadline,
    };

    if mode != "success" {
        // The alarm cannot start its budget before a real mock probe is up.
        while !run.dir.path().join("probe-ready").exists()
            || !run.dir.path().join("alarm-ready").exists()
        {
            run.poll();
            assert!(run.status.is_none(), "example exited before readiness");
            run.tick();
        }
        if mode == "block" {
            let pid = std::fs::read_to_string(run.dir.path().join("nc-pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(alive(pid), "blocked nc never established a live child");
        }
        gate.write_all(b"go\n").unwrap();
    }

    loop {
        run.poll();
        if run.status.is_some() && !run.pids().into_iter().any(alive) {
            break;
        }
        run.tick();
    }
    // Files avoid output-draining threads whose joins could outlive the deadline.
    let stdout = std::fs::read_to_string(run.dir.path().join("stdout")).unwrap();
    let stderr = std::fs::read_to_string(run.dir.path().join("stderr")).unwrap();
    let argv = std::fs::read_to_string(run.dir.path().join("argv")).unwrap();
    let words = argv.lines().collect::<Vec<_>>();
    let expected = ["-z", "-w", "1", "--", TARGET, PORT];
    assert!(!words.is_empty(), "no nc invocation observed");
    assert_eq!(words.len() % expected.len(), 0, "incomplete argv: {argv:?}");
    for probe in words.chunks(expected.len()) {
        assert_eq!(
            probe, expected,
            "nc options or target/port forwarding changed"
        );
    }
    (
        run.status
            .unwrap()
            .code()
            .expect("wait-for-port killed by a signal"),
        stdout,
        stderr,
        words.len() / expected.len(),
    )
}

#[test]
fn wait_for_port_reports_up_for_a_successful_mock_probe() {
    let (status, stdout, stderr, probes) = run_wait_for_port("success");
    assert_eq!(status, 0, "{stderr}");
    assert_eq!(stdout, format!("{TARGET}:{PORT} is up\n"));
    assert_eq!(probes, 1);
}

#[test]
fn wait_for_port_retries_refusal_until_the_outer_deadline() {
    let (status, stdout, stderr, probes) = run_wait_for_port("refuse");
    assert_eq!(status, 1, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(probes >= 2, "persistent refusal did not retry");
    assert!(
        stderr.contains(&format!("{TARGET}:{PORT} never came up within 2s")),
        "{stderr}"
    );
}

#[test]
fn wait_for_port_cancels_and_drains_a_started_blocked_probe() {
    let (status, stdout, stderr, probes) = run_wait_for_port("block");
    assert_eq!(status, 1, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert_eq!(
        probes, 1,
        "the first probe should remain blocked until cancellation"
    );
    assert!(
        stderr.contains(&format!("{TARGET}:{PORT} never came up within 2s")),
        "{stderr}"
    );
}
