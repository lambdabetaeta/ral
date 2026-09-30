//! Which sink the output goes to, for the length of a bracket: swap a Sink onto
//! `shell.io.stdout`, run a closure, restore.  [`with_capture`] installs a
//! buffer, so the bytes become a value; [`with_audit_capture`] *tees*, so bytes
//! are recorded and still go where they were going.  A write's destination is
//! settled where the writer stands, so no byte is ever moved after the fact.
use crate::io::{Sink, buffer_overflowed, new_buffer, take_buffer, tee_with_buffer};
use crate::types::Shell;

/// Restores `shell.io.stdout` on `Drop`, panic included.
struct StdoutScope<'a> {
    shell: &'a mut Shell,
    saved: Sink,
}

impl<'a> StdoutScope<'a> {
    fn enter(shell: &'a mut Shell, stdout: Sink) -> Self {
        let saved = std::mem::replace(&mut shell.io.stdout, stdout);
        Self { shell, saved }
    }
}

impl Drop for StdoutScope<'_> {
    fn drop(&mut self) {
        std::mem::swap(&mut self.shell.io.stdout, &mut self.saved);
    }
}

/// Swap stdout for an in-memory buffer, run `f`, restore, return
/// `(result, bytes, overflowed)`.
///
/// Everything the closure writes drains here.  `try` deliberately captures
/// nothing; `audit` uses the tee below.
///
/// `overflowed` says the buffer's cap truncated those bytes.  It is the only
/// report there is: writers reach the buffer from pump threads whose join
/// discards their value, so nothing on the write path can raise it, and a
/// caller that means to turn the bytes into a value must consult it here.
pub fn with_capture<R, F>(shell: &mut Shell, f: F) -> (R, Vec<u8>, bool)
where
    F: FnOnce(&mut Shell) -> R,
{
    let (sink, buf) = new_buffer();
    let scope = StdoutScope::enter(shell, sink);
    let result = f(scope.shell);
    drop(scope);
    (result, take_buffer(&buf), buffer_overflowed(&buf))
}

/// Restores both sinks on `Drop`, panic included.
struct AuditCaptureScope<'a> {
    shell: &'a mut Shell,
    saved_stdout: Sink,
    saved_stderr: Sink,
}

impl<'a> AuditCaptureScope<'a> {
    fn enter(shell: &'a mut Shell, out_sink: Sink, err_sink: Sink) -> Self {
        Self {
            saved_stdout: std::mem::replace(&mut shell.io.stdout, out_sink),
            saved_stderr: std::mem::replace(&mut shell.io.stderr, err_sink),
            shell,
        }
    }
}

impl Drop for AuditCaptureScope<'_> {
    fn drop(&mut self) {
        std::mem::swap(&mut self.shell.io.stdout, &mut self.saved_stdout);
        std::mem::swap(&mut self.shell.io.stderr, &mut self.saved_stderr);
    }
}

/// Tee every sink this shell can write into buffers while `f` runs, so
/// `audit { … }` records a command's bytes without hiding them.
///
/// Installed by `frame_call` in `evaluator::audit` around each builtin and
/// standalone external; direct-spawn pipeline stages never reach here, since
/// their stdout is a kernel pipe to the next stage and `pipeline::collect`
/// synthesises their node with no bytes.
pub(crate) fn with_audit_capture<R, F>(shell: &mut Shell, f: F) -> (R, Vec<u8>, Vec<u8>)
where
    F: FnOnce(&mut Shell) -> R,
{
    if !shell.local.audit.captures_bytes() {
        return (f(shell), Vec::new(), Vec::new());
    }
    let out_base = shell.io.stdout.clone();
    let err_base = shell.io.stderr.clone();
    let (out_sink, out_buf) = tee_with_buffer(out_base);
    let (err_sink, err_buf) = tee_with_buffer(err_base);
    let scope = AuditCaptureScope::enter(shell, out_sink, err_sink);
    let result = f(scope.shell);
    drop(scope);
    (result, take_buffer(&out_buf), take_buffer(&err_buf))
}
