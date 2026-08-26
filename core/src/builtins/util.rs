//! Shared builtin argument, IO, and conversion helpers.

use crate::types::{
    Break, Env, Error, HandleInner, Settled, Shell, Value, fmt_float, sig, sig_hint,
};
use std::sync::Arc;

/// `i64::MAX` is not itself an `f64` — it rounds up to exactly this, so the
/// magnitude bound is strict.
pub(crate) const I64_BOUND: f64 = 9_223_372_036_854_775_808.0;

/// Cast an integral `f64` to `i64`, refusing what `as i64` would silently saturate.
pub(crate) fn f64_to_i64(name: &str, f: f64) -> Settled<i64> {
    if (-I64_BOUND..I64_BOUND).contains(&f) {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "f is range-checked into [-2^63, 2^63) just above; the cast is exact"
        )]
        Ok(f as i64)
    } else {
        Err(sig(format!(
            "{name}: {} is outside the integer range",
            fmt_float(f)
        )))
    }
}

/// Arity floor for a builtin; `name` rides the error text.
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

pub(crate) fn expect_thunk(val: &Value, cmd: &str) -> Settled<(Arc<crate::ir::Comp>, Arc<Env>)> {
    match val {
        // A spawn body takes no parameters: `comp.arrow()` is `None` for a
        // block-shaped thunk.
        Value::Thunk(closure) if closure.comp.arrow().is_none() => {
            Ok((Arc::clone(&closure.comp), Arc::new(closure.env.clone())))
        }
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

pub(crate) fn as_bytes<'v>(val: &'v Value, ctx: &str) -> Settled<&'v [u8]> {
    match val {
        Value::Bytes(b) => Ok(b),
        other => Err(sig_hint(
            format!("{ctx}: expected Bytes, got {}", other.type_name()),
            "a list of numbers is `ints-to-bytes`; a Bytes value comes from `from-bytes`",
        )),
    }
}

/// Bytes written by number, one `Int` per byte — [`as_bytes`] is the other
/// spelling, for a `Bytes` value already in hand.
pub(crate) fn as_byte_list(val: &Value, ctx: &str) -> Settled<Vec<u8>> {
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

/// Suspensions have no extensional equality, so asking errors rather than
/// answering a non-reflexive `false`.
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

/// Structural equality, shared by `equal` and by `==`/`!=` in `$[…]`.
pub(crate) fn values_equal(a: &Value, b: &Value) -> Settled<bool> {
    Ok(match (a, b) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        #[allow(
            clippy::float_cmp,
            reason = "a Float is finite by construction, so IEEE `==` is reflexive here; epsilon would break that"
        )]
        (Value::Float(x), Value::Float(y)) => x == y,
        #[allow(
            clippy::cast_precision_loss,
            reason = "mixed Int/Float equality is defined by promoting the Int to f64; precision loss beyond 2^53 is intrinsic to cross-tower comparison"
        )]
        #[allow(
            clippy::float_cmp,
            reason = "a Float is finite by construction, so IEEE `==` is reflexive here; epsilon would break that"
        )]
        (Value::Int(x), Value::Float(y)) => (*x as f64) == *y,
        #[allow(
            clippy::cast_precision_loss,
            reason = "mixed Int/Float equality is defined by promoting the Int to f64; precision loss beyond 2^53 is intrinsic to cross-tower comparison"
        )]
        #[allow(
            clippy::float_cmp,
            reason = "a Float is finite by construction, so IEEE `==` is reflexive here; epsilon would break that"
        )]
        (Value::Float(x), Value::Int(y)) => *x == (*y as f64),
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Bytes(x), Value::Bytes(y)) => x == y,
        (Value::List(xs), Value::List(ys)) => {
            if xs.len() == ys.len() {
                // Collect before folding: a short-circuiting `all` would let an
                // early `false` mask a later pair's uncomparability.
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
            // Both sides iterate sorted by key, so a pointwise zip decides it.
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
        // A name is an intensional identity a closure lacks, so natives
        // compare where lambdas refuse; a collected lambda argument still
        // surfaces the refusal.
        (
            Value::Native {
                entry: ea,
                applied: aa,
            },
            Value::Native {
                entry: eb,
                applied: ab,
            },
        ) => {
            if ea.name != eb.name || aa.len() != ab.len() {
                false
            } else {
                let pairwise = aa
                    .iter()
                    .zip(ab.iter())
                    .map(|(x, y)| values_equal(x, y))
                    .collect::<Settled<Vec<bool>>>()?;
                pairwise.into_iter().all(|eq| eq)
            }
        }
        (Value::Thunk(_) | Value::Handle(_), _) | (_, Value::Thunk(_) | Value::Handle(_)) => {
            return Err(uncomparable(a, b, "equal"));
        }
        _ => false,
    })
}

