//! Bundled coreutils / diffutils / ripgrep dispatch.
//!
//! These tools live inside the ral binary, not on PATH, so they need
//! a dispatch shape distinct from spawning a system process: call
//! `uumain` in-process via [`run_uutils_in_process`] when sinks and
//! logical state agree with the kernel-level fds (the
//! [`can_run_uutils_in_process`] fast path), otherwise spawn the
//! `ral --ral-bundled-tool <tool>` command image as an ordinary child
//! so `apply_env` can propagate overrides via that subprocess instead
//! of mutating this process's env or cwd from a thread.  Both
//! `run_uutils_in_process` and the bundled feature set it needs are
//! gated on the same cfg, so the inline fast path is the only call site.

#[cfg(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
use {
    crate::types::{Break, Error, Settled, Shell, Value},
    std::io::Write,
};

#[cfg(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
/// Fast-path predicate: is in-process `uumain` safe right now?
///
/// `uumain` reads libc fd 0 and writes libc fd 1/2 and reads
/// `std::env`/`current_dir` directly, so it is sound only when ral's
/// stdio is direct-to-fd and ral's logical state agrees with the process
/// state.  A non-terminal stdin (`Source::Pipe` / `Source::File`, e.g. a
/// piped-into bundled tool or a function body fed by `<`) is parked in
/// `shell.turn.io.stdin`, not on fd 0, so it forces child placement: the
/// child receives the real stdin handle through ordinary stdio plumbing
/// rather than the parent rewiring its own fd 0.  Any capture buffer or
/// audit Tee, or any `env_overrides` / `within [dir: …]` / `cd`-mutated
/// logical cwd that the kernel doesn't see, forces the child placement
/// for the same reason: mutating process stdio, env, or cwd from this
/// thread would race other ral threads (`spawn`, `par`, pipeline).  An
/// active sandbox projection also refuses the inline path: an in-process
/// tool would run unconfined and escape the projection the child
/// placement pins via `sandbox::self_command`.  The projection fold
/// returns `None` immediately when no grant is active, so the extra check
/// stays cheap on the common path.
pub(crate) fn can_run_uutils_in_process(shell: &Shell) -> bool {
    use crate::io::{Sink, Source};
    let cwd_matches_process = match &shell.mobile.context.cwd.current {
        None => true,
        Some(p) => crate::path::process_cwd().is_some_and(|q| q == *p),
    };
    matches!(shell.turn.io.stdin, Source::Terminal)
        && matches!(shell.turn.io.stdout, Sink::Terminal | Sink::External(_))
        && matches!(shell.turn.io.stderr, Sink::Stderr)
        && shell.mobile.context.env_overrides().is_empty()
        && shell.mobile.context.dir.is_none()
        && cwd_matches_process
        && shell.sandbox_projection().is_none()
}

