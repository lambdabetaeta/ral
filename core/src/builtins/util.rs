//! Shared builtin argument, IO, and conversion helpers.

use crate::types::{Break, Env, Error, HandleInner, Settled, Shell, Value, sig, sig_hint};
use std::sync::Arc;

/// `2^63`: the half-open upper bound an `f64` magnitude must stay under to be
/// representable as `i64`.  `i64::MAX` (`2^63 - 1`) rounds up to `2^63` as an
/// `f64`, so the comparison is strict against this value.
pub(crate) const I64_BOUND: f64 = 9_223_372_036_854_775_808.0;

/// Cast a finite, integral `f64` to `i64`, refusing a magnitude outside the
/// `i64` range rather than saturating it silently (`as i64` clamps to the
/// nearest bound, which would misreport the input).  `name` rides the error.
///
/// # Errors
/// Returns `Err` if `f` is not in `[-2^63, 2^63)`.
pub(crate) fn f64_to_i64(name: &str, f: f64) -> Settled<i64> {
    if (-I64_BOUND..I64_BOUND).contains(&f) {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "f is range-checked into [-2^63, 2^63) just above; the cast is exact"
        )]
        Ok(f as i64)
    } else {
        Err(sig(format!("{name}: {f} is outside the integer range")))
    }
}

/// Return an error if `args` has fewer than `min` elements.
///
/// # Errors
/// Returns `Err` if `args.len() < min`.
pub fn check_arity(args: &[Value], min: usize, name: &str) -> Settled<()> {
    if args.len() < min {
        let noun = if min == 1 { "argument" } else { "arguments" };
        return Err(sig(format!("{name} requires {min} {noun}")));
    }
    Ok(())
}

/// Extract a `HandleInner` reference from `val`, or return a typed error.
pub(crate) fn expect_handle<'a>(val: &'a Value, cmd: &str) -> Settled<&'a HandleInner> {
    match val {
        Value::Handle(h) => Ok(h),
        other => Err(Break::Error(
            Error::new(
                format!(
                    "{cmd} expects a Handle, got {} '{other}'",
                    other.type_name()
                ),
                1,
            )
            .with_hint("use spawn to create a handle"),
        )),
    }
}

/// Extract the body and captured scope from a `Block`, or return a typed error.
pub(crate) fn expect_thunk(val: &Value, cmd: &str) -> Settled<(Arc<crate::ir::Comp>, Arc<Env>)> {
    match val {
        Value::Block { body, captured } => Ok((Arc::clone(body), Arc::clone(captured))),
        other => Err(Break::Error(
            Error::new(
                format!("{cmd} expects a Block, got {} '{other}'", other.type_name()),
                1,
            )
            .with_hint(format!("{cmd} requires a block: {cmd} {{ ... }}")),
        )),
    }
}

pub(crate) fn decode_utf8_strict(bytes: Vec<u8>, context: &str, hint: &str) -> Settled<String> {
    String::from_utf8(bytes).map_err(|e| sig_hint(format!("{context}: {e}"), hint))
}

