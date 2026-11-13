//! Byte-channel codecs, one builtin per `from-X` / `to-X` rather than a
//! name-dispatched `codec <name>`: the typechecker then sees each one's real
//! return type, and a misspelling fails at command lookup.  Decoders are
//! nullary; where their bytes come from is [`super::util::stdin_reader`]'s
//! policy.
//!
//! The checker's own byte-to-text step is not here and is not a command: it is
//! [`crate::ir::Prim::Decode`], an operation whose meaning no session can
//! redefine.

use crate::error::Error;
use crate::fact::{
    ArityFailure, CodecFailure, CodecFailureCsvCell, CodecFailureCsvKeys, CodecFailureJsonLine,
    CodecFailureJsonPointer, CodecFailureNotJson, CodecJsonNumberFailure, CountRule,
    CsvDeserializeCause, CsvPosition, CsvReadFailure, CsvReadFailureDeserialize,
    CsvReadFailureUnequalLengths, CsvReadFailureUtf8, Expected, InvalidCause, InvalidFailure,
    IoOperation, IoSubject, JsonCategory, JsonPointerCause, JsonShape, JsonSyntaxFailure,
    JsonValueFailure, JsonValueNumberCause, Reason, ShapeFailure, StandardStream, Utf8Failure,
    Utf8Subject, ValueShape,
};
use crate::ir::{VClosRef, ValClos, View};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Mooring, Settled, Shell, decode_utf8_strict, pointer_token};

use crate::data::{FOValue, Finite};
use crate::ty::Site;
use std::sync::Arc;

use super::util::{as_byte_list, lossy_line_list};

fn read_stdin_bytes(name: &str, shell: &Shell) -> Settled<Vec<u8>> {
    use std::io::Read;

    let mut bytes = Vec::new();
    super::util::stdin_reader(name, shell)?
        .read_to_end(&mut bytes)
        .map_err(|e| {
            Error::io(
                IoOperation::Read,
                IoSubject::Stream(StandardStream::Stdin),
                &e,
            )
            .with_context(name)
        })?;
    Ok(bytes)
}

/// Channel bytes for a `from-X` decoder.  The typechecker rejects a written
/// argument outright, so this guard is for spread calls, whose arity it
/// cannot see ([`crate::ir::Argv::positional`] gives up on them).
fn no_arguments(args: &[ValClos], name: &str) -> Settled<()> {
    if args.is_empty() {
        return Ok(());
    }
    Err(invalid(
        name,
        InvalidCause::Arity(ArityFailure {
            expected: CountRule::Exactly(0usize.into()),
            got: args.len().into(),
        }),
    )
    .into())
}

fn input_bytes(args: &[ValClos], name: &str, shell: &Shell) -> Settled<Vec<u8>> {
    no_arguments(args, name)?;
    read_stdin_bytes(name, shell)
}

/// Channel text for a `from-X` decoder that needs real UTF-8: the same
/// refusal, and the same way out of it, for every one of them.
fn input_text(args: &[ValClos], name: &str, shell: &Shell) -> Settled<String> {
    decode_utf8_strict(input_bytes(args, name, shell)?, name, Utf8Subject::Input)
}

pub(super) fn builtin_from_bytes(args: &[ValClos], shell: &Shell) -> Settled<ValClos> {
    Ok(ValClos::bytes(input_bytes(args, "from-bytes", shell)?))
}

pub(super) fn builtin_from_string(args: &[ValClos], shell: &Shell) -> Settled<ValClos> {
    Ok(ValClos::string(input_text(args, "from-string", shell)?))
}

pub(super) fn builtin_from_line(args: &[ValClos], shell: &Shell) -> Settled<ValClos> {
    let mut text = input_text(args, "from-line", shell)?;
    text.truncate(crate::io::without_terminator(text.as_bytes()).len());
    Ok(ValClos::string(text))
}

pub(super) fn builtin_from_lines(args: &[ValClos], shell: &Shell) -> Settled<ValClos> {
    no_arguments(args, "from-lines")?;
    lossy_line_list(super::util::stdin_lines("from-lines", shell)?)
}

