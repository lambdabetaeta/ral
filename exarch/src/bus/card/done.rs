//! How a detached worker's completion reads.
//!
//! Core flushes a single `` `done `` value at the end of a background block's
//! deferred buffer; [`value_to_done`] decodes it, [`done_card`] renders it.

use ral_core::Value as RalValue;
use std::fmt::Write;

use super::value::{int_field, map_of, str_field};
use super::{Card, Mark, Role, Span};

/// How a detached `spawn` worker settled: the decoded form of the `` `done ``
/// event core appends to the worker's deferred buffer at completion.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DoneOutcome {
    Ok,
    /// The same `{status, message}` a caught error hands `try` and `poll`.
    Err {
        message: String,
        status: i64,
    },
    Panic {
        message: String,
    },
}

/// Decode a `` `done `` value into its [`DoneOutcome`], `None` for anything
/// else — and since `decode_surface` in `shell_eval.rs` tries this branch last,
/// `None` drops the value rather than passing it on.
///
/// The record's sibling `cmd` field goes unread: [`done_card`] names the worker
/// generically, not by which one it was.
pub(crate) fn value_to_done(v: &RalValue) -> Option<DoneOutcome> {
    let RalValue::Variant { label, payload } = v else {
        return None;
    };
    if label != "done" {
        return None;
    }
    let m = map_of(payload.as_deref()?)?;
    let RalValue::Variant { label, payload } = m.get("outcome")? else {
        return None;
    };
    let outcome = match label.as_str() {
        "ok" => DoneOutcome::Ok,
        "err" => {
            let rec = map_of(payload.as_deref()?)?;
            DoneOutcome::Err {
                message: str_field(rec, "message").unwrap_or_default(),
                status: int_field(rec, "status").unwrap_or(0),
            }
        }
        "panic" => DoneOutcome::Panic {
            message: match payload.as_deref() {
                Some(RalValue::String(s)) => s.clone(),
                _ => String::new(),
            },
        },
        _ => return None,
    };
    Some(outcome)
}

/// Word a settled outcome as a one-line [`Card`] for the human — the same fact
/// `surface_notice` in `bus/post.rs` words for the model.
pub fn done_card(outcome: &DoneOutcome) -> Card {
    let mut spans = Vec::new();
    match outcome {
        DoneOutcome::Ok => {
            spans.push(Span::new(Role::Ok, "done"));
            spans.push(Span::plain("  Background block finished (exit 0)"));
        }
        DoneOutcome::Err { message, status } => {
            spans.push(Span::new(Role::Bad, format!("failed ({status})")));
            let mut body = String::from("  Background block error");
            if !message.is_empty() {
                let _ = write!(body, ": {message}");
            }
            spans.push(Span::plain(body));
        }
        DoneOutcome::Panic { message } => {
            spans.push(Span::new(Role::Bad, "panicked"));
            let mut body = String::from("  Background block error");
            if !message.is_empty() {
                let _ = write!(body, ": {message}");
            }
            spans.push(Span::plain(body));
        }
    }
    Card(vec![Mark::Text { spans }])
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{card_value, map_value, s};
    use super::*;

    /// Mirrors core's `done_event`: `cmd` plus a closed outcome variant.
    fn done_value(cmd: &str, outcome: RalValue) -> RalValue {
        RalValue::Variant {
            label: "done".into(),
            payload: Some(Box::new(RalValue::map(vec![
                ("cmd".into(), s(cmd)),
                ("outcome".into(), outcome),
            ]))),
        }
    }
    fn variant(label: &str, payload: RalValue) -> RalValue {
        RalValue::Variant {
            label: label.into(),
            payload: Some(Box::new(payload)),
        }
    }

    #[test]
    fn value_to_done_decodes_each_outcome() {
        assert_eq!(
            value_to_done(&done_value("<block>", variant("ok", RalValue::Unit))),
            Some(DoneOutcome::Ok)
        );
        let err = variant(
            "err",
            RalValue::map(vec![
                ("cmd".into(), s("<runtime>")),
                ("status".into(), RalValue::Int(2)),
                ("message".into(), s("boom")),
                ("line".into(), RalValue::Int(3)),
                ("col".into(), RalValue::Int(1)),
            ]),
        );
        assert_eq!(
            value_to_done(&done_value("<block>", err)),
            Some(DoneOutcome::Err {
                message: "boom".into(),
                status: 2,
            })
        );
        assert_eq!(
            value_to_done(&done_value("<block>", variant("panic", s("kaput")))),
            Some(DoneOutcome::Panic {
                message: "kaput".into(),
            })
        );
    }

    #[test]
    fn value_to_done_rejects_non_done_values() {
        assert!(value_to_done(&card_value(vec![])).is_none());
        assert!(value_to_done(&map_value(vec![("kind", s("read"))])).is_none());
        assert!(value_to_done(&s("plain")).is_none());
    }
}
