//! Test-only builders mirroring the values the kit and core put on the
//! `surface` sink; shared by the decoder tests in `decode` and `done`.

use ral_core::Value as RalValue;

pub(super) fn s(text: &str) -> RalValue {
    RalValue::String(text.into())
}
pub(super) fn list(items: Vec<RalValue>) -> RalValue {
    RalValue::list(items)
}
pub(super) fn card_value(marks: Vec<RalValue>) -> RalValue {
    RalValue::Variant {
        label: "card".into(),
        payload: Some(Box::new(RalValue::list(marks))),
    }
}

/// A bare map — the shape an observation rides the sink as, and so the foil a
/// variant-labelled decoder must reject.
pub(super) fn map_value(fields: Vec<(&str, RalValue)>) -> RalValue {
    RalValue::map(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