fn json_to_fo(j: serde_json::Value) -> Result<FOValue, CodecJsonNumberFailure> {
    Ok(match j {
        serde_json::Value::Null => FOValue::Unit,
        serde_json::Value::Bool(value) => FOValue::Bool { value },
        serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(value), _) => FOValue::Int { value },
            (None, Some(f)) if n.is_f64() => match Finite::new(f) {
                Some(value) => FOValue::Float { value },
                None => {
                    return Err(CodecJsonNumberFailure {
                        text: n.to_string(),
                        cause: JsonValueNumberCause::NonFinite,
                    });
                }
            },
            _ => {
                return Err(CodecJsonNumberFailure {
                    text: n.to_string(),
                    cause: if n.is_f64() {
                        JsonValueNumberCause::NonFinite
                    } else {
                        JsonValueNumberCause::OutOfI64
                    },
                });
            }
        },
        serde_json::Value::String(value) => FOValue::String { value },
        serde_json::Value::Array(arr) => FOValue::List {
            items: arr.into_iter().map(json_to_fo).collect::<Result<_, _>>()?,
        },
        serde_json::Value::Object(obj) => FOValue::Map {
            entries: obj
                .into_iter()
                .map(|(k, v)| Ok((k, json_to_fo(v)?)))
                .collect::<Result<_, _>>()?,
        },
    })
}

fn json_to_value(j: serde_json::Value) -> Result<ValClos, CodecJsonNumberFailure> {
    json_to_fo(j).map(ValClos::from)
}

pub(crate) fn builtin_from_json(
    args: &[ValClos],
    site: &Arc<Site>,
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    let text = input_text(args, "from-json", shell)?;
    let json = serde_json::from_str(&text)
        .map_err(|e| codec("from-json", CodecFailure::JsonSyntax(json_syntax(&e))))?;
    let value = json_to_value(json).map_err(|e| codec("from-json", CodecFailure::JsonNumber(e)))?;
    site.admitted("from-json", value, shell)
}

/// The member or element `token` names under `json`, an object member by exact
/// key and an array element by canonical decimal (RFC 6901).
fn step<'a>(
    json: &'a serde_json::Value,
    token: &str,
    at: &str,
) -> Result<&'a serde_json::Value, CodecFailureJsonPointer> {
    let cause = match json {
        serde_json::Value::Object(members) => {
            if let Some(value) = members.get(token) {
                return Ok(value);
            }
            JsonPointerCause::MissingMember(members.keys().cloned().collect())
        }
        serde_json::Value::Array(items) => {
            if let Some(value) = token
                .parse::<usize>()
                .ok()
                .filter(|index| index.to_string() == token)
                .and_then(|index| items.get(index))
            {
                return Ok(value);
            }
            JsonPointerCause::NoElement(items.len().into())
        }
        scalar => JsonPointerCause::Scalar(json_shape(scalar)),
    };
    Err(CodecFailureJsonPointer {
        at: at.into(),
        token: token.into(),
        cause,
    })
}

fn json_shape(json: &serde_json::Value) -> JsonShape {
    match json {
        serde_json::Value::Null => JsonShape::Null,
        serde_json::Value::Bool(_) => JsonShape::Bool,
        serde_json::Value::Number(_) => JsonShape::Number,
        serde_json::Value::String(_) => JsonShape::String,
        serde_json::Value::Array(_) => JsonShape::Array,
        serde_json::Value::Object(_) => JsonShape::Object,
    }
}

/// `from-json-at tokens`: decode, then read the value the RFC 6901 reference
/// tokens lead to.  The tokens are a list and not a pointer string, so a key
/// holding a `/` or a `~` needs no escaping.
pub(crate) fn builtin_from_json_at(
    args: &[ValClos],
    site: &Arc<Site>,
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    const DOOR: &str = "from-json-at";
    let tokens: Vec<String> = args[0]
        .as_list(DOOR)?
        .iter()
        .enumerate()
        .map(|(index, token)| match token.view() {
            View::String(s) => Ok(s.to_string()),
            _ => Err(Error::new(Reason::Invalid(Box::new(
                InvalidFailure::new(
                    DOOR.into(),
                    InvalidCause::Shape(ShapeFailure {
                        expected: Expected::Shape(ValueShape::String),
                        found: token.found(),
                    }),
                )
                .index(index.into())
                .index(0usize.into()),
            )))),
        })
        .collect::<Result<_, _>>()?;
    let text = decode_utf8_strict(read_stdin_bytes(DOOR, shell)?, DOOR, Utf8Subject::Input)?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| codec(DOOR, CodecFailure::JsonSyntax(json_syntax(&e))))?;
    let mut leaf = &json;
    let mut at = String::new();
    for token in &tokens {
        leaf = step(leaf, token, &at).map_err(|why| codec(DOOR, CodecFailure::JsonPointer(why)))?;
        at.push('/');
        at.push_str(&pointer_token(token));
    }
    let value =
        json_to_value(leaf.clone()).map_err(|e| codec(DOOR, CodecFailure::JsonNumber(e)))?;
    site.admitted(DOOR, value, shell)
}

