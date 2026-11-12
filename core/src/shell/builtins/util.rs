//! Shared builtin argument, IO, and conversion helpers.

use crate::error::Error;
use crate::fact::{
    Expected, InvalidCause, InvalidFailure, IoOperation, IoSubject, Number, NumberRange,
    OperationState, Reason, RegexCause, RegexFailure, ShapeFailure, StandardStream, StateFailure,
    ValueShape,
};
use crate::io::Source;
use crate::shell::runtime::{Settled, Shell, ValClos, View};

/// Bytes written by number, one `Int` per byte — [`ValClos::as_bytes`] is the
/// other spelling, for a `Bytes` value already in hand.
pub(crate) fn as_byte_list(val: &ValClos, ctx: &str) -> Settled<Vec<u8>> {
    let items = val.as_list(ctx)?;
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let cause = match item.view() {
                View::Int(n) => match u8::try_from(n) {
                    Ok(byte) => return Ok(byte),
                    Err(_) => InvalidCause::Range(NumberRange {
                        value: Number::Int(n),
                        minimum: Number::Int(0),
                        maximum: Number::Int(255),
                    }),
                },
                _ => InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Int),
                    found: item.found(),
                }),
            };
            Err(Error::new(Reason::Invalid(Box::new(
                InvalidFailure::new(ctx.into(), cause).index(index.into()),
            )))
            .into())
        })
        .collect()
}

/// The one stdin policy every reading builtin shares: the installed reader
/// (a pipe, a `<` redirect, or a non-tty fd 0), EOF for `Empty`, and a
/// refusal for a terminal, since these builtins want bytes and not a prompt.
/// The [`super::codecs`] decoders and [`stdin_lines`] both drain through
/// here.
pub(crate) fn stdin_reader(name: &str, shell: &Shell) -> Settled<Box<dyn std::io::BufRead>> {
    if matches!(shell.io.stdin, Source::Terminal) {
        return Err(Error::new(Reason::State(Box::new(StateFailure::NoByteInput(
            OperationState {
                operation: name.into(),
            },
        ))))
        .into());
    }
    let Some(reader) = shell.io.stdin.reader().map_err(|e| {
        Error::io(
            IoOperation::Duplicate,
            IoSubject::Stream(StandardStream::Stdin),
            &e,
        )
    })?
    else {
        return Ok(Box::new(std::io::empty()));
    };
    // Re-scoped to the scope in focus, so a cleanup's reads are masked with it.
    let reader = reader.interruptible(shell.scope.clone());
    Ok(Box::new(std::io::BufReader::new(reader)))
}

/// The one line reader: `reader` a line at a time, each stripped by
/// [`crate::io::without_terminator`] and left undecoded, since decoding is each
/// caller's policy.  The final line need not be terminated.
pub(crate) fn read_lines<R: std::io::BufRead>(
    name: &'static str,
    mut reader: R,
) -> impl Iterator<Item = Settled<Vec<u8>>> {
    std::iter::from_fn(move || {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => None,
            Ok(_) => {
                line.truncate(crate::io::without_terminator(&line).len());
                Some(Ok(line))
            }
            Err(e) => Some(Err(Error::io(
                IoOperation::Read,
                IoSubject::Stream(StandardStream::Stdin),
                &e,
            )
            .with_context(name)
            .into())),
        }
    })
}

/// [`read_lines`] over the shell's stdin, through [`stdin_reader`].
///
/// # Errors
/// Returns `Err` if stdin cannot be resolved; each item, if its read fails.
pub(crate) fn stdin_lines(
    name: &'static str,
    shell: &Shell,
) -> Settled<impl Iterator<Item = Settled<Vec<u8>>> + use<>> {
    Ok(read_lines(name, stdin_reader(name, shell)?))
}

