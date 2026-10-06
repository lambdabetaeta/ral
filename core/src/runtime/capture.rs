//! Which sink the output goes to, for the length of a bracket: swap a Sink onto
//! `shell.io.stdout`, run a closure, restore.  [`with_capture`] installs a
//! buffer, so the bytes become a value; [`with_audit_capture`] *tees*, so bytes
//! are recorded and still go where they were going.  A write's destination is
//! settled where the writer stands, so no byte is ever moved after the fact.
use crate::io::{Sink, new_buffer, tee_with_buffer};
use crate::types::Shell;
use std::mem::{replace, swap};

/// Restores the sinks it replaced on `Drop`, panic included.
struct SinkScope<'a> {
    shell: &'a mut Shell,
    stdout: Sink,
    stderr: Option<Sink>,
}

impl<'a> SinkScope<'a> {
    fn enter(shell: &'a mut Shell, stdout: Sink, stderr: Option<Sink>) -> Self {
        Self {
            stdout: replace(&mut shell.io.stdout, stdout),
            stderr: stderr.map(|sink| replace(&mut shell.io.stderr, sink)),
            shell,
        }
    }
}

impl Drop for SinkScope<'_> {
    fn drop(&mut self) {
        swap(&mut self.shell.io.stdout, &mut self.stdout);
        if let Some(saved) = &mut self.stderr {
            swap(&mut self.shell.io.stderr, saved);
        }
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
    let scope = SinkScope::enter(shell, sink, None);
    let result = f(scope.shell);
    drop(scope);
    (result, buf.take(), buf.overflowed())
}

/// Tee every sink this shell can write into buffers while `f` runs, so
/// `audit { … }` records a command's bytes without hiding them.
///
/// Installed by `call_external` in `runtime::command_call` around each
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
    let (out_sink, out_buf) = tee_with_buffer(shell.io.stdout.clone());
    let (err_sink, err_buf) = tee_with_buffer(shell.io.stderr.clone());
    let scope = SinkScope::enter(shell, out_sink, Some(err_sink));
    let result = f(scope.shell);
    drop(scope);
    (result, out_buf.take(), err_buf.take())
}
