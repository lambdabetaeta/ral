//! Bundled coreutils / diffutils / ripgrep dispatch.
//!
//! These tools live inside the ral binary rather than on PATH.  A call runs
//! `uumain` in this process when `can_run_uutils_in_process` admits it, and
//! otherwise spawns `ral --ral-bundled-tool <tool>` as an ordinary child, so
//! `apply_env` can hand that child the env and cwd overrides instead of this
//! thread mutating the process's own.

#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
use {
    crate::types::{Break, Error, Mooring, Settled, Shell, Value},
    std::io::Write,
};

#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
/// Is in-process `uumain` safe right now?
///
/// `uumain` reads fd 0, writes fd 1/2 and consults `std::env` / `current_dir`
/// directly, so inline is sound only where ral's logical state already agrees
/// with the process state — a parked stdin, a capture buffer, an env override
/// or a logical cwd the kernel does not already share would each need this
/// thread to rewire fds, env or cwd, racing every sibling `spawn` / `par` /
/// pipeline thread.  A sandbox projection is refused separately: inline, the
/// tool would escape the confinement `build_command` gives the child.
pub(crate) fn can_run_uutils_in_process(shell: &Shell) -> bool {
    use crate::io::{Sink, Source};
    matches!(shell.io.stdin, Source::Terminal)
        && matches!(shell.io.stdout, Sink::Terminal | Sink::External(_))
        && matches!(shell.io.stderr, Sink::Stderr)
        && shell.mobile.context.env_overrides().is_empty()
        && shell.mobile.context.cwd_agrees_with_process()
        && shell.sandbox_projection().is_none()
}

/// Serialises the inline path: `invoke_bundled`'s reset → invoke → read of
/// uucore's process-global exit-code cell would otherwise interleave between
/// the REPL thread and a `spawn` / `watch` / `par` worker.  It guards that cell
/// and nothing else — fd, env and cwd stay thread-local because the gate above
/// excludes them, never because this lock would make mutating them safe.
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
static INLINE_UUTILS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Call `uumain` for `tool` in this process; a panicking tool surfaces as a
/// 1-status error.
///
/// `can_run_uutils_in_process` must have admitted the call, and `command::run`
/// adds `redirects.is_empty()` to that gate: this path reads fd 0 as it stands
/// and wires no `> file` / `2>&1` plumbing.  The exit-code protocol lives in
/// `invoke_bundled` in `crate::builtins::uutils`; this placement adds cell
/// serialisation, cwd save/restore and panic isolation around it.
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
pub(crate) fn run_uutils_in_process(
    tool: &str,
    arg_strs: &[String],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    use crate::builtins::uutils;

    // These userspace buffers sit above the fds `uumain` writes, so drain them
    // here or ral's pending output lands after the tool's.
    std::io::stdout().flush().ok();
    std::io::stderr().flush().ok();

    // A poisoned lock only means an earlier tool panicked, and `invoke_bundled`
    // clears the cell on entry, so the poison carries no stale state.
    let exit_code_guard = INLINE_UUTILS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // The gate already pins the effective cwd to the process cwd, so this snapshot
    // is defence against a tool that `chdir`s internally and would otherwise
    // leak that into the next builtin's process-cwd read.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:uutils-cwd-save] Defensive process-cwd snapshot around a third-party uutils tool that might internally chdir; restored below. Process-cwd bookkeeping, not the model's data I/O — the bundled exec surfaces via emit_io(&exec) further down."
    )]
    let saved_cwd = std::env::current_dir().ok();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        uutils::invoke_bundled(tool, arg_strs)
    }));

    if let Some(cwd) = saved_cwd {
        #[allow(
            clippy::disallowed_methods,
            reason = "[io-door:silent:uutils-cwd-restore] Restores the pre-call process cwd after a third-party uutils tool that might internally chdir. Process-cwd bookkeeping, not the model's data I/O."
        )]
        let _ = std::env::set_current_dir(cwd);
    }

    // `uumain` wrote through those same buffers; drain them before ral resumes.
    std::io::stdout().flush().ok();
    std::io::stderr().flush().ok();

    let Ok(exit_code) = result else {
        // Door 3 — EXEC (panic branch): emit before propagating, so this
        // completion door fires exactly once as the normal branch does.
        drop(exit_code_guard);
        mooring.emit_io(&super::io_event::exec(tool, arg_strs, 1));
        return Err(Break::Error(Error::new(
            format!("bundled tool '{tool}' panicked"),
            1,
        )));
    };

    // The cell is read; do not hold the lock through the bookkeeping below.
    drop(exit_code_guard);

    // Door 3 — EXEC: `command::run` returns here rather than reaching its own
    // exec door below the spawn, so this call fires exactly one exec event.
    mooring.emit_io(&super::io_event::exec(tool, arg_strs, exit_code));
    shell.mobile.control.last_status = exit_code;
    if exit_code == 0 {
        Ok(Value::Unit)
    } else {
        Err(Break::Error(Error::new(
            format!("'{tool}' exited with status {exit_code}"),
            exit_code,
        )))
    }
}