pub(crate) fn as_byte_list(val: &Value, ctx: &str) -> Settled<Vec<u8>> {
    if let Value::Bytes(b) = val {
        return Ok(b.clone());
    }
    let items = crate::types::as_list(val, ctx)?;
    let mut out = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        match item {
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

/// The closed taxonomy of comparable runtime values.  Comparison over a
/// computation value (Lambda/Block/Handle) is undeliverable — there is no
/// extensional equality on suspensions — so it is an error, never a silent
/// `false`.  `equal` and the ordering primitives share this judgment.
fn uncomparable(a: &Value, b: &Value, op: &str) -> Break {
    sig_hint(
        format!(
            "{op}: cannot compare {} with {}",
            a.type_name(),
            b.type_name()
        ),
        "comparison is defined on scalars, strings, bytes, lists, maps, and variants",
    )
}

/// Total structural equality over comparable values.  Mixed Int/Float
/// compares numerically; Bytes and Variant are structural; a computation
/// value on either side is uncomparable (errors rather than reporting
/// `false`, so reflexivity holds on every comparable value).
pub(crate) fn values_equal(a: &Value, b: &Value) -> Settled<bool> {
    Ok(match (a, b) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        #[allow(
            clippy::float_cmp,
            reason = "shell `==` is defined as bit-exact IEEE equality; epsilon would break reflexivity"
        )]
        (Value::Float(x), Value::Float(y)) => x == y,
        #[allow(
            clippy::cast_precision_loss,
            reason = "mixed Int/Float equality is defined by promoting the Int to f64; precision loss beyond 2^53 is intrinsic to cross-tower comparison"
        )]
        #[allow(
            clippy::float_cmp,
            reason = "shell `==` is defined as bit-exact IEEE equality; epsilon would break reflexivity"
        )]
        (Value::Int(x), Value::Float(y)) => (*x as f64) == *y,
        #[allow(
            clippy::cast_precision_loss,
            reason = "mixed Int/Float equality is defined by promoting the Int to f64; precision loss beyond 2^53 is intrinsic to cross-tower comparison"
        )]
        #[allow(
            clippy::float_cmp,
            reason = "shell `==` is defined as bit-exact IEEE equality; epsilon would break reflexivity"
        )]
        (Value::Float(x), Value::Int(y)) => *x == (*y as f64),
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Bytes(x), Value::Bytes(y)) => x == y,
        (Value::List(xs), Value::List(ys)) => {
            if xs.len() == ys.len() {
                // Comparability is checked pairwise before equality is
                // decided, so every pair's error surfaces regardless of
                // what an earlier (unrelated) pair happened to compare to.
                let pairwise = xs
                    .iter()
                    .zip(ys.iter())
                    .map(|(a, b)| values_equal(a, b))
                    .collect::<Settled<Vec<bool>>>()?;
                pairwise.into_iter().all(|eq| eq)
            } else {
                false
            }
        }
        (Value::Map(xs), Value::Map(ys)) => {
            // Both sides iterate sorted-by-key, so a single zip suffices:
            // either the key streams agree pointwise or the maps differ.
            // As with List, each pair's comparability is checked
            // independently of the others before equality is decided.
            if xs.len() == ys.len() {
                let pairwise = xs
                    .iter()
                    .zip(ys.iter())
                    .map(|((kx, vx), (ky, vy))| -> Settled<bool> {
                        Ok(kx == ky && values_equal(vx, vy)?)
                    })
                    .collect::<Settled<Vec<bool>>>()?;
                pairwise.into_iter().all(|eq| eq)
            } else {
                false
            }
        }
        (
            Value::Variant {
                label: lx,
                payload: px,
            },
            Value::Variant {
                label: ly,
                payload: py,
            },
        ) => {
            lx == ly
                && match (px, py) {
                    (Some(x), Some(y)) => values_equal(x, y)?,
                    (None, None) => true,
                    _ => false,
                }
        }
        (Value::Lambda { .. } | Value::Block { .. } | Value::Handle(_), _)
        | (_, Value::Lambda { .. } | Value::Block { .. } | Value::Handle(_)) => {
            return Err(uncomparable(a, b, "equal"));
        }
        _ => false,
    })
}

/// Total ordering over the comparison taxonomy: Int·Int as `i64` (no f64
/// precision loss above 2^53), Int/Float lifted to f64, String·String
/// lexicographic.  NaN and every other pairing are uncomparable.  Backs
/// `lt`/`gt`, `sort-list`, and the `$[…]` ordering operators, so the
/// expression evaluator and the builtins cannot drift.
pub(crate) fn value_ordering(a: &Value, b: &Value, op: &str) -> Settled<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            let (x, y) = (a.as_float().unwrap(), b.as_float().unwrap());
            x.partial_cmp(&y)
                .ok_or_else(|| sig(format!("{op}: cannot order NaN")))
        }
        (Value::String(x), Value::String(y)) => Ok(x.cmp(y)),
        _ => Err(sig_hint(
            format!(
                "{op}: cannot compare {} with {}",
                a.type_name(),
                b.type_name()
            ),
            "ordering is defined on numbers and strings",
        )),
    }
}

pub(crate) fn order_cmp(
    args: &[Value],
    shell: &mut Shell,
    name: &str,
    want: fn(std::cmp::Ordering) -> bool,
) -> Settled<Value> {
    check_arity(args, 2, name)?;
    let r = want(value_ordering(&args[0], &args[1], name)?);
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

/// Render the first argument as a `String`.
///
/// # Errors
/// Returns `Err` if `args` is empty.
pub fn arg0_str(args: &[Value], name: &str) -> Settled<String> {
    check_arity(args, 1, name)?;
    Ok(args[0].to_string())
}

/// Resolve `path` against the within-scoped cwd and capability-check it for
/// read, returning the resolved path.
///
/// The resolve+`check_fs_read` idiom every fs query builtin opens with, so
/// probing honours `within [dir: …]` rather than resolving against the OS cwd.
///
/// # Errors
/// Returns `Err` if the read capability check denies the resolved path.
pub fn checked_read_path(shell: &mut Shell, path: &str) -> Settled<crate::path::ResolvedPath> {
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp)?;
    Ok(rp)
}

/// True if `path` resolves and passes the read capability check — the
/// skip-denied test a directory-walk loop uses to drop an off-limits entry
/// without aborting the whole walk.
pub fn admits_read(shell: &mut Shell, path: &str) -> bool {
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp).is_ok()
}

