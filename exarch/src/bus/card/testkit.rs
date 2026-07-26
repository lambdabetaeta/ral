//! Test-only scaffolding: the ral values a kit would hand the `surface`
//! builtin, built the way core builds them. Shared by the card decoders'
//! tests so each doesn't mint its own copy.

use ral_core::Value as RalValue;

pub(super) fn s(text: &str) -> RalValue {
    RalValue::String(text.into())
}
pub(super) fn list(items: Vec<RalValue>) -> RalValue {
    RalValue::list(items)
}
/// Build a `` `card [marks…] `` runtime value the way the kit does.
pub(super) fn card_value(marks: Vec<RalValue>) -> RalValue {
    RalValue::Variant {
        label: "card".into(),
        payload: Some(Box::new(RalValue::list(marks))),
    }
}
/// Build a `Value::Map` of `(field, value)` pairs the way core does.
pub(super) fn io_value(fields: Vec<(&str, RalValue)>) -> RalValue {
    RalValue::map(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