/// JSON Lines: a JSON text on each line the one line rule splits, a blank line
/// holding none.  Each line parses alone, so the line an error names is the
/// input's and serde's own position is only a column.
pub(crate) fn builtin_from_jsonl(
    args: &[ValClos],
    site: &Arc<Site>,
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    no_arguments(args, "from-jsonl")?;
    let mut records = Vec::new();
    for (index, line) in super::util::stdin_lines("from-jsonl", shell)?.enumerate() {
        let line = line?;
        // JSON's whitespace, less the LF no line holds.
        if line.iter().all(|b| matches!(b, b' ' | b'\t' | b'\r')) {
            continue;
        }
        let n = index + 1;
        let json = serde_json::from_slice(&line).map_err(|error| {
            codec(
                "from-jsonl",
                CodecFailure::JsonLine(CodecFailureJsonLine {
                    line: n.into(),
                    cause: JsonValueFailure::Syntax(json_syntax(&error)),
                }),
            )
        })?;
        records.push(json_to_value(json).map_err(|error| {
            codec(
                "from-jsonl",
                CodecFailure::JsonLine(CodecFailureJsonLine {
                    line: n.into(),
                    cause: JsonValueFailure::Number(error),
                }),
            )
        })?);
    }
    site.admitted("from-jsonl", ValClos::list(records), shell)
}

fn codec(operation: &str, cause: CodecFailure) -> Error {
    invalid(operation, InvalidCause::Codec(cause))
}

fn json_syntax(error: &serde_json::Error) -> JsonSyntaxFailure {
    JsonSyntaxFailure {
        class: match error.classify() {
            serde_json::error::Category::Syntax => JsonCategory::Syntax,
            serde_json::error::Category::Eof => JsonCategory::Eof,
            serde_json::error::Category::Data | serde_json::error::Category::Io => {
                unreachable!("parsing JSON values from bytes performs neither validation nor I/O")
            }
        },
        line: error.line().into(),
        column: error.column().into(),
        detail: JsonSyntaxFailure::parser_detail(error),
    }
}

/// CSV cells stay text; the site checks each row's required shape.
pub(super) fn builtin_from_csv(
    args: &[ValClos],
    site: &Arc<Site>,
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    let bytes = input_bytes(args, "from-csv", shell)?;
    let mut rdr = csv::Reader::from_reader(bytes.as_slice());
    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| codec("from-csv", CodecFailure::CsvRead(csv_failure(&e))))?
        .iter()
        .map(str::to_owned)
        .collect();
    let mut seen = std::collections::HashSet::with_capacity(headers.len());
    for h in &headers {
        if !seen.insert(h) {
            return Err(codec("from-csv", CodecFailure::CsvDuplicateHeader(h.clone())).into());
        }
    }
    let mut rows = Vec::new();
    for record in rdr.records() {
        let record =
            record.map_err(|e| codec("from-csv", CodecFailure::CsvRead(csv_failure(&e))))?;
        // The reader refuses a ragged record, so the zip drops nothing.
        let fields = headers
            .iter()
            .cloned()
            .zip(record.iter().map(ValClos::string));
        rows.push(ValClos::map(fields));
    }
    site.admitted("from-csv", ValClos::list(rows), shell)
}

fn csv_position(position: &csv::Position) -> CsvPosition {
    CsvPosition {
        byte: position.byte().into(),
        line: position.line().into(),
        record: position.record().into(),
    }
}