/// Ordering on numbers and strings.  Int·Int compares as `i64`, so integers
/// past 2^53 order exactly rather than through f64.  Backs `lt`/`gt`,
/// `sort-list`, and the `$[…]` comparisons, which therefore cannot drift.
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
    let r = want(value_ordering(&args[0], &args[1], name)?);
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

/// Render the first argument as a `String`; application already gated the
/// count for every fixed-arity-1 caller.
pub fn arg0_str(args: &[Value]) -> String {
    args[0].to_string()
}

/// Resolve `path` against the `within [dir: …]` scoped cwd and capability-check
/// it for read — the move every fs query builtin opens with, so probing never
/// falls back to the OS cwd.
///
/// # Errors
/// Returns `Err` if the read capability check denies the resolved path.
pub fn checked_read_path(shell: &mut Shell, path: &str) -> Settled<crate::path::ResolvedPath> {
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp)?;
    Ok(rp)
}

/// [`checked_read_path`] as a predicate, so a walk skips an off-limits entry
/// instead of aborting.
pub fn admits_read(shell: &mut Shell, path: &str) -> bool {
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp).is_ok()
}

/// The one stdin policy every reading builtin shares: an installed `Source`
/// (pipeline pipe or `<` redirect) if there is one; else a refusal when startup
/// stdin was a terminal, since these builtins want bytes and not a prompt; else
/// the inherited fd 0.  The [`super::codecs`] decoders and
/// [`for_each_stdin_line`] both drain through here.
pub(crate) fn stdin_reader(name: &str, shell: &mut Shell) -> Settled<Box<dyn std::io::BufRead>> {
    // `Empty` is a deliberate no-input marker: immediate EOF, never the "no
    // input" error and never a fall-through to fd 0.
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

/// Iterate the shell's stdin line by line through [`stdin_reader`].
///
/// # Errors
/// Returns `Err` if stdin cannot be resolved, if a read fails, or if `f` does.
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

/// Total JSON projection for the `--audit` dump: lossy UTF-8 for Bytes, `null`
/// for non-finite Floats, type-tagged stubs for suspensions.  Legibility over
/// round-trip fidelity — none of it decodes back.
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
        Value::Thunk(c) => match c.comp.arrow() {
            Some((param, _)) => serde_json::json!({"type": "Lambda", "param": format!("{param:?}")}),
            None => serde_json::json!({"type": "Block"}),
        },
        Value::Native { entry, applied } => {
            serde_json::json!({"type": "Native", "name": entry.name.as_ref(), "applied": applied.len()})
        }
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

    /// The guarantee an exarch tool run (`RunStdin::Empty`) rests on: a tool
    /// command reading stdin can never steal the TUI's controlling terminal.
    #[test]
    fn empty_source_reads_as_eof() {
        let mut shell = Shell::default();
        shell.io.stdin = Source::Empty;
        let mut reader = stdin_reader("test", &mut shell).expect("Empty must not error");
        let mut buf = Vec::new();
        let n = reader.read_to_end(&mut buf).expect("read");
        assert_eq!(n, 0, "Empty source yields no bytes");
        assert!(buf.is_empty());
        // A persistent marker: a second read still sees `Empty`, never
        // collapsing to `Terminal` and its fd-0 fall-through.
        assert!(matches!(shell.io.stdin, Source::Empty));
    }
}