/// [`read_lines`]' lines as a list of Strings, each decoded lossily so a line
/// survives invalid bytes.  Decoded per line, the text is never held whole;
/// no invalid sequence spans a `\n`, so the result is the same.
///
/// # Errors
/// The first failed read.
pub(crate) fn lossy_line_list(lines: impl Iterator<Item = Settled<Vec<u8>>>) -> Settled<ValClos> {
    let lines = lines
        .map(|line| {
            let line = String::from_utf8(line?)
                .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            Ok(ValClos::string(line))
        })
        .collect::<Settled<Vec<_>>>()?;
    Ok(ValClos::list(lines))
}

/// Classify the regex compiler's refusal before rendering it.
pub fn regex_failure(pattern: &str, error: &regex::Error) -> RegexFailure {
    RegexFailure {
        pattern: pattern.into(),
        cause: match error {
            regex::Error::Syntax(detail) => RegexCause::Syntax(detail.clone()),
            regex::Error::CompiledTooBig(limit) => RegexCause::CompiledTooBig((*limit).into()),
            other => RegexCause::Unclassified(other.to_string()),
        },
    }
}

#[cfg(test)]
mod stdin_tests {
    use super::stdin_reader;
    use crate::io::Source;
    use std::io::Read;

    /// The guarantee an exarch tool run (`RunStdin::Empty`) rests on: a tool
    /// command reading stdin can never steal the TUI's controlling terminal.
    #[test]
    fn empty_source_reads_as_eof() {
        let mut shell = crate::test_helper::core_shell();
        shell.io.stdin = Source::Empty;
        let mut reader = stdin_reader("test", &shell).expect("Empty must not error");
        let mut buf = Vec::new();
        let n = reader.read_to_end(&mut buf).expect("read");
        assert_eq!(n, 0, "Empty source yields no bytes");
        assert_eq!(buf, Vec::<u8>::new());
        // A persistent marker: a second read still sees `Empty`, never
        // collapsing to `Terminal` and its fd-0 fall-through.
        assert!(matches!(shell.io.stdin, Source::Empty));
    }

    /// `Terminal` means a terminal: a reading builtin refuses it rather than
    /// wait on a prompt, whatever the startup probe said.
    #[test]
    fn a_terminal_source_is_refused() {
        let mut shell = crate::test_helper::core_shell();
        shell.io.stdin = Source::Terminal;
        let Err(refused) = stdin_reader("from-lines", &shell) else {
            panic!("a terminal stdin must be refused");
        };
        assert!(
            matches!(refused, crate::shell::runtime::Break::Error(error) if matches!(&error.reason, crate::fact::Reason::State(failure) if matches!(**failure, crate::fact::StateFailure::NoByteInput(_))))
        );
    }

    /// A non-tty fd 0 reaches a reading builtin as the reader it became.
    #[cfg(unix)]
    #[test]
    fn a_reader_source_is_read() {
        use std::io::Write;
        let (r, mut w) = crate::process::cloexec_pipe().expect("data pipe");
        w.write_all(b"bytes").expect("write");
        drop(w);
        let mut shell = crate::test_helper::core_shell();
        shell.io.stdin = Source::Reader(crate::io::SourceReader::pipe(r));
        let mut buf = String::new();
        stdin_reader("test", &shell)
            .expect("a reader is read")
            .read_to_string(&mut buf)
            .expect("read");
        assert_eq!(buf, "bytes");
    }

    #[test]
    fn read_lines_preserves_the_io_cause_and_caller_context() {
        struct FailedRead;
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::PermissionDenied.into())
            }
        }
        let Err(crate::shell::runtime::Break::Error(error)) =
            super::read_lines("from-lines", std::io::BufReader::new(FailedRead))
                .next()
                .expect("failed read")
        else {
            panic!("read refusal")
        };
        assert_eq!(error.context(), Some("from-lines"));
        let crate::fact::Reason::Io(failure) = error.reason else {
            panic!("I/O reason")
        };
        assert_eq!(failure.operation, crate::fact::IoOperation::Read);
        assert_eq!(
            failure.subject,
            crate::fact::IoSubject::Stream(crate::fact::StandardStream::Stdin)
        );
        assert_eq!(
            failure.cause,
            crate::io::failure(&std::io::ErrorKind::PermissionDenied.into())
        );
    }
}