fn csv_failure(error: &csv::Error) -> CsvReadFailure {
    match error.kind() {
        csv::ErrorKind::Io(error) => CsvReadFailure::Io(crate::io::failure(error)),
        csv::ErrorKind::Utf8 { pos, err } => CsvReadFailure::Utf8(CsvReadFailureUtf8 {
            position: pos.as_ref().map(csv_position),
            field: err.field().into(),
            valid_up_to: err.valid_up_to().into(),
        }),
        csv::ErrorKind::UnequalLengths {
            pos,
            expected_len,
            len,
        } => CsvReadFailure::UnequalLengths(CsvReadFailureUnequalLengths {
            position: pos.as_ref().map(csv_position),
            expected: (*expected_len).into(),
            got: (*len).into(),
        }),
        csv::ErrorKind::Seek => CsvReadFailure::Seek,
        csv::ErrorKind::Serialize(detail) => CsvReadFailure::Serialize(detail.clone()),
        csv::ErrorKind::Deserialize { pos, err } => {
            CsvReadFailure::Deserialize(CsvReadFailureDeserialize {
                position: pos.as_ref().map(csv_position),
                field: err.field().map(Into::into),
                cause: csv_deserialize(err.kind()),
            })
        }
        _ => CsvReadFailure::Unclassified(error.to_string()),
    }
}

fn csv_deserialize(cause: &csv::DeserializeErrorKind) -> CsvDeserializeCause {
    use csv::DeserializeErrorKind as C;
    match cause {
        C::Message(detail) => CsvDeserializeCause::Message(detail.clone()),
        C::Unsupported(detail) => CsvDeserializeCause::Unsupported(detail.clone()),
        C::UnexpectedEndOfRow => CsvDeserializeCause::UnexpectedEndOfRow,
        C::InvalidUtf8(error) => CsvDeserializeCause::InvalidUtf8(Utf8Failure {
            valid_up_to: error.valid_up_to().into(),
            error_len: error.error_len().map(Into::into),
        }),
        C::ParseBool(_) => CsvDeserializeCause::ParseBool,
        C::ParseInt(error) => CsvDeserializeCause::ParseInt(match error.kind() {
            std::num::IntErrorKind::Empty => crate::fact::IntegerParseCause::Empty,
            std::num::IntErrorKind::InvalidDigit => crate::fact::IntegerParseCause::InvalidDigit,
            std::num::IntErrorKind::PosOverflow => crate::fact::IntegerParseCause::PositiveOverflow,
            std::num::IntErrorKind::NegOverflow => crate::fact::IntegerParseCause::NegativeOverflow,
            std::num::IntErrorKind::Zero => crate::fact::IntegerParseCause::Zero,
            _ => crate::fact::IntegerParseCause::Unclassified,
        }),
        C::ParseFloat(_) => CsvDeserializeCause::ParseFloat,
    }
}

/// Records and maps share the key-ordered runtime carrier.
pub(super) fn builtin_to_csv(args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    let rows = args[0].as_list("to-csv")?;
    let mut wtr = csv::Writer::from_writer(Vec::new());
    if let Some(first) = rows.iter().next() {
        let header = first.as_map("to-csv")?;
        if header.is_empty() {
            return Err(codec("to-csv", CodecFailure::CsvNoColumns).into());
        }
        wtr.write_record(header.keys())
            .map_err(|e| codec("to-csv", CodecFailure::CsvRead(csv_failure(&e))))?;
        for (i, row) in rows.iter().enumerate() {
            let map = row.as_map("to-csv")?;
            let stray = header
                .keys()
                .chain(map.keys())
                .find(|k| header.contains_key(k) != map.contains_key(k));
            if let Some(key) = stray {
                let (has, lacks) = if map.contains_key(key) {
                    (i, 0)
                } else {
                    (0, i)
                };
                return Err(codec(
                    "to-csv",
                    CodecFailure::CsvKeys(CodecFailureCsvKeys {
                        has_row: has.into(),
                        lacks_row: lacks.into(),
                        key: key.into(),
                    }),
                )
                .into());
            }
            let cells = map
                .iter()
                .map(|(key, value)| csv_cell(value, i, key))
                .collect::<Settled<Vec<_>>>()?;
            wtr.write_record(cells)
                .map_err(|e| codec("to-csv", CodecFailure::CsvRead(csv_failure(&e))))?;
        }
    }
    let bytes = wtr.into_inner().map_err(|e| {
        codec(
            "to-csv",
            CodecFailure::CsvFinalize(crate::io::failure(e.error())),
        )
    })?;
    write_encoded(&bytes, shell)
}

