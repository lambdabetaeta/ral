//! Shared builtin argument, IO, and conversion helpers.

use crate::types::{Value, Settled, sig, HandleInner, Break, Error, Env, sig_hint, List, Shell};
use std::sync::Arc;

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

/// Extract the elements of a `List`, or return a typed error.
///
/// # Errors
/// Returns `Err` if `val` is not a `List`.
pub fn as_list(val: &Value, ctx: &str) -> Settled<List> {
    match val {
        Value::List(items) => Ok(items.clone()),
        _ => Err(sig(format!(
            "{ctx} expects a List, got {}",
            val.type_name()
        ))),
    }
}

/// Borrow the underlying `Map`, or return a typed error.
///
/// # Errors
/// Returns `Err` if `val` is not a `Map`.
pub fn as_map<'a>(val: &'a Value, ctx: &str) -> Settled<&'a crate::types::Map> {
    match val {
        Value::Map(m) => Ok(m),
        _ => Err(sig(format!("{ctx} expects a Map, got {}", val.type_name()))),
    }
}

pub(crate) fn as_byte_list(val: &Value, ctx: &str) -> Settled<Vec<u8>> {
    if let Value::Bytes(b) = val {
        return Ok(b.clone());
    }
    let items = as_list(val, ctx)?;
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
        (Value::Float(x), Value::Float(y)) => x == y,
        #[allow(
            clippy::cast_precision_loss,
            reason = "mixed Int/Float equality is defined by promoting the Int to f64; precision loss beyond 2^53 is intrinsic to cross-tower comparison"
        )]
        (Value::Int(x), Value::Float(y)) => (*x as f64) == *y,
        #[allow(
            clippy::cast_precision_loss,
            reason = "mixed Int/Float equality is defined by promoting the Int to f64; precision loss beyond 2^53 is intrinsic to cross-tower comparison"
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
        _ => Err(uncomparable(a, b, op)),
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
    // and no "no input" error (the turn deliberately installed no input).
    if matches!(shell.turn.io.stdin, crate::io::Source::Empty) {
        return Ok(Box::new(std::io::empty()));
    }
    if let Some(reader) = shell.turn.io.stdin.take_reader() {
        return Ok(Box::new(std::io::BufReader::new(reader)));
    }
    if shell.turn.io.terminal.startup_stdin_tty {
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

pub(crate) fn json_to_value(j: &serde_json::Value) -> Settled<Value> {
    Ok(match j {
        serde_json::Value::Null => Value::Unit,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if n.is_f64() {
                // A genuine JSON float; `as_f64` cannot fail here.
                Value::Float(n.as_f64().unwrap())
            } else {
                // An integer literal that overflowed `i64` (a `u64` above
                // `i64::MAX`).  Reading it as `f64` would silently round
                // away its low bits, so refuse rather than corrupt it.
                return Err(sig(format!(
                    "from-json: integer {n} is outside the supported range"
                )));
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => {
            Value::list(arr.iter().map(json_to_value).collect::<Settled<Vec<_>>>()?)
        }
        serde_json::Value::Object(obj) => Value::Map(
            obj.iter()
                .map(|(k, v)| Ok((k.clone(), json_to_value(v)?)))
                .collect::<Settled<_>>()?,
        ),
    })
}

/// Encode `v` as JSON for `to-json`.
///
/// A typed byte↔value crossing must
/// refuse what it cannot faithfully represent rather than erase it: a
/// non-finite Float (NaN / ±Infinity) has no JSON number, and a
/// computation value (Lambda / Block / Handle) has no data shape, so each
/// errors.  Bytes render as an integer array, the form `from-bytes`
/// round-trips.  Mirrors `from-json`, which errors on the analogous shape
/// mistake on the way in.
///
/// # Errors
/// Returns `Err` if `v`, or any value nested within it, is a non-finite
/// `Float` (NaN / ±Infinity) or a computation value (`Lambda` / `Block` /
/// `Handle`).
pub fn value_to_json(v: &Value) -> Settled<serde_json::Value> {
    Ok(match v {
        Value::Unit => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => serde_json::json!(*n),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| sig(format!("to-json: {f} has no JSON representation")))?,
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::List(items) => {
            serde_json::Value::Array(items.iter().map(value_to_json).collect::<Settled<_>>()?)
        }
        Value::Map(pairs) => {
            let obj: serde_json::Map<String, serde_json::Value> = pairs
                .iter()
                .map(|(k, v)| Ok((k.clone(), value_to_json(v)?)))
                .collect::<Settled<_>>()?;
            serde_json::Value::Object(obj)
        }
        Value::Lambda { .. } | Value::Block { .. } | Value::Handle(_) => {
            return Err(sig(format!(
                "to-json: {} has no JSON representation",
                v.type_name()
            )));
        }
        Value::Bytes(b) => {
            serde_json::Value::Array(b.iter().map(|byte| serde_json::json!(*byte)).collect())
        }
        Value::Variant { label, payload } => {
            let mut obj = serde_json::Map::new();
            obj.insert("tag".into(), serde_json::Value::String(label.clone()));
            if let Some(p) = payload {
                obj.insert("payload".into(), value_to_json(p)?);
            }
            serde_json::Value::Object(obj)
        }
    })
}

/// Variant for the `--audit` JSON dump: total, never failing.
///
/// Byte fields
/// render as lossy-UTF-8 strings rather than integer arrays so the
/// execution tree stays readable, non-finite Floats fall to `null`, and
/// computation values become type-tagged stubs.  The audit tree is a
/// debug surface, so legibility wins over round-trip fidelity here.
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
    /// guarantee an exarch tool turn (`TurnStdin::Empty`) relies on so a tool
    /// command that reads stdin can never steal the TUI's controlling terminal.
    #[test]
    fn empty_source_reads_as_eof() {
        let mut shell = Shell::default();
        shell.turn.io.stdin = Source::Empty;
        let mut reader = stdin_reader("test", &mut shell).expect("Empty must not error");
        let mut buf = Vec::new();
        let n = reader.read_to_end(&mut buf).expect("read");
        assert_eq!(n, 0, "Empty source yields no bytes");
        assert!(buf.is_empty());
        // The source is a persistent marker: a second read still sees Empty,
        // never collapsing to `Terminal` (fd-0 fall-through).
        assert!(matches!(shell.turn.io.stdin, Source::Empty));
    }
}
