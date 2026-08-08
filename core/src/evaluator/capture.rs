//! Byte-capture brackets: swap a Sink onto `shell.io`, run a closure, restore,
//! drain the buffer.  [`with_capture`] *replaces* stdout, so the bytes become a
//! value; [`with_audit_capture`] *tees*, so they are recorded and still seen.
use crate::io::{Sink, new_buffer, take_buffer, tee_with_buffer};
use crate::types::Shell;
/// Restores `shell.io.stdout` and `capture_outer` on `Drop`, panic included.
struct CaptureScope<'a> {
    shell: &'a mut Shell,
    saved_stdout: Option<Sink>,
    #[allow(
        clippy::option_option,
        reason = "save-slot for an `Option<Sink>` field restored on Drop; outer/inner are distinct states"
    )]
    saved_capture_outer: Option<Option<Sink>>,
}

impl<'a> CaptureScope<'a> {
    /// A discarded statement's bytes reach the nearest enclosing *visible*
    /// stream, never an enclosing capture buffer, however deep the brackets
    /// nest.  So `capture_outer` names that visible stream, and only the
    /// outermost bracket gets to name it: a nested bracket keeps whatever is
    /// already there, since the sink it displaced is another buffer.  Install
    /// the displaced stdout only when the slot is empty — at the top level, or
    /// in the fresh shell of a pipeline stage, whose visible stream is its
    /// wire.
    fn enter(shell: &'a mut Shell, buffer_sink: Sink) -> Self {
        let saved_stdout = std::mem::replace(&mut shell.io.stdout, buffer_sink);
        let saved_capture_outer = shell.io.capture_outer.take();
        shell.io.capture_outer = match &saved_capture_outer {
            Some(visible) => visible.try_clone().ok(),
            None => saved_stdout.try_clone().ok(),
        };
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
            self.shell.io.capture_outer = prev;
        }
        if let Some(prev) = self.saved_stdout.take() {
            self.shell.io.stdout = prev;
        }
    }
}

/// Swap stdout for an in-memory buffer, run `f`, restore, return `(result, bytes)`.
///
/// The enclosing *visible* stream is left in `shell.io.capture_outer`, where
/// `eval_seq` flushes a sequence's non-final bytes, so what drains here is the
/// final value alone and the rest is seen.  `try` deliberately captures
/// nothing; `audit` uses the tee below.
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

/// Restores both sinks on `Drop`, panic included.
struct AuditCaptureScope<'a> {
    shell: &'a mut Shell,
    saved_stdout: Option<Sink>,
    saved_stderr: Option<Sink>,
}

impl<'a> AuditCaptureScope<'a> {
    fn enter(shell: &'a mut Shell, out_sink: Sink, err_sink: Sink) -> Self {
        let saved_stdout = std::mem::replace(&mut shell.io.stdout, out_sink);
        let saved_stderr = std::mem::replace(&mut shell.io.stderr, err_sink);
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
            self.shell.io.stdout = prev;
        }
        if let Some(prev) = self.saved_stderr.take() {
            self.shell.io.stderr = prev;
        }
    }
}

/// Tee `shell.io.stdout`/`stderr` into buffers while `f` runs, so `audit { … }`
/// records a command's bytes without hiding them.  Installed by `frame_call` in
/// `evaluator::audit` around each builtin and standalone external; direct-spawn
/// pipeline stages never reach here, since their stdout is a kernel pipe to the
/// next stage and `pipeline::collect` synthesises their node with no bytes.
///
/// A failed [`Sink::try_clone`] returns `Err` rather than falling back to the
/// terminal, so a command under a redirect is never silently rerouted.
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
    let out_base = shell.io.stdout.try_clone()?;
    let err_base = shell.io.stderr.try_clone()?;
    let (out_sink, out_buf) = tee_with_buffer(out_base);
    let (err_sink, err_buf) = tee_with_buffer(err_base);
    let scope = AuditCaptureScope::enter(shell, out_sink, err_sink);
    let result = f(scope.shell);
    drop(scope);
    Ok((result, take_buffer(&out_buf), take_buffer(&err_buf)))
}