fn csv_cell(value: VClosRef<'_>, row: usize, key: &str) -> Settled<String> {
    if let Some(text) = value.scalar_text() {
        return Ok(text);
    }
    Err(codec(
        "to-csv",
        CodecFailure::CsvCell(CodecFailureCsvCell {
            row: row.into(),
            key: key.into(),
            found: value.shape(),
        }),
    )
    .into())
}

fn write_encoded(bytes: &[u8], shell: &mut Shell) -> Settled<ValClos> {
    shell.write_stdout(bytes)?;
    Ok(ValClos::UNIT)
}

pub(super) fn builtin_to_bytes(args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    let bs = args[0].as_bytes("to-bytes")?;
    write_encoded(bs, shell)
}

pub(super) fn builtin_ints_to_bytes(args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    let bs = as_byte_list(&args[0], "ints-to-bytes")?;
    write_encoded(&bs, shell)
}

pub(super) fn builtin_to_string(args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    write_encoded(args[0].to_string().as_bytes(), shell)
}

pub(super) fn builtin_to_line(args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    let mut s = args[0].to_string();
    s.push('\n');
    write_encoded(s.as_bytes(), shell)
}

/// `echo`'s base-frame body: the argv rendered ([`ValClos::render_argv`]),
/// single-space intercalate, trailing newline to the byte channel.  `()` is
/// nothing to print, which the checker refuses and this backstops.
pub(super) fn builtin_echo(
    args: &[ValClos],
    _mooring: &crate::shell::runtime::Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    if let Some(index) = args.iter().position(|arg| matches!(arg.view(), View::Unit)) {
        return Err(Error::new(Reason::Invalid(Box::new(
            InvalidFailure::new(
                "echo".into(),
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::AnyOf(vec![
                        ValueShape::Bool,
                        ValueShape::Int,
                        ValueShape::Float,
                        ValueShape::String,
                        ValueShape::Bytes,
                        ValueShape::List,
                        ValueShape::Map,
                        ValueShape::Variant,
                        ValueShape::Block,
                        ValueShape::Function,
                        ValueShape::Handle,
                    ]),
                    found: crate::fact::Found::Unit,
                }),
            )
            .index(index.into()),
        )))
        .into());
    }
    let mut s = ValClos::render_argv(args).join(" ");
    s.push('\n');
    write_encoded(s.as_bytes(), shell)
}

pub(super) fn builtin_to_lines(args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    let mut text = String::new();
    for item in args[0].as_list("to-lines")?.iter() {
        text.push_str(&item.to_string());
        text.push('\n');
    }
    write_encoded(text.as_bytes(), shell)
}

/// Encode `v` as JSON, refusing whatever has no faithful JSON form rather
/// than erasing it; Bytes become the integer array `ints-to-bytes` accepts
/// back.
///
/// # Errors
/// If `v` or anything nested within it is a block, function or handle.
fn value_to_json(v: VClosRef<'_>) -> Result<serde_json::Value, CodecFailureNotJson> {
    FOValue::try_from(v)
        .map(|fo| fo.to_json(|b| b.iter().map(|&byte| serde_json::json!(byte)).collect()))
        .map_err(|e| CodecFailureNotJson {
            leaf: e.leaf.into(),
            nested: e.nested,
        })
}

pub(super) fn builtin_to_json(args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    let json = value_to_json(VClosRef::from(&args[0]))
        .map_err(|e| codec("to-json", CodecFailure::NotJson(e)))?;
    write_encoded(json.to_string().as_bytes(), shell)
}

