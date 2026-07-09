//! Hidden helper modes for ral's own integration tests.
//!
//! These flags exist so a test can drive ral as a pipeline stage and
//! observe runtime state (process group, controlling terminal) without
//! shipping an extra helper binary.  The flags are not user-facing and
//! are not advertised in `--help`.

/// Hidden multicall sentinel: emit the helper's own pgid and exit.
#[cfg(unix)]
pub(crate) const PGID_CHECK_FLAG: &str = "--ral-test-pgid-check";

/// Dispatch a hidden test-helper flag.  Returns `Some(code)` when the
/// flag was recognised so the caller can exit immediately; otherwise
/// `None` and the normal CLI path runs.
///
/// `--ral-test-pgid-check <tag>`
///     Write `pgid:<tag>=<getpgrp()>\n` to stderr and exit 0.  Tests use
///     this to confirm pipeline stages share the pgid the parent
///     established.  When *stderr* is a tty the helper additionally
///     writes `tcpgrp:<tag>=<tcgetpgrp(stderr)>\n`.  Stderr is the
///     pipeline-friendly probe: pipeline stages have their stdin /
///     stdout rerouted through OS pipes but inherit stderr from the
///     parent ral, so PTY-based tests (which install the pty as the
///     parent's stderr) see "tty=yes" while batch-mode tests (cargo's
///     piped stderr capture) see "tty=no" — even when a controlling
///     terminal exists upstream of the cargo runner.
pub fn try_run_test_helper() -> Option<u8> {
    #[cfg(unix)]
    {
        use std::fmt::Write as _;
        use std::io::Write;

        let mut args = std::env::args_os();
        let _argv0 = args.next();
        let flag = args.next()?;
        if flag != PGID_CHECK_FLAG {
            return None;
        }
        let tag = args
            .next()
            .map_or_else(|| "stage".into(), |t| t.to_string_lossy().into_owned());
        let pgid = unsafe { libc::getpgrp() };
        let stderr_fd = libc::STDERR_FILENO;

        // Build the whole output in one buffer and emit it via a
        // single `write_all`.  Bare `eprintln!` produces several small
        // syscalls per format substitution; with three pipeline
        // stages writing the same line concurrently to a shared
        // stderr the bytes interleave (`pgid:pgid:downup==…`).  A
        // single `write_all` under PIPE_BUF (4096) is atomic on
        // pipes and ttys.
        let mut buf = String::new();
        let _ = writeln!(&mut buf, "pgid:{tag}={pgid}");
        if unsafe { libc::isatty(stderr_fd) } == 1 {
            let fg = unsafe { libc::tcgetpgrp(stderr_fd) };
            if fg >= 0 {
                let _ = writeln!(&mut buf, "tcpgrp:{tag}={fg}");
            }
        }
        let _ = std::io::stderr().write_all(buf.as_bytes());
        Some(0)
    }
    #[cfg(not(unix))]
    {
        None
    }
}
