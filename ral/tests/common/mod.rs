#![allow(
    clippy::disallowed_macros,
    reason = "[test] libtest captures print macros"
)]

//! Shared scaffolding for `ral` integration tests.
//!
//! Locates the `ral` binary built by cargo, generates a fresh temp script
//! path, spawns the binary on it, captures stdout/stderr, and propagates
//! the exit code.  The helpers below collect that scaffolding in one place.
//!
//! Cargo treats `tests/common/mod.rs` as a module rather than its own
//! integration test target — that is why it lives here and not in
//! `tests/common.rs`.

#![allow(dead_code)] // not every test file uses every helper

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod pty;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// `stderr` without debug-build trace lines (`[[DEBUG] …`).
pub fn diagnostics(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|l| !l.starts_with("[[DEBUG] "))
        .collect::<Vec<_>>()
        .join("\n")
}

pub use ral_core::test_helper::confinement::Output;

#[track_caller]
fn denied_by(
    proof: &ral_core::test_helper::confinement::Confining,
    control: &Output,
    out: &Output,
    marker: &str,
    attribution: &str,
) {
    ral_core::test_helper::confinement::denied(proof, control, out, marker);
    assert_ne!(out.status, 0, "grant refusal must fail: {out:?}");
    assert!(
        out.stderr.contains(attribution),
        "expected {attribution:?}: {out:?}"
    );
}

#[track_caller]
pub fn denied_by_grant(
    proof: &ral_core::test_helper::confinement::Confining,
    control: &Output,
    out: &Output,
    marker: &str,
) {
    denied_by(proof, control, out, marker, "denied by grant");
}

#[track_caller]
pub fn denied_by_active_grant(
    proof: &ral_core::test_helper::confinement::Confining,
    control: &Output,
    out: &Output,
    marker: &str,
) {
    denied_by(proof, control, out, marker, "denied by active grant");
}

#[ctor::ctor(unsafe)]
fn init_test_binary() {
    if let Some(code) = ral_core::invocation::serve_process_before_main(&[]) {
        std::process::exit(i32::from(code));
    }
}

/// Path to the `ral` binary cargo built for this test target.
///
/// `CARGO_BIN_EXE_ral` is provided by cargo to integration tests in the
/// same crate as the binary, so this stays correct under any
/// `CARGO_TARGET_DIR` (worktrees, dev containers, custom configs).
pub fn ral_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ral"))
}

/// A fresh `Command` for the `ral` binary, its `PATH` pinned to the
/// platform's own binaries rather than inherited from the runner.
///
/// Script fixtures bind short names (`let n = …`, `let x = …`) that ral's
/// value/command disjointness rule checks against the live `PATH`; inheriting
/// the ambient one makes every such test a hostage to whatever the host
/// happens to have installed — a Node version manager's `n`, X11's `x`, and
/// so on, different on every machine and every runner image. Pinning it here
/// makes the whole suite deterministic. Unix only: every bare command these
/// tests invoke (`grep`, `cat`, `head`, `sh`, …) lives in `/usr/bin` or
/// `/bin` on both Linux and macOS; Windows' own `PATH` conventions are left
/// untouched, since nothing here has needed hardening against them yet.
pub fn ral_command() -> Command {
    let mut cmd = Command::new(ral_bin());
    if cfg!(unix) {
        cmd.env("PATH", "/usr/bin:/bin");
    }
    cmd
}

static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(1);

/// Build a unique temp file path of the form `<prefix>_<pid>_<id>.<ext>`.
pub fn fresh_tmp_path(prefix: &str, ext: &str) -> PathBuf {
    let mut tmp = std::env::temp_dir();
    let pid = std::process::id();
    let id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
    tmp.push(format!("{prefix}_{pid}_{id}.{ext}"));
    tmp
}

