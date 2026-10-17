//! Which sink the output goes to, for the length of a bracket: swap a Sink onto
//! `shell.io.stdout`, run a closure, restore.  [`with_capture`] installs a
//! buffer, so the bytes become a value; [`with_audit_capture`] *tees*, so bytes
//! are recorded and still go where they were going.  A write's destination is
//! settled where the writer stands, so no byte is ever moved after the fact.
use crate::io::{Sink, new_buffer, tee_with_buffer};
use crate::types::{AuditIo, Shell};
use std::mem::{replace, swap};

const AUDIT_BUFFER_CAP: usize = 1024 * 1024;

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
pub(crate) fn with_audit_capture<R, F>(shell: &mut Shell, f: F) -> (R, AuditIo)
where
    F: FnOnce(&mut Shell) -> R,
{
    if !shell.local.audit.captures_bytes() {
        return (f(shell), AuditIo::default());
    }
    let (out_sink, out_buf) = tee_with_buffer(shell.io.stdout.clone(), AUDIT_BUFFER_CAP);
    let (err_sink, err_buf) = tee_with_buffer(shell.io.stderr.clone(), AUDIT_BUFFER_CAP);
    let scope = SinkScope::enter(shell, out_sink, Some(err_sink));
    let result = f(scope.shell);
    drop(scope);
    let (stdout, stdout_total) = out_buf.take_counted();
    let (stderr, stderr_total) = err_buf.take_counted();
    let io = AuditIo {
        stdout,
        stderr,
        stdout_total,
        stderr_total,
    };
    (result, io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CapturePolicy;

    #[test]
    fn audit_caps_both_streams_without_limiting_ordinary_capture() {
        let mut shell = crate::test_helper::core_shell();
        let scope = shell.local.audit.open(CapturePolicy::Bytes);
        let stdout: Vec<u8> = (0..=255).cycle().take(AUDIT_BUFFER_CAP + 4096).collect();
        let stderr = vec![255; AUDIT_BUFFER_CAP + 1];
        let (err_sink, err_buf) = new_buffer();
        shell.io.stderr = err_sink;
        let ((result, io), captured, overflowed) = with_capture(&mut shell, |shell| {
            with_audit_capture(shell, |shell| {
                shell.write_stdout(&stdout)?;
                shell.write_stderr(&stderr)
            })
        });
        result.unwrap();
        let _trail = shell.local.audit.close(scope);
        assert!(!overflowed);
        assert_eq!(captured, stdout);
        assert_eq!(err_buf.take(), stderr);
        assert_eq!(io.stdout, stdout[..AUDIT_BUFFER_CAP]);
        assert_eq!(io.stderr, stderr[..AUDIT_BUFFER_CAP]);
        assert_eq!(io.stdout_total, stdout.len() as u64);
        assert_eq!(io.stderr_total, stderr.len() as u64);
    }

    #[test]
    fn the_exact_audit_limit_is_complete() {
        let mut shell = crate::test_helper::core_shell();
        let scope = shell.local.audit.open(CapturePolicy::Bytes);
        let (sink, _buf) = new_buffer();
        shell.io.stdout = sink;
        let (result, io) = with_audit_capture(&mut shell, |shell| {
            shell.write_stdout(&vec![0; AUDIT_BUFFER_CAP])
        });
        result.unwrap();
        let _trail = shell.local.audit.close(scope);
        assert_eq!(io.stdout.len(), AUDIT_BUFFER_CAP);
        assert_eq!(io.stdout_total, AUDIT_BUFFER_CAP as u64);
        assert_eq!(io.stderr, b"");
        assert_eq!(io.stderr_total, 0);
    }
}
