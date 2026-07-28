//! Test-only builders mirroring the values the kit and core put on the
//! `surface` sink; shared by the decoder tests in `decode`, `io`, and `done`.

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
pub(super) fn io_value(fields: Vec<(&str, RalValue)>) -> RalValue {
    RalValue::map(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
