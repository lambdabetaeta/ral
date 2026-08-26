//! Which sink the payload goes to, for the length of a bracket: swap a Sink
//! onto `shell.io.stdout`, run a closure, restore.  [`with_capture`] installs a
//! buffer, so the bytes become a value; [`with_ambient_stdout`] installs the
//! visible stream, so a discarded statement is seen; [`with_audit_capture`]
//! *tees*, so bytes are recorded and still go where they were going.
//!
//! None of them touches `shell.io.ambient`.  A write's destination is settled
//! where the writer stands, so no byte is ever moved after the fact and nesting
//! has no rule to get wrong.
use crate::io::{Sink, buffer_overflowed, new_buffer, take_buffer, tee_into, tee_with_buffer};
use crate::types::Shell;

/// Restores `shell.io.stdout` on `Drop`, panic included.
struct StdoutScope<'a> {
    shell: &'a mut Shell,
    saved: Option<Sink>,
}

impl<'a> StdoutScope<'a> {
    fn enter(shell: &'a mut Shell, stdout: Sink) -> Self {
        let saved = std::mem::replace(&mut shell.io.stdout, stdout);
        Self {
            shell,
            saved: Some(saved),
        }
    }
}

impl Drop for StdoutScope<'_> {
    fn drop(&mut self) {
        if let Some(prev) = self.saved.take() {
            self.shell.io.stdout = prev;
        }
    }
}

/// Swap stdout for an in-memory buffer, run `f`, restore, return
/// `(result, bytes, overflowed)`.
///
/// What drains here is the tail's bytes alone: a sequence's non-final parts ran
/// under [`with_ambient_stdout`] and never wrote to this buffer in the first
/// place.  `try` deliberately captures nothing; `audit` uses the tee below.
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

/// Run `f` with the payload sink replaced by the visible one, for a computation
/// whose value is discarded: `echo b` in `{ echo b ; echo c }` writes where it
/// is seen, at the moment it writes.
///
/// A thin bracket over [`crate::io::Io::to_ambient`]: the swap it performs is
/// the primitive; this restores it on the way out, panic included.
pub(crate) fn with_ambient_stdout<R, F>(shell: &mut Shell, f: F) -> R
where
    F: FnOnce(&mut Shell) -> R,
{
    let saved = shell.io.to_ambient();
    let scope = StdoutScope {
        shell,
        saved: Some(saved),
    };
    f(scope.shell)
}

/// Restores all three sinks on `Drop`, panic included.
struct AuditCaptureScope<'a> {
    shell: &'a mut Shell,
    saved_stdout: Option<Sink>,
    saved_ambient: Option<Sink>,
    saved_stderr: Option<Sink>,
}

impl<'a> AuditCaptureScope<'a> {
    fn enter(shell: &'a mut Shell, out_sink: Sink, amb_sink: Sink, err_sink: Sink) -> Self {
        let saved_stdout = std::mem::replace(&mut shell.io.stdout, out_sink);
        let saved_ambient = std::mem::replace(&mut shell.io.ambient, amb_sink);
        let saved_stderr = std::mem::replace(&mut shell.io.stderr, err_sink);
        Self {
            shell,
            saved_stdout: Some(saved_stdout),
            saved_ambient: Some(saved_ambient),
            saved_stderr: Some(saved_stderr),
        }
    }
}

impl Drop for AuditCaptureScope<'_> {
    fn drop(&mut self) {
        if let Some(prev) = self.saved_stdout.take() {
            self.shell.io.stdout = prev;
        }
        if let Some(prev) = self.saved_ambient.take() {
            self.shell.io.ambient = prev;
        }
        if let Some(prev) = self.saved_stderr.take() {
            self.shell.io.stderr = prev;
        }
    }
}

/// Tee every sink this shell can write into buffers while `f` runs, so
/// `audit { … }` records a command's bytes without hiding them.  The ambient
/// sink is teed alongside stdout because a discarded statement writes there
/// directly — under the drain those bytes passed through stdout first, and
/// teeing stdout alone was enough.  Both feed the one stdout buffer: they are
/// two conduits, not two records.
///
/// Installed by `frame_call` in `evaluator::audit` around each builtin and
/// standalone external; direct-spawn pipeline stages never reach here, since
/// their stdout is a kernel pipe to the next stage and `pipeline::collect`
/// synthesises their node with no bytes.
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
    let out_base = shell.io.stdout.clone();
    let amb_base = shell.io.ambient.clone();
    let err_base = shell.io.stderr.clone();
    let (out_sink, out_buf) = tee_with_buffer(out_base);
    let amb_sink = tee_into(amb_base, &out_buf);
    let (err_sink, err_buf) = tee_with_buffer(err_base);
    let scope = AuditCaptureScope::enter(shell, out_sink, amb_sink, err_sink);
    let result = f(scope.shell);
    drop(scope);
    Ok((result, take_buffer(&out_buf), take_buffer(&err_buf)))
}
