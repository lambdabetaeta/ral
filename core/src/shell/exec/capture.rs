//! Which sink the output goes to, for the length of a bracket: swap a Sink onto
//! `shell.io.stdout`, run a closure, restore.  [`with_capture`] installs a
//! buffer, so the bytes become a value; [`with_audit_capture`] *tees*, so bytes
//! are recorded and still go where they were going.  A write's destination is
//! settled where the writer stands, so no byte is ever moved after the fact.
use super::redirect::Prior;
use crate::fact::AuditIo;
use crate::io::{Retained, Sink, Streams, new_buffer, tee_with_buffer};
use crate::shell::runtime::Shell;
use std::mem::replace;

const AUDIT_BUFFER_CAP: usize = 1024 * 1024;

/// Restores the sinks it replaced on `Drop`, panic included.
struct SinkScope<'a> {
    shell: &'a mut Shell,
    prior: Option<Prior>,
}

impl<'a> SinkScope<'a> {
    fn enter(shell: &'a mut Shell, stdout: Sink, stderr: Option<Sink>) -> Self {
        let prior = Prior {
            stdout: Some(replace(&mut shell.io.stdout, stdout)),
            stderr: stderr.map(|sink| replace(&mut shell.io.stderr, sink)),
            ..Prior::default()
        };
        Self {
            shell,
            prior: Some(prior),
        }
    }
}

impl Drop for SinkScope<'_> {
    fn drop(&mut self) {
        if let Some(prior) = self.prior.take() {
            prior.restore(self.shell);
        }
    }
}

/// Swap stdout for an in-memory buffer, run `f`, restore, return
/// `(result, retained)`.
///
/// Everything the closure writes drains here.  `try` deliberately captures
/// nothing; `audit` uses the tee below.
///
/// A cut prefix is the only report of the buffer's cap: writers reach the
/// buffer from pump threads whose join discards their value, so nothing on
/// the write path can raise it, and a caller that means to turn the bytes
/// into a value must consult [`Retained::is_whole`] here.
pub fn with_capture<R, F>(shell: &mut Shell, f: F) -> (R, Retained)
where
    F: FnOnce(&mut Shell) -> R,
{
    let (sink, buf) = new_buffer();
    let scope = SinkScope::enter(shell, sink, None);
    let result = f(scope.shell);
    drop(scope);
    (result, buf.take())
}

/// Tee every sink this shell can write into buffers while `f` runs, so
/// `audit { … }` records a command's bytes without hiding them.
///
/// Installed by `call_external` in `shell::exec::dispatch` around each
/// external.  Byte audit threads every pipeline stage (`resolve_launch`), so
/// none is spawned direct and untapped.
pub(crate) fn with_audit_capture<R, F>(shell: &mut Shell, f: F) -> (R, AuditIo)
where
    F: FnOnce(&mut Shell) -> R,
{
    if !shell.local.audit.captures_bytes() {
        return (f(shell), AuditIo::default());
    }
    let bases = Streams {
        stdout: shell.io.stdout.clone(),
        stderr: shell.io.stderr.clone(),
    };
    let (sinks, buffers) = bases
        .map(|base| tee_with_buffer(base, AUDIT_BUFFER_CAP))
        .unzip();
    let scope = SinkScope::enter(shell, sinks.stdout, Some(sinks.stderr));
    let result = f(scope.shell);
    drop(scope);
    (result, buffers.map(|buf| buf.take()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::runtime::CapturePolicy;

    #[test]
    fn audit_caps_both_streams_without_limiting_ordinary_capture() {
        let mut shell = crate::test_helper::core_shell();
        let scope = shell.local.audit.open(CapturePolicy::Bytes);
        let stdout: Vec<u8> = (0..=255).cycle().take(AUDIT_BUFFER_CAP + 4096).collect();
        let stderr = vec![255; AUDIT_BUFFER_CAP + 1];
        let (err_sink, err_buf) = new_buffer();
        shell.io.stderr = err_sink;
        let ((result, io), captured) = with_capture(&mut shell, |shell| {
            with_audit_capture(shell, |shell| {
                shell.write_stdout(&stdout)?;
                shell.write_stderr(&stderr)
            })
        });
        result.unwrap();
        let _trail = shell.local.audit.close(scope);
        assert!(captured.is_whole());
        assert_eq!(captured.bytes, stdout);
        assert_eq!(err_buf.take().bytes, stderr);
        assert_eq!(io.stdout.bytes, stdout[..AUDIT_BUFFER_CAP]);
        assert_eq!(io.stderr.bytes, stderr[..AUDIT_BUFFER_CAP]);
        assert_eq!(io.stdout.total, stdout.len() as u64);
        assert_eq!(io.stderr.total, stderr.len() as u64);
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
        assert_eq!(io.stdout.bytes.len(), AUDIT_BUFFER_CAP);
        assert!(io.stdout.is_whole());
        assert_eq!(io.stderr, Retained::default());
    }
}