#[cfg(all(
    test,
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
mod tests {
    use super::{INLINE_UUTILS_LOCK, can_run_uutils_in_process};
    use crate::types::Shell;
    use std::cell::Cell;
    use std::sync::Barrier;

    /// The lock must keep two threads out of each other's reset → invoke → read
    /// window.  A non-atomic `Cell` stands in for the uucore exit-code cell: an
    /// interleave would clobber the value between the write and the read back.
    #[test]
    fn lock_serialises_exit_code_cell_across_threads() {
        struct Cell0(Cell<i32>);
        // SAFETY: every access goes through `INLINE_UUTILS_LOCK`, so the
        // cell is never touched concurrently.
        unsafe impl Sync for Cell0 {}
        let cell = Cell0(Cell::new(0));
        let start = Barrier::new(2);

        std::thread::scope(|s| {
            for code in [7, 13] {
                let cell = &cell;
                let start = &start;
                s.spawn(move || {
                    start.wait();
                    for _ in 0..2_000 {
                        let _g = INLINE_UUTILS_LOCK
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        cell.0.set(code);
                        assert_eq!(cell.0.get(), code, "exit-code cell interleaved");
                    }
                });
            }
        });

        assert!(INLINE_UUTILS_LOCK.try_lock().is_ok());
    }

    /// A clean default shell is the one shape the inline fast path admits.
    #[test]
    fn clean_terminal_shell_admits_inline() {
        let shell = Shell::default();
        assert!(
            can_run_uutils_in_process(&shell),
            "the clean-terminal case must keep the inline fast path"
        );
    }

    /// A non-terminal stdin forces the child placement: feeding it inline would
    /// mean aliasing the pipe onto the parent's own fd 0, racing sibling
    /// `spawn` / `par` / pipeline threads.
    #[test]
    fn pipe_stdin_forces_child_placement() {
        let mut shell = Shell::default();
        let (reader, _writer) = os_pipe::pipe().expect("pipe");
        shell.io.stdin = crate::io::Source::Pipe(reader);
        assert!(
            !can_run_uutils_in_process(&shell),
            "a non-terminal stdin must force child placement, not inline \
             fd-0 surgery"
        );
    }

    /// A `cd` away from the process cwd forces the child placement.
    #[test]
    fn cd_away_from_the_process_cwd_forces_child_placement() {
        let mut shell = Shell::default();
        let dir = tempfile::tempdir().expect("tempdir");
        shell.seed_cwd(dir.path().to_path_buf());
        assert!(!can_run_uutils_in_process(&shell));
    }

    /// A `within [dir: …]` that merely names the process cwd is inline-safe:
    /// `uumain`'s own `current_dir()` read answers the same place.
    #[test]
    fn within_dir_naming_the_process_cwd_admits_inline() {
        let mut shell = Shell::default();
        let here = crate::path::process_cwd().expect("test process has a cwd");
        let admitted = shell.with_cwd(here, |s| can_run_uutils_in_process(s));
        assert!(admitted);
    }

    /// A `within [dir: …]` naming anywhere else still forces the child
    /// placement.
    #[test]
    fn within_dir_elsewhere_forces_child_placement() {
        let mut shell = Shell::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let admitted = shell.with_cwd(dir.path().to_path_buf(), |s| can_run_uutils_in_process(s));
        assert!(!admitted);
    }
}
