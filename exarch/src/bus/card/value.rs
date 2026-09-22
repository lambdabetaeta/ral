//! Field readers the card decoders share: each lifts one field out of a
//! surfaced record, answering `None` on absence or wrong shape, so a malformed
//! record costs a field rather than the decode.

use ral_core::serial::FOValue;

/// `v` when it is a record, so a field lookup can follow.
pub(super) fn record(v: &FOValue) -> Option<&FOValue> {
    matches!(v, FOValue::Map { .. }).then_some(v)
}

pub(super) fn str_field(m: &FOValue, field: &str) -> Option<String> {
    m.field(field)?.as_str().map(str::to_owned)
}

/// A magnitude — a `Measure` bound, a hunk's start line — clamped into `u32`.
pub(super) fn count_field(m: &FOValue, field: &str) -> Option<u32> {
    let n = m.field(field)?.as_int()?;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "value pre-clamped to [0, u32::MAX]"
    )]
    let clamped = n.clamp(0, i64::from(u32::MAX)) as u32;
    Some(clamped)
}

/// An exit status, unclamped — a code to report, not a magnitude to scale.
pub(super) fn int_field(m: &FOValue, field: &str) -> Option<i64> {
    m.field(field)?.as_int()
}

/// A list field's items; empty when absent or not a list.
pub(super) fn items<'a>(m: &'a FOValue, field: &str) -> &'a [FOValue] {
    m.field(field)
        .and_then(FOValue::as_list)
        .unwrap_or_default()
}

/// `v` in ral's own syntax: what an unreadable mark degrades to.
pub(super) fn shown(v: &FOValue) -> String {
    ral_core::Value::from(v.clone()).to_string()
}
