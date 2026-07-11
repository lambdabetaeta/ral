//! Two capture-policy primitives that the evaluator wraps around
//! command dispatch when bytes need to be observed.
//!
//! Both follow the same swap-restore-drain dance: replace a Sink on
//! `shell.turn.io`, run the closure, restore the saved Sink, drain the
//! buffer.  They differ on visibility:
//!
//!   * [`with_capture`] *replaces* `shell.turn.io.stdout` with
//!     `Sink::Buffer`, diverting final byte output from the terminal so
//!     it can be bound as a value.
//!   * [`with_audit_capture`] *tees* through `Sink::Tee(Buffer, real)`,
//!     so bytes are observed AND stay visible.
//!
//! Reading the two side-by-side is the documentation: same shape,
//! different policy.
use crate::io::{Sink, new_buffer, take_buffer, tee_with_buffer};
use crate::types::Shell;
/// RAII guard for [`with_capture`]: swaps `shell.turn.io.stdout` for an
/// in-memory buffer sink and restores it on `Drop` — including on panic.
struct CaptureScope<'a> {
    shell: &'a mut Shell,
    saved_stdout: Option<Sink>,
    #[allow(clippy::option_option, reason = "save-slot for an `Option<Sink>` field restored on Drop; outer/inner are distinct states")]
    saved_capture_outer: Option<Option<Sink>>,
}

impl<'a> CaptureScope<'a> {
    fn enter(shell: &'a mut Shell, buffer_sink: Sink) -> Self {
        let saved_stdout = std::mem::replace(&mut shell.turn.io.stdout, buffer_sink);
        let saved_capture_outer = std::mem::replace(
            &mut shell.turn.io.capture_outer,
            saved_stdout.try_clone().ok(),
        );
        Self {
            shell,
            saved_stdout: Some(saved_stdout),
            saved_capture_outer: Some(saved_capture_outer),
        }
    }
}

impl Drop for CaptureScope<'_> {
    fn drop(&mut self) {
        if let Some(prev) = self.saved_capture_outer.take() {
            self.shell.turn.io.capture_outer = prev;
        }
        if let Some(prev) = self.saved_stdout.take() {
            self.shell.turn.io.stdout = prev;
        }
    }
}

/// Swap stdout for an in-memory buffer, run `f`, restore, return `(result, bytes)`.
///
/// A surrounding `Seq` flushes non-final byte effects to the saved
/// outer sink, so the drained buffer represents the final byte value
/// at the binding boundary.
/// This is the sole capture primitive.  `try` does not capture: control
/// flow and byte capture are kept separate (§10.1).  Use `audit` to record
/// per-command bytes into the execution tree.
pub fn with_capture<R, F>(shell: &mut Shell, f: F) -> (R, Vec<u8>)
where
    F: FnOnce(&mut Shell) -> R,
{
    let (sink, buf) = new_buffer();
    let scope = CaptureScope::enter(shell, sink);
    let result = f(scope.shell);
    drop(scope);
    (result, take_buffer(&buf))
}

/// RAII guard for [`with_audit_capture`]: tees stdout and stderr through
/// in-memory buffers and restores the original sinks on `Drop`, panic or
/// otherwise.
struct AuditCaptureScope<'a> {
    shell: &'a mut Shell,
    saved_stdout: Option<Sink>,
    saved_stderr: Option<Sink>,
}

impl<'a> AuditCaptureScope<'a> {
    fn enter(shell: &'a mut Shell, out_sink: Sink, err_sink: Sink) -> Self {
        let saved_stdout = std::mem::replace(&mut shell.turn.io.stdout, out_sink);
        let saved_stderr = std::mem::replace(&mut shell.turn.io.stderr, err_sink);
        Self {
            shell,
            saved_stdout: Some(saved_stdout),
            saved_stderr: Some(saved_stderr),
        }
    }
}

impl Drop for AuditCaptureScope<'_> {
    fn drop(&mut self) {
        if let Some(prev) = self.saved_stdout.take() {
            self.shell.turn.io.stdout = prev;
        }
        if let Some(prev) = self.saved_stderr.take() {
            self.shell.turn.io.stderr = prev;
        }
    }
}

/// Per-command byte capture for `audit { … }`: tee `shell.turn.io.stdout` and
/// `shell.turn.io.stderr` through in-memory buffers while `f` runs, then restore.
///
/// Returns `Ok((result, stdout_bytes, stderr_bytes))`.  The output stays
/// visible — Tee writes to both the buffer and the original sink — which is
/// the difference from [`with_capture`], whose buffer *replaces* the visible
/// sink for let-binding semantics.  Restoration is RAII (see
/// [`AuditCaptureScope`]); a panic from `f` still puts both sinks back.
///
/// Teeing requires a clone of the live stdout/stderr sinks; when
/// [`Sink::try_clone`] fails (e.g. fd exhaustion on a `File`/`Pipe`
/// destination), the error is returned as `Err` rather than substituting a
/// different destination, so a command under a redirect or pipeline stage is
/// never silently rerouted to the terminal.
///
/// When `shell.local.audit.captures_bytes()` is false this short-circuits:
/// `f` runs against the unmodified sinks, the returned buffers are empty, and
/// no clone or swap occurs.  The capture policy is the single control signal
/// for "we are inside an `audit` scope"; the Sink shape itself is a
/// consequence, not the predicate.
///
/// Installed once around each *command* dispatch (see
/// [`crate::runtime::transport::dispatch`]) for builtins and standalone externals.  Pipeline
/// stages don't pass through this path — they capture per-stage via
/// [`crate::io::tee_with_buffer`] at stage launch, because their stdout
/// is `Sink::Pipe(writer)` to the next stage and never touches
/// `shell.turn.io.stdout`.  Distinct from `Shell::audit_child` (a method
/// on `Shell` that collects child `ExecNode`s into an
/// [`AuditFragment`]).
pub(crate) fn with_audit_capture<R, F>(
    shell: &mut Shell,
    f: F,
) -> std::io::Result<(R, Vec<u8>, Vec<u8>)>
where
    F: FnOnce(&mut Shell) -> R,
{
    if !shell.local.audit.captures_bytes() {
        return Ok((f(shell), Vec::new(), Vec::new()));
    }
    let out_base = shell.turn.io.stdout.try_clone()?;
    let err_base = shell.turn.io.stderr.try_clone()?;
    let (out_sink, out_buf) = tee_with_buffer(out_base);
    let (err_sink, err_buf) = tee_with_buffer(err_base);
    let scope = AuditCaptureScope::enter(shell, out_sink, err_sink);
    let result = f(scope.shell);
    drop(scope);
    Ok((result, take_buffer(&out_buf), take_buffer(&err_buf)))
}