/// Resolve the shell's stdin to one buffered byte reader, applying the
/// stdin-draining policy every reading builtin shares: prefer the shell's
/// installed `Source` reader (a pipeline pipe or a `<` redirect); else
/// error if startup stdin was a terminal (the reading builtins want bytes,
/// not an interactive prompt); else fall back to the process's
/// `stdin().lock()` (the script-run case where stdin is inherited but no
/// `Source` was installed).  Both the byte-blob reader
/// ([`super::codecs::read_stdin_bytes`]) and the line iterator
/// ([`for_each_stdin_line`]) drain through it, so the three-arm policy
/// lives in one place.  `name` rides the no-input error.
pub(crate) fn stdin_reader(name: &str, shell: &mut Shell) -> Settled<Box<dyn std::io::BufRead>> {
    // An explicit empty source reads as immediate EOF — no fd-0 fall-through,
    // and no "no input" error (the run deliberately installed no input).
    if matches!(shell.io.stdin, crate::io::Source::Empty) {
        return Ok(Box::new(std::io::empty()));
    }
    if let Some(reader) = shell.io.stdin.take_reader() {
        return Ok(Box::new(std::io::BufReader::new(reader)));
    }
    if shell.io.terminal.startup_stdin_tty {
        return Err(sig(format!(
            "{name}: no input (pipe bytes or pass a value as argument)"
        )));
    }
    Ok(Box::new(std::io::stdin().lock()))
}

/// Iterate the shell's stdin one line at a time, invoking `f` per line.
/// `name` rides the error messages.
///
/// # Errors
/// Returns `Err` if stdin cannot be resolved (see [`stdin_reader`]), if
/// reading a line fails, or if `f` returns `Err`.
pub fn for_each_stdin_line(
    name: &str,
    shell: &mut Shell,
    mut f: impl FnMut(String, &mut Shell) -> Settled<()>,
) -> Settled<()> {
    use std::io::BufRead;
    let reader = stdin_reader(name, shell)?;
    for line in reader.lines() {
        f(line.map_err(|e| sig(format!("{name}: {e}")))?, shell)?;
    }
    Ok(())
}

#[cfg(feature = "grep")]
/// Render the useful part of a regex parser error for a builtin surface.
pub fn regex_err(ctx: &str, pattern: &str, full: &str) -> String {
    let cause = full
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("error:"))
        .and_then(|l| l.trim_start().strip_prefix("error:"))
        .map_or("invalid pattern", str::trim);
    format!("{ctx}: invalid pattern '{pattern}': {cause}")
}

/// Total, never-failing JSON projection for the `--audit` dump.
///
/// Byte fields render as lossy-UTF-8 strings, non-finite Floats fall to
/// `null`, and computation values become type-tagged stubs — legibility
/// over round-trip fidelity.
pub fn value_to_json_lossy_bytes(v: &Value) -> serde_json::Value {
    match v {
        Value::Unit => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => serde_json::json!(*n),
        Value::Float(f) => serde_json::json!(*f),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::List(items) => {
            serde_json::Value::Array(items.iter().map(value_to_json_lossy_bytes).collect())
        }
        Value::Map(pairs) => serde_json::Value::Object(
            pairs
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json_lossy_bytes(v)))
                .collect(),
        ),
        Value::Lambda { param, .. } => {
            serde_json::json!({"type": "Lambda", "param": format!("{param:?}")})
        }
        Value::Block { .. } => serde_json::json!({"type": "Block"}),
        Value::Handle(_) => serde_json::json!({"type": "Handle"}),
        Value::Bytes(b) => serde_json::Value::String(String::from_utf8_lossy(b).into_owned()),
        Value::Variant { label, payload } => {
            let mut obj = serde_json::Map::new();
            obj.insert("tag".into(), serde_json::Value::String(label.clone()));
            if let Some(p) = payload {
                obj.insert("payload".into(), value_to_json_lossy_bytes(p));
            }
            serde_json::Value::Object(obj)
        }
    }
}

#[cfg(test)]
mod stdin_tests {
    use super::stdin_reader;
    use crate::io::Source;
    use crate::types::Shell;
    use std::io::Read;

    /// An explicit empty stdin source reads as immediate EOF — not the "no
    /// input" error and, crucially, *not* a fall-through to fd 0. This is the
    /// guarantee an exarch tool run (`RunStdin::Empty`) relies on so a tool
    /// command that reads stdin can never steal the TUI's controlling terminal.
    #[test]
    fn empty_source_reads_as_eof() {
        let mut shell = Shell::default();
        shell.io.stdin = Source::Empty;
        let mut reader = stdin_reader("test", &mut shell).expect("Empty must not error");
        let mut buf = Vec::new();
        let n = reader.read_to_end(&mut buf).expect("read");
        assert_eq!(n, 0, "Empty source yields no bytes");
        assert!(buf.is_empty());
        // The source is a persistent marker: a second read still sees Empty,
        // never collapsing to `Terminal` (fd-0 fall-through).
        assert!(matches!(shell.io.stdin, Source::Empty));
    }
}
