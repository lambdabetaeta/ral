//! Byte-channel codecs, one builtin per `from-X` / `to-X` rather than a
//! name-dispatched `codec <name>`: the typechecker then sees each one's real
//! return type, and a misspelling fails at command lookup.  Decoders are
//! nullary; where their bytes come from is [`super::util::stdin_reader`]'s
//! policy.

use crate::ir::{CompKind, Val};
use crate::source::Spanned;
use crate::stream::{DONE_LABEL, HEAD_FIELD, MORE_LABEL, TAIL_FIELD};
use crate::types::{Env, Settled, Shell, Value, as_list, as_map_ref, sig, sig_hint};
use std::sync::Arc;

use super::util::{as_byte_list, check_arity, decode_utf8_strict};

fn read_stdin_bytes(name: &str, shell: &mut Shell) -> Settled<Vec<u8>> {
    use std::io::Read;

    let mut bytes = Vec::new();
    super::util::stdin_reader(name, shell)?
        .read_to_end(&mut bytes)
        .map_err(|e| sig(format!("{name}: {e}")))?;
    Ok(bytes)
}

/// Channel bytes for a `from-X` decoder.  The typechecker rejects a written
/// argument outright, so this guard is for spread calls, whose arity it
/// cannot see ([`crate::ir::args::positional`] gives up on them).
fn input_bytes(args: &[Value], name: &str, shell: &mut Shell) -> Settled<Vec<u8>> {
    if !args.is_empty() {
        return Err(sig_hint(
            format!("{name}: takes no arguments — it reads the byte channel"),
            "to decode a value in hand, pipe it through the matching encoder: `to-string $x | from-json`",
        ));
    }
    read_stdin_bytes(name, shell)
}

pub(super) fn builtin_from_bytes(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    Ok(Value::Bytes(input_bytes(args, "from-bytes", shell)?))
}

pub(super) fn builtin_from_string(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bytes = input_bytes(args, "from-string", shell)?;
    Ok(Value::String(decode_utf8_strict(
        bytes,
        "from-string: input is not valid UTF-8",
        "use from-bytes to keep raw bytes",
    )?))
}

pub(super) fn builtin_from_line(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bytes = input_bytes(args, "from-line", shell)?;
    let text = decode_utf8_strict(
        bytes,
        "from-line: input is not valid UTF-8",
        "use from-bytes to keep raw bytes",
    )?;
    Ok(Value::String(
        crate::io::str_strip_one_terminator(&text).to_owned(),
    ))
}

fn stream_cons(head: String, tail: Value) -> Value {
    let mut captured = Env::new();
    captured.set("__stream_tail".into(), tail);
    let body = Arc::new(Spanned::synthetic(CompKind::Return(Val::Variable(
        "__stream_tail".into(),
    ))));
    Value::Variant {
        label: MORE_LABEL.into(),
        payload: Some(Box::new(Value::map(vec![
            (HEAD_FIELD.into(), Value::String(head)),
            (
                TAIL_FIELD.into(),
                Value::Block {
                    body,
                    captured: Arc::new(captured),
                },
            ),
        ]))),
    }
}

/// Decode the channel into a `` `more``/`` `done`` Stream of lines, decoding
/// lossily so a line stream survives invalid bytes.  Only the shape is lazy:
/// the channel is read to EOF and every node built before this returns, so a
/// downstream `stream-take 3` still drains an unbounded source.
pub(super) fn builtin_from_lines(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bytes = input_bytes(args, "from-lines", shell)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let mut s = Value::Variant {
        label: DONE_LABEL.into(),
        payload: None,
    };
    for line in text.lines().rev() {
        s = stream_cons(line.to_owned(), s);
    }
    Ok(s)
}