/// Call `uumain` for `tool` in this process.  Stdio is restored before
/// return; a panicking tool surfaces as a 1-status error.
///
/// `can_run_uutils_in_process` must have admitted the call: the
/// caller's stdio discipline and env/cwd state are read implicitly
/// through libc, not through `shell`.  Admission implies ambient
/// terminal stdin and no call-site redirects (`Source::Terminal` in,
/// `Sink::Terminal | External` out, with `redirects.is_empty()`), so
/// this path reads fd 0 as-is and wires no `> file` / `2>&1` plumbing.
/// A non-terminal stdin (`Pipe` / `File`) is excluded by the gate and
/// forces child placement, where the child receives the real stdin
/// handle through ordinary stdio plumbing — the parent never rewires
/// its own fd 0 to make this in-process call look like an exec.
/// Serialises the inline `uutils_invoke` path so the uucore exit-code
/// cell cannot interleave across threads.  `reset_exit_code`,
/// `uutils_invoke`, and `get_exit_code` all touch one process-global
/// cell; two clean-terminal inline invocations on different threads (the
/// REPL thread plus a `spawn` / `watch` / `par` worker) would otherwise
/// reset/invoke/read out of order and read each other's status.
///
/// Per ADR `260616_bundled-tools-as-exec-images` §"Two placements", this
/// mutex guards ONLY the exit-code cell.  It is not an authority
/// mechanism and must never be used to admit fd/env/cwd mutation: those
/// stay thread-local solely by the inline gate excluding every state the
/// child placement owns.
#[cfg(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
static INLINE_UUTILS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
pub(crate) fn run_uutils_in_process(
    tool: &str,
    arg_strs: &[String],
    shell: &mut Shell,
) -> Settled<Value> {
    use crate::builtins::uutils;

    // Rust's userspace stdout/stderr buffers sit above the kernel
    // fd / Win32 std slot.  Flush pending bytes here so they land at
    // the parent destination before `uumain` writes its own output to
    // the same fds.
    std::io::stdout().flush().ok();
    std::io::stderr().flush().ok();

    // Hold this guard across the whole reset → invoke → read window so
    // no other thread can touch the uucore exit-code cell between them.
    // A poisoned lock just means a previous tool panicked (its panic is
    // caught below, and `reset_exit_code` immediately clears the cell),
    // so proceed under the poisoned guard exactly as the other
    // process-local serialisation mutexes in the tree do.
    let _exit_code_guard = INLINE_UUTILS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    uutils::reset_exit_code();

    // `can_run_uutils_in_process` already gates on
    // `cwd.current == process_cwd`, so a well-behaved tool sees an
    // unchanged cwd.  Snapshot it anyway: a misbehaving tool that
    // internally `chdir`s would otherwise leak its change into the
    // next builtin's process-cwd read.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:uutils-cwd-save] Defensive process-cwd snapshot around a third-party uutils tool that might internally chdir; restored below. Process-cwd bookkeeping, not the model's data I/O — the bundled exec surfaces via emit_io(exec) further down."
    )]
    let saved_cwd = std::env::current_dir().ok();

    let os_args: Vec<std::ffi::OsString> = std::iter::once(std::ffi::OsString::from(tool))
        .chain(arg_strs.iter().map(std::ffi::OsString::from))
        .collect();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        uutils::uutils_invoke(tool, os_args)
    }));

    if let Some(cwd) = saved_cwd {
        #[allow(
            clippy::disallowed_methods,
            reason = "[io-door:silent:uutils-cwd-restore] Restores the pre-call process cwd after a third-party uutils tool that might internally chdir. Process-cwd bookkeeping, not the model's data I/O."
        )]
        let _ = std::env::set_current_dir(cwd);
    }

    // Flush uumain's buffered output before returning to ral.
    std::io::stdout().flush().ok();
    std::io::stderr().flush().ok();

    let exit_code = if let Ok(code) = result {
        let global = uutils::get_exit_code();
        if global == 0 { code } else { global }
    } else {
        // Door 3 — EXEC (inline bundled, panic branch): a panicking tool
        // surfaces as status 1, outcome "bad".  Emit before propagating
        // so this completion door fires exactly once, like the normal
        // branch below.
        drop(_exit_code_guard);
        shell.emit_io(super::io_event::exec(tool, arg_strs, 1));
        return Err(Break::Error(
            Error::new(format!("bundled tool '{tool}' panicked"), 1)
                .at_loc(shell.turn.loc.source_loc(0)),
        ));
    };

    // The exit-code cell has been read; release the lock before the
    // status bookkeeping below so unrelated inline work does not wait.
    drop(_exit_code_guard);

    // Door 3 — EXEC (inline bundled completion): the sole inline-uutils
    // door.  The caller (`command::run`) returns before its spawn path for
    // this image, so no external exec event double-fires for this call.
    shell.emit_io(super::io_event::exec(tool, arg_strs, exit_code));
    shell.mobile.control.last_status = exit_code;
    if exit_code == 0 {
        Ok(Value::Unit)
    } else {
        Err(Break::Error(
            Error::new(
                format!("'{tool}' exited with status {exit_code}"),
                exit_code,
            )
            .at_loc(shell.turn.loc.source_loc(tool.len())),
        ))
    }
}

#[cfg(all(
    test,
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
mod tests {
    use super::{INLINE_UUTILS_LOCK, can_run_uutils_in_process};
    use crate::types::Shell;
    use std::cell::Cell;
    use std::sync::Barrier;

    /// `INLINE_UUTILS_LOCK` must serialise the reset → invoke → read
    /// window so two threads cannot interleave on the uucore exit-code
    /// cell.  Model that cell with a non-atomic `Cell` only ever touched
    /// under the lock: if the guard truly excludes the sibling, each
    /// thread's write is still readable after the simulated invoke.  An
    /// interleave would let the sibling clobber the value between write
    /// and read.  No fd/env/cwd state is touched, so this is safe inline
    /// per ADR `260616_bundled-tools-as-exec-images`.
    #[test]
    fn lock_serialises_exit_code_cell_across_threads() {
        // `Sync` is not implied by the lock; wrap the non-`Sync` cell in
        // a newtype the lock makes safe to share, mirroring how the real
        // process-global cell is only sound under the guard.
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
                        // A sibling permitted into this window would
                        // overwrite `code` before we read it back.
                        assert_eq!(cell.0.get(), code, "exit-code cell interleaved");
                    }
                });
            }
        });

        // The guard drops correctly: the lock is reacquirable afterward.
        assert!(INLINE_UUTILS_LOCK.try_lock().is_ok());
    }

    /// A clean default shell — terminal stdin, terminal stdout, fd-2
    /// stderr, no env override / scoped dir / sandbox — is the one shape
    /// the inline `uumain` fast path is admitted for.
    #[test]
    fn clean_terminal_shell_admits_inline() {
        let shell = Shell::default();
        assert!(
            can_run_uutils_in_process(&shell),
            "the clean-terminal case must keep the inline fast path"
        );
    }

    /// A non-terminal stdin (`Source::Pipe`) forces child placement.  The
    /// inline path runs `uumain` against the parent's fd 0; aliasing the
    /// pipe onto fd 0 to feed it is the parent fd-0 surgery the
    /// sandbox-external-children ADR forbids (it races sibling `spawn` /
    /// `par` / pipeline threads), so the gate must refuse inline and let
    /// the child receive the real stdin handle through ordinary stdio.
    #[test]
    fn pipe_stdin_forces_child_placement() {
        let mut shell = Shell::default();
        let (reader, _writer) = os_pipe::pipe().expect("pipe");
        shell.turn.io.stdin = crate::io::Source::Pipe(reader);
        assert!(
            !can_run_uutils_in_process(&shell),
            "a non-terminal stdin must force child placement, not inline \
             fd-0 surgery"
        );
    }
}
