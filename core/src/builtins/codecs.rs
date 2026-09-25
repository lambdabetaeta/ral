//! Byte-channel codecs, one builtin per `from-X` / `to-X` rather than a
//! name-dispatched `codec <name>`: the typechecker then sees each one's real
//! return type, and a misspelling fails at command lookup.  Decoders are
//! nullary; where their bytes come from is [`super::util::stdin_reader`]'s
//! policy.
//!
//! The checker's own byte-to-text step is not here and is not a command: it is
//! [`crate::ir::CompKind::Decode`], syntax whose meaning no session can
//! redefine.

use crate::types::{Settled, Shell, Value, as_list, as_map_ref, sig, sig_hint};

use super::util::{as_byte_list, as_bytes, decode_utf8_strict, lossy_line_list};

fn read_stdin_bytes(name: &str, shell: &Shell) -> Settled<Vec<u8>> {
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
fn no_arguments(args: &[Value], name: &str) -> Settled<()> {
    if args.is_empty() {
        return Ok(());
    }
    Err(sig_hint(
        format!("{name}: takes no arguments — it reads the byte channel"),
        "to decode a value in hand, pipe it through the matching encoder: `to-string $x | from-json`",
    ))
}

fn input_bytes(args: &[Value], name: &str, shell: &Shell) -> Settled<Vec<u8>> {
    no_arguments(args, name)?;
    read_stdin_bytes(name, shell)
}

/// Channel text for a `from-X` decoder that needs real UTF-8: the same
/// refusal, and the same way out of it, for every one of them.
fn input_text(args: &[Value], name: &str, shell: &Shell) -> Settled<String> {
    decode_utf8_strict(
        input_bytes(args, name, shell)?,
        &format!("{name}: input is not valid UTF-8"),
        "use from-bytes to keep raw bytes",
    )
}

pub(super) fn builtin_from_bytes(args: &[Value], shell: &Shell) -> Settled<Value> {
    Ok(Value::bytes(input_bytes(args, "from-bytes", shell)?))
}

pub(super) fn builtin_from_string(args: &[Value], shell: &Shell) -> Settled<Value> {
    Ok(Value::string(input_text(args, "from-string", shell)?))
}

pub(super) fn builtin_from_line(args: &[Value], shell: &Shell) -> Settled<Value> {
    let mut text = input_text(args, "from-line", shell)?;
    text.truncate(text.len() - crate::io::terminator_len(text.as_bytes()));
    Ok(Value::string(text))
}

pub(super) fn builtin_from_lines(args: &[Value], shell: &Shell) -> Settled<Value> {
    no_arguments(args, "from-lines")?;
    lossy_line_list(super::util::stdin_lines("from-lines", shell)?)
}

/// The one JSON number ral refuses: a `u64` above `i64::MAX`, whose low bits
/// `f64` would round away.
struct OutOfRange(serde_json::Number);

impl std::fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "integer {} is outside the supported range", self.0)
    }
}

fn json_to_value(j: serde_json::Value) -> Result<Value, OutOfRange> {
    Ok(match j {
        serde_json::Value::Null => Value::Unit,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if n.is_f64() {
                // `is_f64` just held.
                Value::Float(n.as_f64().unwrap())
            } else {
                return Err(OutOfRange(n));
            }
        }
        serde_json::Value::String(s) => Value::string(s),
        serde_json::Value::Array(arr) => Value::list(
            arr.into_iter()
                .map(json_to_value)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        serde_json::Value::Object(obj) => Value::Map(
            obj.into_iter()
                .map(|(k, v)| Ok((k, json_to_value(v)?)))
                .collect::<Result<_, _>>()?,
        ),
    })
}

pub(super) fn builtin_from_json(args: &[Value], shell: &Shell) -> Settled<Value> {
    let text = input_text(args, "from-json", shell)?;
    let json = serde_json::from_str(&text).map_err(|e| sig(format!("from-json: {e}")))?;
    json_to_value(json).map_err(|e| sig(format!("from-json: {e}")))
}