/// JSON Lines: each element as `to-json` writes it, compact and so free of
/// LF, then a `\n`.  Every element encodes before any byte is written.
pub(super) fn builtin_to_jsonl(args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    let mut text = String::new();
    for (i, item) in args[0].as_list("to-jsonl")?.iter().enumerate() {
        let json = value_to_json(item).map_err(|e| {
            Error::new(Reason::Invalid(Box::new(
                InvalidFailure::new(
                    "to-jsonl".into(),
                    InvalidCause::Codec(CodecFailure::NotJson(e)),
                )
                .index(i.into()),
            )))
        })?;
        text.push_str(&json.to_string());
        text.push('\n');
    }
    write_encoded(text.as_bytes(), shell)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::Datum;
    use crate::fact::{DataStep, Found};
    use crate::ir::{Env, block_over};
    use crate::shell::runtime::Break;

    fn input(bytes: &[u8]) -> Shell {
        let mut shell = crate::test_helper::core_shell();
        shell.io.stdin =
            crate::io::Source::Reader(crate::io::SourceReader::from_bytes(bytes.to_vec()).unwrap());
        shell
    }

    fn site(ty: &crate::ty::Ty) -> Arc<Site> {
        Arc::new(Site::snapshot(
            &crate::typecheck::Unifier::new(),
            ty,
            crate::ty::UnitId::fresh(),
        ))
    }

    fn failure(result: Settled<ValClos>) -> InvalidFailure {
        let Err(Break::Error(error)) = result else {
            panic!("expected invalid input")
        };
        assert_eq!(error.hints(), []);
        let Reason::Invalid(failure) = error.reason else {
            panic!("invalid input")
        };
        let encoded = failure.clone().encode();
        let decoded = InvalidFailure::decode(&encoded).unwrap();
        assert_eq!(decoded, *failure);
        assert_eq!(decoded.describe(), failure.describe());
        assert_eq!(decoded.advice(), failure.advice());
        *failure
    }

    #[test]
    fn json_numbers_syntax_and_pointer_failures_keep_their_causes() {
        let n = json_to_value(serde_json::json!(u64::MAX)).unwrap_err();
        assert_eq!(
            n,
            CodecJsonNumberFailure {
                text: u64::MAX.to_string(),
                cause: JsonValueNumberCause::OutOfI64
            }
        );
        let mut shell = input(b"{");
        assert!(matches!(
            failure(builtin_from_json(
                &[],
                &site(&crate::ty::Ty::unit()),
                &Mooring::adrift(),
                &mut shell
            ))
            .cause,
            InvalidCause::Codec(CodecFailure::JsonSyntax(JsonSyntaxFailure {
                class: JsonCategory::Eof,
                ..
            }))
        ));
        let doc = serde_json::json!({"a": [1], "b": true});
        let missing = step(&doc, "z", "").unwrap_err();
        assert_eq!(missing.at, "");
        assert_eq!(missing.token, "z");
        assert_eq!(
            missing.cause,
            JsonPointerCause::MissingMember(vec!["a".into(), "b".into()])
        );
        for token in ["1", "01", "-1", "-"] {
            assert_eq!(
                step(&doc["a"], token, "/a").unwrap_err().cause,
                JsonPointerCause::NoElement(1usize.into())
            );
        }
        assert_eq!(
            step(&doc["b"], "a", "/b").unwrap_err().cause,
            JsonPointerCause::Scalar(JsonShape::Bool)
        );
        let mut shell = input(b"{}");
        let failure = failure(builtin_from_json_at(
            &[ValClos::list(vec![ValClos::int(2)])],
            &site(&crate::ty::Ty::unit()),
            &Mooring::adrift(),
            &mut shell,
        ));
        assert_eq!(
            failure.path,
            vec![
                DataStep::Index(0usize.into()),
                DataStep::Index(0usize.into())
            ]
        );
        assert!(matches!(
            failure.cause,
            InvalidCause::Shape(ShapeFailure {
                found: Found::Int(2),
                ..
            })
        ));
    }

    #[test]
    fn json_lines_keep_real_input_lines_and_encoding_element_positions() {
        for (bytes, numeric) in [
            (&b"\n \t\n{\n"[..], false),
            (&b"\n\n18446744073709551615\n"[..], true),
        ] {
            let mut shell = input(bytes);
            let failure = failure(builtin_from_jsonl(
                &[],
                &site(&crate::ty::Ty::list(crate::ty::Ty::unit())),
                &Mooring::adrift(),
                &mut shell,
            ));
            assert!(failure.describe().starts_with("from-jsonl: input line 3: "));
            assert_eq!(failure.advice(), [] as [String; 0]);
            let InvalidCause::Codec(CodecFailure::JsonLine(line)) = failure.cause else {
                panic!("input line")
            };
            assert_eq!(line.line, 3usize.into());
            assert_eq!(matches!(line.cause, JsonValueFailure::Number(_)), numeric);
        }
        let block = block_over(&Env::new());
        for (value, nested) in [
            (block.clone(), false),
            (ValClos::list(vec![block.clone()]), true),
        ] {
            let cause = value_to_json(VClosRef::from(&value)).unwrap_err();
            assert_eq!(
                cause,
                CodecFailureNotJson {
                    leaf: crate::fact::OpaqueKind::Block,
                    nested
                }
            );
        }
        let mut shell = input(b"");
        let failure = failure(builtin_to_jsonl(
            &[ValClos::list(vec![ValClos::int(1), block])],
            &mut shell,
        ));
        assert_eq!(failure.path, vec![DataStep::Index(1usize.into())]);
        assert!(matches!(
            failure.cause,
            InvalidCause::Codec(CodecFailure::NotJson(CodecFailureNotJson {
                nested: false,
                ..
            }))
        ));
    }

    #[test]
    fn csv_library_projection_keeps_positions_and_foreign_subcauses() {
        let mut reader = csv::Reader::from_reader(&b"a,b\n1\n"[..]);
        reader.headers().unwrap();
        let error = reader.records().next().unwrap().unwrap_err();
        let CsvReadFailure::UnequalLengths(width) = csv_failure(&error) else {
            panic!("unequal widths")
        };
        assert_eq!(width.expected, 2usize.into());
        assert_eq!(width.got, 1usize.into());
        assert!(width.position.is_some());
        let mut reader = csv::Reader::from_reader(&b"a\n\xff\n"[..]);
        reader.headers().unwrap();
        let error = reader.records().next().unwrap().unwrap_err();
        assert!(
            matches!(csv_failure(&error), CsvReadFailure::Utf8(CsvReadFailureUtf8 { field, valid_up_to, position: Some(_) }) if field == 0usize.into() && valid_up_to == 0usize.into())
        );
    }

    #[test]
    fn csv_local_validation_retains_headers_keys_and_cell_heads() {
        let mut shell = input(b"a,a\n1,2\n");
        assert!(
            matches!(failure(builtin_from_csv(&[], &site(&crate::ty::Ty::unit()), &Mooring::adrift(), &mut shell)).cause,
            InvalidCause::Codec(CodecFailure::CsvDuplicateHeader(header)) if header == "a")
        );
        assert_eq!(
            failure(builtin_to_csv(
                &[ValClos::list(vec![ValClos::map(vec![])])],
                &mut shell
            ))
            .cause,
            InvalidCause::Codec(CodecFailure::CsvNoColumns)
        );
        let rows = ValClos::list(vec![
            ValClos::map(vec![("a".into(), ValClos::int(1))]),
            ValClos::map(vec![]),
        ]);
        assert_eq!(
            failure(builtin_to_csv(&[rows], &mut shell)).cause,
            InvalidCause::Codec(CodecFailure::CsvKeys(CodecFailureCsvKeys {
                has_row: 0usize.into(),
                lacks_row: 1usize.into(),
                key: "a".into(),
            }))
        );
        assert_eq!(
            failure(csv_cell(VClosRef::from(&ValClos::UNIT), 4, "a").map(ValClos::string)).cause,
            InvalidCause::Codec(CodecFailure::CsvCell(CodecFailureCsvCell {
                row: 4usize.into(),
                key: "a".into(),
                found: ValueShape::Unit,
            }))
        );
    }

    #[test]
    fn decoder_arity_and_echo_refusals_keep_counts_and_indices() {
        let failure =
            failure(no_arguments(&[ValClos::UNIT], "from-string").map(|()| ValClos::UNIT));
        assert_eq!(
            failure.cause,
            InvalidCause::Arity(ArityFailure {
                expected: CountRule::Exactly(0usize.into()),
                got: 1usize.into()
            })
        );
        assert_eq!(
            failure.advice(),
            [
                "to decode a value in hand, pipe it through the matching encoder: `to-string $text | from-string`"
            ]
        );
        let mut shell = input(b"");
        let failure = self::failure(builtin_echo(
            &[ValClos::string("x"), ValClos::UNIT],
            &Mooring::adrift(),
            &mut shell,
        ));
        assert_eq!(failure.path, vec![DataStep::Index(1usize.into())]);
        assert_eq!(
            failure.advice(),
            [crate::ty::RefusedArg::Unit.remedy("echo")]
        );
        assert!(matches!(
            failure.cause,
            InvalidCause::Shape(ShapeFailure {
                found: Found::Unit,
                ..
            })
        ));
    }
}
