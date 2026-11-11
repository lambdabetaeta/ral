//! Typed JSON boundaries: syntax, structural representation, then validation.

mod de;
mod raw;
mod syntax;

pub use de::decode_structural;
pub use raw::{RawDatum, RawFOValue, drop_value};
pub use syntax::{JsonDocument, JsonId, JsonRef};

use super::datum::Datum;
use crate::fact::{DecodeFailure, JsonEncodeFailure, JsonInputFailure};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonFormat {
    Compact,
    Pretty,
}

pub trait JsonDecode: Sized {
    /// # Errors
    /// Returns a structural or semantic failure with its logical path.
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure>;
}

pub trait JsonEncode {
    /// # Errors
    /// Returns a typed refusal for a value the JSON schema cannot represent.
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure>;
}

/// # Errors
/// Returns JSON syntax, structural or semantic validation failures.
pub fn from_slice<T: JsonDecode>(bytes: &[u8]) -> Result<T, JsonInputFailure> {
    let document = JsonDocument::parse(bytes).map_err(JsonInputFailure::Syntax)?;
    T::decode_json(document.root()).map_err(JsonInputFailure::Decode)
}

/// # Errors
/// Returns the schema's typed JSON encoding refusal.
pub fn to_vec<T: JsonEncode + ?Sized>(value: &T) -> Result<Vec<u8>, JsonEncodeFailure> {
    value.encode_json(JsonFormat::Compact)
}

/// # Errors
/// Returns the schema's typed JSON encoding refusal.
pub fn to_vec_pretty<T: JsonEncode + ?Sized>(value: &T) -> Result<Vec<u8>, JsonEncodeFailure> {
    value.encode_json(JsonFormat::Pretty)
}

/// # Errors
/// Returns syntax, raw leaf or canonical `Datum` validation failures.
pub fn from_datum_slice<T: Datum>(bytes: &[u8]) -> Result<T, JsonInputFailure> {
    let document = JsonDocument::parse(bytes).map_err(JsonInputFailure::Syntax)?;
    let raw: RawDatum<T> = decode_structural(document.root()).map_err(JsonInputFailure::Decode)?;
    raw.validate().map_err(JsonInputFailure::Decode)
}

pub fn to_datum_vec<T: Datum + Clone>(value: &T) -> Vec<u8> {
    let raw = RawDatum::from_datum(value.clone());
    let bytes = serialize_structural(&raw, JsonFormat::Compact);
    drop(raw);
    bytes
}

/// For audited raw serializers whose only fallible leaves were validated
/// before serialization. This is not a blanket `JsonEncode` implementation.
///
/// # Panics
/// Only if the audited structural representation violates its total-serialization contract.
pub fn serialize_structural<T: Serialize + ?Sized>(value: &T, format: JsonFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    match format {
        JsonFormat::Compact => {
            let mut serializer = serde_json::Serializer::new(&mut bytes);
            value
                .serialize(serde_stacker::Serializer::new(&mut serializer))
                .expect("audited structural JSON serialization is total");
        }
        JsonFormat::Pretty => {
            let mut serializer = serde_json::Serializer::pretty(&mut bytes);
            value
                .serialize(serde_stacker::Serializer::new(&mut serializer))
                .expect("audited structural JSON serialization is total");
        }
    }
    bytes
}

/// Stack growth for recursive raw/domain conversion, independent of compiler
/// stacks and reusable by owner adapters in other crates.
pub fn on_json_stack<T>(f: impl FnOnce() -> T) -> T {
    stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, f)
}

#[cfg(test)]
mod tests;
