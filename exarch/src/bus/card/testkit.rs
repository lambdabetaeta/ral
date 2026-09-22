//! Test-only builders mirroring the values the kit and core put on the
//! `surface` sink; shared by every surface decoder test.

use ral_core::serial::FOValue;

pub(crate) fn s(text: &str) -> FOValue {
    FOValue::String { value: text.into() }
}
pub(crate) fn int(value: i64) -> FOValue {
    FOValue::Int { value }
}
pub(crate) fn list(items: Vec<FOValue>) -> FOValue {
    FOValue::List { items }
}
pub(crate) fn variant(label: &str, payload: FOValue) -> FOValue {
    FOValue::Variant {
        label: label.into(),
        payload: Some(Box::new(payload)),
    }
}
pub(crate) fn card_value(marks: Vec<FOValue>) -> FOValue {
    variant("card", list(marks))
}

/// A bare map — the shape an observation rides the sink as, and so the foil a
/// variant-labelled decoder must reject.
pub(crate) fn map_value(fields: Vec<(&str, FOValue)>) -> FOValue {
    FOValue::Map {
        entries: fields.into_iter().map(|(k, v)| (k.into(), v)).collect(),
    }
}