/// JSON Lines: a JSON text on each line the one line rule splits, a blank line
/// holding none.  Each line parses alone, so the line an error names is the
/// input's and serde's own position is only a column.
pub(super) fn builtin_from_jsonl(args: &[Value], shell: &Shell) -> Settled<Value> {
    no_arguments(args, "from-jsonl")?;
    let mut records = Vec::new();
    for (index, line) in super::util::stdin_lines("from-jsonl", shell)?.enumerate() {
        let line = line?;
        // JSON's whitespace, less the LF no line holds.
        if line.iter().all(|b| matches!(b, b' ' | b'\t' | b'\r')) {
            continue;
        }
        let n = index + 1;
        let json = serde_json::from_slice(&line).map_err(|e| {
            sig(format!(
                "from-jsonl: line {n}, column {}: {}",
                e.column(),
                unpositioned(&e)
            ))
        })?;
        records.push(json_to_value(json).map_err(|e| sig(format!("from-jsonl: line {n}: {e}")))?);
    }
    Ok(Value::list(records))
}

/// serde's message less the position it appends, for a caller stating the
/// position in its own terms.
fn unpositioned(e: &serde_json::Error) -> String {
    let mut message = e.to_string();
    let at = format!(" at line {} column {}", e.line(), e.column());
    message.truncate(message.strip_suffix(&at).map_or(message.len(), str::len));
    message
}

/// Decode CSV into a list of records keyed by the header row; fields stay
/// `String`, since CSV is untyped.  A duplicate header is refused rather than
/// resolved last-write-wins — a record cannot hold two columns of one name.
pub(super) fn builtin_from_csv(args: &[Value], shell: &Shell) -> Settled<Value> {
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
            .map(|(i, h)| (h.clone(), Value::string(record.get(i).unwrap_or(""))))
            .collect::<Vec<_>>();
        rows.push(Value::map(fields));
    }
    Ok(Value::list(rows))
}

/// Encode a list of records as CSV.  Columns are the first record's keys in
/// sorted order — `Map` is key-ordered, so no original column order survives
/// into one to be recovered.
pub(super) fn builtin_to_csv(args: &[Value], shell: &mut Shell) -> Settled<Value> {
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
    write_encoded(&bytes, shell)
}

fn write_encoded(bytes: &[u8], shell: &mut Shell) -> Settled<Value> {
    shell.write_stdout(bytes)?;
    Ok(Value::Unit)
}

pub(super) fn builtin_to_bytes(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bs = as_bytes(&args[0], "to-bytes")?;
    write_encoded(bs, shell)
}

pub(super) fn builtin_ints_to_bytes(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bs = as_byte_list(&args[0], "ints-to-bytes")?;
    write_encoded(&bs, shell)
}

pub(super) fn builtin_to_string(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    write_encoded(args[0].to_string().as_bytes(), shell)
}

pub(super) fn builtin_to_line(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let mut s = args[0].to_string();
    s.push('\n');
    write_encoded(s.as_bytes(), shell)
}

/// `echo`'s base-frame body: the argv rendered ([`Value::render_argv`]),
/// single-space intercalate, trailing newline to the byte channel.
pub(super) fn builtin_echo(
    args: &[Value],
    _mooring: &crate::types::Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let mut s = Value::render_argv(args).join(" ");
    s.push('\n');
    write_encoded(s.as_bytes(), shell)
}

pub(super) fn builtin_to_lines(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let mut text = String::new();
    for item in &as_list(&args[0], "to-lines")? {
        text.push_str(&item.to_string());
        text.push('\n');
    }
    write_encoded(text.as_bytes(), shell)
}

/// Encode `v` as JSON, refusing whatever has no faithful JSON form rather
/// than erasing it; Bytes become the integer array `ints-to-bytes` accepts
/// back.
///
/// [`super::util::value_to_json_lossy_bytes`] is the total counterpart, where
/// legibility outranks fidelity.
///
/// # Errors
/// If `v` or anything nested within it is a non-finite `Float` or a
/// computation value (`Lambda` / `Block` / `Handle`).
pub(crate) fn value_to_json(v: &Value) -> Settled<serde_json::Value> {
    Ok(match v {
        Value::Unit => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => serde_json::json!(*n),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| sig(format!("to-json: {f} has no JSON representation")))?,
        Value::String(s) => serde_json::Value::String(s.to_string()),
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
        Value::Thunk(_) | Value::Native { .. } | Value::Handle(_) => {
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
    let text = serde_json::to_string(&value_to_json(&args[0])?)
        .map_err(|e| sig(format!("to-json: {e}")))?;
    write_encoded(&text.into_bytes(), shell)
}
