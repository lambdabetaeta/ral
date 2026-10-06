//! Shared builtin argument, IO, and conversion helpers.

use crate::types::{Settled, Shell, Value, sig, sig_hint};

/// Arity floor for a builtin; `name` rides the error text.
///
/// # Errors
/// Returns `Err` if `args.len() < min`.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn check_arity(args: &[Value], min: usize, name: &str) -> Settled<()> {
    if args.len() < min {
        let noun = if min == 1 { "argument" } else { "arguments" };
        return Err(sig(format!("{name} requires {min} {noun}")));
    }
    Ok(())
}

/// Bytes written by number, one `Int` per byte — [`Value::as_bytes`] is the
/// other spelling, for a `Bytes` value already in hand.
pub(crate) fn as_byte_list(val: &Value, ctx: &str) -> Settled<Vec<u8>> {
    let items = val.as_list(ctx)?;
    let mut out = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        match item.as_ref() {
            Value::Int(n) => match u8::try_from(*n) {
                Ok(b) => out.push(b),
                Err(_) => {
                    return Err(sig_hint(
                        format!("{ctx}: byte at index {idx} out of range: {n}"),
                        "bytes must be Int values in range 0..255",
                    ));
                }
            },
            _ => {
                return Err(sig_hint(
                    format!(
                        "{ctx}: expected Int at index {idx}, got {}",
                        item.type_name()
                    ),
                    "bytes must be Int values in range 0..255",
                ));
            }
        }
    }
    Ok(out)
}

/// Resolve `path` against the `within [dir: …]` scoped cwd and capability-check
/// it for read — the move every fs query builtin opens with, so probing never
/// falls back to the OS cwd.
///
/// # Errors
/// Returns `Err` if the read capability check denies the resolved path.
pub fn checked_read_path(shell: &mut Shell, path: &str) -> Settled<crate::path::LexicalPath> {
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp)?;
    Ok(rp)
}

/// [`checked_read_path`] as a predicate, so a walk skips an off-limits entry
/// instead of aborting.
pub(crate) fn admits_read(shell: &mut Shell, path: &str) -> bool {
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp).is_ok()
}

/// The one stdin policy every reading builtin shares: an installed `Source`
/// (pipeline pipe or `<` redirect) if there is one; else a refusal when startup
/// stdin was a terminal, since these builtins want bytes and not a prompt; else
/// the inherited fd 0.  The [`super::codecs`] decoders and [`stdin_lines`]
/// both drain through here.
pub(crate) fn stdin_reader(name: &str, shell: &Shell) -> Settled<Box<dyn std::io::BufRead>> {
    // `Empty` is a deliberate no-input marker: immediate EOF, never the "no
    // input" error and never a fall-through to fd 0.
    if matches!(shell.io.stdin, crate::io::Source::Empty) {
        return Ok(Box::new(std::io::empty()));
    }
    if let Some(reader) = shell
        .io
        .stdin
        .reader()
        .map_err(|e| crate::types::Error::io("could not duplicate stdin", &e))?
    {
        return Ok(Box::new(std::io::BufReader::new(reader)));
    }
    if shell.io.terminal.startup_stdin_tty {
        return Err(sig(format!(
            "{name}: no input (pipe bytes or pass a value as argument)"
        )));
    }
    Ok(Box::new(std::io::stdin().lock()))
}

/// The one line reader: `reader` a line at a time, each stripped of its
/// [`crate::io::terminator_len`] and left undecoded, since decoding is each
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
                line.truncate(line.len() - crate::io::terminator_len(&line));
                Some(Ok(line))
            }
            Err(e) => Some(Err(sig(format!("{name}: {e}")))),
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
pub(crate) fn lossy_line_list(lines: impl Iterator<Item = Settled<Vec<u8>>>) -> Settled<Value> {
    let lines = lines
        .map(|line| {
            let line = String::from_utf8(line?)
                .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            Ok(Value::string(line))
        })
        .collect::<Settled<Vec<_>>>()?;
    Ok(Value::list(lines))
}

/// Dig the cause line out of the regex crate's multi-line parse error.
pub fn regex_err(ctx: &str, pattern: &str, full: &str) -> String {
    let cause = full
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("error:"))
        .and_then(|l| l.trim_start().strip_prefix("error:"))
        .map_or("invalid pattern", str::trim);
    format!("{ctx}: invalid pattern '{pattern}': {cause}")
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
}
