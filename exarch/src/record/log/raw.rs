//! The envelope borrows commit syntax; sealing never reconstructs input bytes.

use super::{DecodeFailure, JsonEncodeFailure, NonEmpty, Range, Record, Sum, Unsealed};
use crate::record::json::{duplicate, missing, shape};
use ral_core::data::json::{JsonDocument, JsonFormat, JsonRef, decode_structural};
use ral_core::fact::{DecodeCause, JsonShape, UnknownField};

#[derive(Debug)]
pub(super) struct SealedLine<'a> {
    document: JsonDocument<'a>,
    pub(super) sum: Sum,
    commit: Range<usize>,
}

impl SealedLine<'_> {
    pub(super) fn body(&self) -> JsonRef<'_, '_> {
        self.document
            .root()
            .members()
            .expect("checked envelope")
            .map(|(_, value)| value)
            .find(|value| value.byte_range() == self.commit)
            .expect("checked commit range")
    }
}

/// Syntax and the complete envelope precede seal spelling and body validation.
pub(super) fn unseal(line: &[u8]) -> Result<SealedLine<'_>, Unsealed> {
    let document = JsonDocument::parse(line).map_err(Unsealed::Syntax)?;
    let (sealed, commit) = envelope(document.root()).map_err(Unsealed::Shape)?;
    let actual = Sum::of(commit.source());
    if sealed != actual.to_string() {
        return Err(Unsealed::Sum { sealed, actual });
    }
    let commit = commit.byte_range();
    Ok(SealedLine {
        document,
        sum: actual,
        commit,
    })
}

fn envelope<'d, 'a>(value: JsonRef<'d, 'a>) -> Result<(String, JsonRef<'d, 'a>), DecodeFailure> {
    let members = value
        .members()
        .ok_or_else(|| shape(value, &[JsonShape::Object]))?;
    let mut sum = None;
    let mut commit = None;
    for (key, value) in members {
        match key {
            "sum" => {
                if sum.is_some() {
                    return Err(duplicate(key));
                }
                sum = Some(decode_structural::<String>(value).map_err(|e| e.field(key))?);
            }
            "commit" => {
                if commit.is_some() {
                    return Err(duplicate(key));
                }
                commit = Some(value);
            }
            _ => {
                return Err(DecodeFailure::new(DecodeCause::UnknownField(
                    UnknownField {
                        field: key.into(),
                        offered: vec!["sum".into(), "commit".into()],
                    },
                )));
            }
        }
    }
    Ok((
        sum.ok_or_else(|| missing("sum"))?,
        commit.ok_or_else(|| missing("commit"))?,
    ))
}

/// Body conversion is fallible; its one encoding is both hashed and written.
pub(super) fn seal(
    at_unix_ms: u64,
    records: &NonEmpty<Record>,
) -> Result<(Sum, Vec<u8>), JsonEncodeFailure> {
    let body = crate::record::json::RawBody::from_records(at_unix_ms, records)?;
    let bytes = body.encode(JsonFormat::Compact);
    Ok(enclose(&bytes))
}

fn enclose(commit: &[u8]) -> (Sum, Vec<u8>) {
    let sum = Sum::of(commit);
    let mut line = format!(r#"{{"sum":"{sum}","commit":"#).into_bytes();
    line.extend_from_slice(commit);
    line.push(b'}');
    (sum, line)
}

#[cfg(test)]
pub(super) fn sealed_fixture(commit: &[u8]) -> Vec<u8> {
    enclose(commit).1
}
