//! Captures: run a closure with the shell's output going into buffers.
//! [`with_capture`] replaces stdout, so the bytes can become a value.
//! [`with_audit_capture`] tees stdout and stderr, so the bytes are recorded
//! and still reach their destination.
use crate::fact::AuditIo;
use crate::io::{IoPatch, Retained, Streams, new_buffer, tee_with_buffer};
use crate::shell::runtime::Shell;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

/// Cap on the bytes an audit record keeps per stream.
const AUDIT_BUFFER_CAP: usize = 1024 * 1024;

/// Run `f` with stdout going into a buffer.  Returns `f`'s result and what
/// the buffer kept.
///
/// The buffer is capped, and the write path cannot report the cut.  A caller
/// that turns the bytes into a value must check [`Retained::is_whole`].
pub fn with_capture<R>(shell: &mut Shell, f: impl FnOnce(&mut Shell) -> R) -> (R, Retained) {
    let (sink, buffer) = new_buffer();
    let patch = IoPatch {
        stdout: Some(sink),
        ..IoPatch::default()
    };
    let result = with_io_patched(shell, patch, f);
    (result, buffer.take())
}

/// Run `f` with stdout and stderr each teed into a buffer, if the open
/// audit records bytes.  Returns `f`'s result and what the buffers kept.
///
/// `Command::with_record` wraps every external in this.  Under byte audit a
/// pipeline stage always runs on a thread with a shell (`pipeline::resolve`),
/// so no external escapes it.
pub(crate) fn with_audit_capture<R>(
    shell: &mut Shell,
    f: impl FnOnce(&mut Shell) -> R,
) -> (R, AuditIo) {
    if !shell.local.audit.captures_bytes() {
        return (f(shell), AuditIo::default());
    }
    let (sinks, buffers) = Streams {
        stdout: shell.io.stdout.clone(),
        stderr: shell.io.stderr.clone(),
    }
    .map(|destination| tee_with_buffer(destination, AUDIT_BUFFER_CAP))
    .unzip();
    let patch = IoPatch {
        stdout: Some(sinks.stdout),
        stderr: Some(sinks.stderr),
        ..IoPatch::default()
    };
    let result = with_io_patched(shell, patch, f);
    (result, buffers.map(|buffer| buffer.take()))
}

/// Run `f` with `patch` installed over the shell's streams.  The replaced
/// streams are restored when `f` returns or panics.
fn with_io_patched<R>(shell: &mut Shell, patch: IoPatch, f: impl FnOnce(&mut Shell) -> R) -> R {
    let prior = shell.io.swap(patch);
    let result = catch_unwind(AssertUnwindSafe(|| f(shell)));
    shell.io.restore(prior);
    result.unwrap_or_else(|payload| resume_unwind(payload))
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
}
