//! Field readers the card decoders share: each lifts one field out of a ral
//! [`ral_core::Value`] map, answering `None` on absence or wrong shape where
//! `ral_core::as_map` would raise, so a malformed record costs a field rather
//! than the decode.

use ral_core::Value as RalValue;

pub(super) fn map_of(v: &RalValue) -> Option<&ral_core::types::Map> {
    match v {
        RalValue::Map(m) => Some(m),
        _ => None,
    }
}

pub(super) fn str_field(m: &ral_core::types::Map, field: &str) -> Option<String> {
    match m.get(field) {
        Some(RalValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// A magnitude — a `Measure` bound, a hunk's start line — clamped into `u32`.
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

/// An exit status, unclamped — a code to report, not a magnitude to scale.
pub(super) fn int_field(m: &ral_core::types::Map, field: &str) -> Option<i64> {
    match m.get(field) {
        Some(RalValue::Int(n)) => Some(*n),
        _ => None,
    }
}
