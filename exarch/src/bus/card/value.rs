//! The narrow reading layer every card decoder shares.
//!
//! Each function here is a total accessor: it lifts one field out of a ral
//! [`ral_core::Value`] map, answering `None` rather than raising when the
//! field is absent or the wrong shape. Decoders build on these instead of
//! matching on the value themselves, so a malformed record degrades to a
//! missing field, never a panic.

use ral_core::Value as RalValue;

/// `&Value` → `&Map` when it is one.
pub(super) fn map_of(v: &RalValue) -> Option<&ral_core::types::Map> {
    match v {
        RalValue::Map(m) => Some(m),
        _ => None,
    }
}

/// A string-typed field of a record.
pub(super) fn str_field(m: &ral_core::types::Map, field: &str) -> Option<String> {
    match m.get(field) {
        Some(RalValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// An optional bytes-typed field of a record.
pub(super) fn bytes_field(m: &ral_core::types::Map, field: &str) -> Option<Vec<u8>> {
    match m.get(field) {
        Some(RalValue::Bytes(b)) => Some(b.clone()),
        _ => None,
    }
}

/// An integer-typed field clamped into `u32` (negatives floor to 0).
pub(super) fn count_field(m: &ral_core::types::Map, field: &str) -> Option<u32> {
    match m.get(field) {
        Some(RalValue::Int(n)) => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "value pre-clamped to [0, u32::MAX]"
            )]
            let clamped = (*n).clamp(0, i64::from(u32::MAX)) as u32;
            Some(clamped)
        }
        _ => None,
    }
}

/// An integer-typed field, unclamped — an exec status carries the full signed
/// range a process exit can name (negatives for signal-coded exits).
pub(super) fn int_field(m: &ral_core::types::Map, field: &str) -> Option<i64> {
    match m.get(field) {
        Some(RalValue::Int(n)) => Some(*n),
        _ => None,
    }
}

/// A list-of-strings field; non-string elements render as their display so a
/// partially-formed field (an `argv`, a row list) stays faithful, and a
/// missing or non-list field is empty.
pub(super) fn strings_field(m: &ral_core::types::Map, field: &str) -> Vec<String> {
    match m.get(field) {
        Some(RalValue::List(items)) => items
            .iter()
            .map(|v| match v {
                RalValue::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    }
}