/// Write `script` to a fresh temp file, run `ral <file>`, return captured I/O.
/// stdin is `/dev/null`.  The temp file is removed once the child exits.
pub fn run(prefix: &str, script: &str) -> Output {
    let tmp = fresh_tmp_path(prefix, "ral");
    std::fs::write(&tmp, script).unwrap();

    let child = ral_command()
        .arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");

    let out = child.wait_with_output().unwrap();
    std::fs::remove_file(&tmp).ok();

    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

/// Like [`run`], but with `envs` overlaid on the inherited environment — for
/// the tests whose subject *is* a variable the runner also sets, `PATH` above
/// all.
pub fn run_with_env(prefix: &str, envs: &[(&str, &str)], script: &str) -> Output {
    let tmp = fresh_tmp_path(prefix, "ral");
    std::fs::write(&tmp, script).unwrap();

    let mut cmd = ral_command();
    cmd.arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }

    let out = cmd.spawn().expect("spawn ral").wait_with_output().unwrap();
    std::fs::remove_file(&tmp).ok();

    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

/// Like [`run`], but feeds `stdin_data` to the child instead of an empty
/// stdin.
pub fn run_with_stdin(prefix: &str, script: &str, stdin_data: &[u8]) -> Output {
    let tmp = fresh_tmp_path(prefix, "ral");
    std::fs::write(&tmp, script).unwrap();

    let mut child = ral_command()
        .arg(&tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");

    if !stdin_data.is_empty() {
        child.stdin.take().unwrap().write_all(stdin_data).unwrap();
    }

    let out = child.wait_with_output().unwrap();
    std::fs::remove_file(&tmp).ok();

    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

/// Write `script` to a fresh temp file and spawn `ral <args> <file>` on it,
/// stdin null and both output streams piped; the caller removes the file.
pub fn spawn_script(prefix: &str, args: &[&str], script: &str) -> (Child, PathBuf) {
    let tmp = fresh_tmp_path(prefix, "ral");
    std::fs::write(&tmp, script).unwrap();
    let child = ral_command()
        .args(args)
        .arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");
    (child, tmp)
}

/// Collect `child`'s output, or give up (returning `None`) if it hasn't
/// exited within `timeout`, killing it first so nothing is left running.
/// Used by tests guarding against a hang rather than a wrong answer.
pub fn wait_with_timeout(mut child: Child, timeout: Duration) -> Option<Output> {
    use std::io::Read;

    fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    }
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);

    let start = std::time::Instant::now();
    loop {
        match child.try_wait().unwrap() {
            Some(status) => {
                let text = |h: Option<std::thread::JoinHandle<String>>| {
                    h.and_then(|h| h.join().ok()).unwrap_or_default()
                };
                return Some(Output {
                    stdout: text(stdout),
                    stderr: text(stderr),
                    status: status.code().unwrap_or(1),
                });
            }
            None if start.elapsed() > timeout => {
                child.kill().ok();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// [`spawn_script`] then [`wait_with_timeout`]: `args` ahead of the script
/// path, `None` if the child hung.  The temp file is removed either way.
pub fn run_with_timeout(
    prefix: &str,
    args: &[&str],
    script: &str,
    timeout: Duration,
) -> Option<Output> {
    let (child, tmp) = spawn_script(prefix, args, script);
    let out = wait_with_timeout(child, timeout);
    std::fs::remove_file(&tmp).ok();
    out
}

/// Every `.ral` file under `dir`, sorted, never entering a directory named in
/// `prune`.
pub fn discover(dir: &Path, prune: &[&str]) -> Vec<PathBuf> {
    let mut scripts = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !path
                .file_name()
                .is_some_and(|n| prune.iter().any(|p| n == *p))
            {
                scripts.extend(discover(&path, prune));
            }
        } else if path.extension().is_some_and(|e| e == "ral") {
            scripts.push(path);
        }
    }
    scripts.sort();
    scripts
}

/// A canonical scratch directory, removed on drop.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Self {
        let dir = fresh_tmp_path(tag, "d");
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(&dir).unwrap())
    }

    pub fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An executable `#!/bin/sh` script at `path` printing `ran`, its directory
/// made.
#[cfg(unix)]
pub fn script(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "#!/bin/sh\necho ran\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `paths` as a ral list body: each an `absolute` place, comma-separated.
pub fn places(paths: &[&Path]) -> String {
    (paths.iter())
        .map(|p| format!("`absolute '{}'", ral_core::path::shown(p)))
        .collect::<Vec<_>>()
        .join(", ")
}
