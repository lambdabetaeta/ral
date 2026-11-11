//! Exact-test re-entry for tests that must not capture the runner's terminal.
//!
//! Unix helpers start a new session with no controlling terminal; every helper
//! starts with null stdin and file-backed stdout/stderr. A caller that needs a
//! tty uses `terminal::isolated`, which installs one only in the helper.

use std::fs::File;
use std::io::{self, Read, Seek};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const HELPER: &str = "RAL_ISOLATED_TEST";
const DONE: &str = "RAL_ISOLATED_TEST_DONE";
const WAIT: Duration = Duration::from_secs(30);

/// [`isolated`] for the enclosing test `$test`, named as libtest names it.
#[macro_export]
macro_rules! isolated {
    ($test:ident, $check:expr) => {
        $crate::test_helper::subprocess::isolated(
            &$crate::test_helper::subprocess::exact_name(module_path!(), stringify!($test)),
            $check,
        )
    };
}

/// libtest's name for `test` in `module`: the module path without its crate.
#[must_use]
pub fn exact_name(module: &str, test: &str) -> String {
    match module.split_once("::") {
        Some((_, path)) => format!("{path}::{test}"),
        None => test.to_owned(),
    }
}

/// Re-enter this exact test with null stdin and captured diagnostics.
/// A completion marker rejects misspelled test names; helper exit is bounded.
/// Concurrent actors still need checkpoints.
///
/// # Panics
/// If spawn, bounded exit, test execution, or the completion marker fails.
#[track_caller]
pub fn isolated(test: &str, check: impl FnOnce()) {
    if std::env::var(HELPER).as_deref() == Ok(test) {
        check();
        // Tested stdout and libtest output cannot stand in for completion.
        std::fs::write(
            std::env::var_os(DONE).expect("helper completion path"),
            b"completed",
        )
        .expect("write helper completion");
        return;
    }

    let done = tempfile::NamedTempFile::new().expect("helper completion file");
    let log = tempfile::tempfile().expect("helper output file");
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(HELPER, test)
        .env(DONE, done.path())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().expect("helper stdout")))
        .stderr(Stdio::from(log.try_clone().expect("helper stderr")));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: only the async-signal-safe setsid syscall runs in the hook.
        // It detaches the child without changing the runner's session or tty.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Capture-only helpers must not share the runner's console either.
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let child = crate::process::spawn(&mut command).expect("spawn isolated helper");
    drop(command);
    let mut helper = Helper {
        child: Some(child),
        log,
    };
    let status = helper.finish();
    // On timeout, stop the writer before reading its diagnostics.
    helper.terminate();
    let output = helper.output();
    let completed = std::fs::read(done.path()).is_ok_and(|bytes| bytes == b"completed");
    assert!(
        status.as_ref().is_ok_and(ExitStatus::success) && completed,
        "isolated helper {test}: exit {status:?}, completed {completed}\n{output}"
    );
}

struct Helper {
    child: Option<Child>,
    log: File,
}

impl Helper {
    fn finish(&mut self) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.as_mut().expect("unreaped helper").try_wait()? {
                // Never signal this numeric group ID again after reaping.
                self.child = None;
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "helper did not exit",
                ));
            }
            // This bounds exit polling only, never proves event ordering.
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }

    fn output(&mut self) -> String {
        let _ = self.log.rewind();
        let mut bytes = Vec::new();
        // A broken test cannot make diagnostics consume unbounded memory.
        let _ = Read::by_ref(&mut self.log)
            .take(64 * 1024)
            .read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn terminate(&mut self) {
        if let Some(mut child) = self.child.take() {
            // The unreaped Unix helper pins its group ID. Kill before wait,
            // and never use that ID after the leader has been reaped.
            #[cfg(unix)]
            if let Ok(pid) = libc::pid_t::try_from(child.id()) {
                // SAFETY: setsid made this owned child its own group leader.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.terminate();
    }
}