fn json_to_value(j: &serde_json::Value) -> Settled<Value> {
    Ok(match j {
        serde_json::Value::Null => Value::Unit,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if n.is_f64() {
                // `is_f64` just held.
                Value::Float(n.as_f64().unwrap())
            } else {
                // A `u64` above `i64::MAX`: `f64` would round away its low
                // bits, so refuse rather than corrupt it.
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

pub(super) fn builtin_from_json(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bytes = input_bytes(args, "from-json", shell)?;
    let text = decode_utf8_strict(
        bytes,
        "from-json: input is not valid UTF-8",
        "use from-bytes to keep raw bytes",
    )?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| sig(format!("from-json: {e}")))?;
    json_to_value(&json)
}

/// Decode CSV into a list of records keyed by the header row; fields stay
/// `String`, since CSV is untyped.  A duplicate header is refused rather than
/// resolved last-write-wins — a record cannot hold two columns of one name.
pub(super) fn builtin_from_csv(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bytes = input_bytes(args, "from-csv", shell)?;
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(bytes.as_slice());
    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| sig(format!("from-csv: {e}")))?
        .iter()
        .map(str::to_owned)
        .collect();
    let mut seen = std::collections::HashSet::with_capacity(headers.len());
    for h in &headers {
        if !seen.insert(h) {
            return Err(sig(format!("from-csv: duplicate header column {h:?}")));
        }
    }
    let mut rows = Vec::new();
    for record in rdr.records() {
        let record = record.map_err(|e| sig(format!("from-csv: {e}")))?;
        let fields = headers
            .iter()
            .enumerate()
            .map(|(i, h)| {
                (
                    h.clone(),
                    Value::String(record.get(i).unwrap_or("").to_owned()),
                )
            })
            .collect::<Vec<_>>();
        rows.push(Value::map(fields));
    }
    Ok(Value::list(rows))
}

/// Encode a list of records as CSV.  Columns are the first record's keys in
/// sorted order — `Map` is key-ordered, so no original column order survives
/// into one to be recovered.
pub(super) fn builtin_to_csv(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-csv")?;
    let rows = as_list(&args[0], "to-csv")?;
    let mut wtr = csv::WriterBuilder::new().from_writer(Vec::new());
    if let Some(first) = rows.iter().next() {
        let headers: Vec<String> = as_map_ref(first, "to-csv")?.keys().cloned().collect();
        wtr.write_record(&headers)
            .map_err(|e| sig(format!("to-csv: {e}")))?;
        for row in &rows {
            let map = as_map_ref(row, "to-csv")?;
            let fields: Vec<String> = headers
                .iter()
                .map(|h| map.get(h).map_or_else(String::new, Value::to_string))
                .collect();
            wtr.write_record(&fields)
                .map_err(|e| sig(format!("to-csv: {e}")))?;
        }
    }
    let bytes = wtr.into_inner().map_err(|e| sig(format!("to-csv: {e}")))?;
    write_encoded("to-csv", bytes, shell)
}

/// Write `bytes` to stdout and claim success: an encoder sets its own exit
/// status rather than leaving whatever the previous command left.
fn write_stdout_ok(name: &str, bytes: &[u8], shell: &mut Shell) -> Settled<()> {
    shell
        .write_stdout(bytes)
        .map_err(|e| sig(format!("{name}: {e}")))?;
    shell.mobile.control.last_status = 0;
    Ok(())
}

fn write_encoded(name: &str, bytes: Vec<u8>, shell: &mut Shell) -> Settled<Value> {
    write_stdout_ok(name, &bytes, shell)?;
    Ok(Value::Bytes(bytes))
}

pub(super) fn builtin_to_bytes(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-bytes")?;
    let bs = as_byte_list(&args[0], "to-bytes")?;
    write_encoded("to-bytes", bs, shell)
}

pub(super) fn builtin_to_string(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-string")?;
    write_encoded("to-string", args[0].to_string().into_bytes(), shell)
}

pub(super) fn builtin_to_line(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-line")?;
    let mut s = args[0].to_string();
    s.push('\n');
    write_stdout_ok("to-line", s.as_bytes(), shell)?;
    Ok(Value::Unit)
}

pub(super) fn builtin_to_lines(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-lines")?;
    let items = as_list(&args[0], "to-lines")?;
    let joined = items
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    write_encoded("to-lines", joined.into_bytes(), shell)
}

/// Encode `v` as JSON, refusing whatever has no faithful JSON form rather
/// than erasing it; Bytes become the integer array `to-bytes` accepts back.
///
/// [`super::util::value_to_json_lossy_bytes`] is the total counterpart, where
/// legibility outranks fidelity.
///
/// # Errors
/// If `v` or anything nested within it is a non-finite `Float` or a
/// computation value (`Lambda` / `Block` / `Handle`).
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

pub(super) fn builtin_to_json(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-json")?;
    let text = serde_json::to_string(&value_to_json(&args[0])?)
        .map_err(|e| sig(format!("to-json: {e}")))?;
    write_encoded("to-json", text.into_bytes(), shell)
}
