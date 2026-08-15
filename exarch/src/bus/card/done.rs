//! How a detached worker's completion reads.
//!
//! Core flushes a single `` `done `` value at the end of a background block's
//! deferred buffer; [`value_to_done`] decodes it, [`settled_spans`] words it.

use ral_core::Value as RalValue;

use super::value::{int_field, map_of, str_field};
use super::{Role, Span};

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
/// The record's sibling `cmd` field goes unread: core spells it `<block>` for
/// every `spawn`, so it names no worker in particular.
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

/// How a settled block reads: muted prose around the outcome.
///
/// The outcome alone carries a level, roled `ok`/`bad` exactly as the
/// `$ cmd → status` exec row roles an exit code — which is what a settled
/// block's is.
///
/// It names no worker: a `defer`'s `cmd` is core's constant `<block>`
/// (`prelude.ral`'s `defer` is `spawn`, which passes it), so there is nothing
/// yet to distinguish one settlement from another.
pub fn settled_spans(outcome: &DoneOutcome) -> Vec<Span> {
    let (role, how, message) = match outcome {
        DoneOutcome::Ok => (Role::Ok, "exit 0".to_string(), ""),
        DoneOutcome::Err { message, status } => {
            (Role::Bad, format!("exit {status}"), message.as_str())
        }
        DoneOutcome::Panic { message } => (Role::Bad, "panic".to_string(), message.as_str()),
    };
    let mut spans = vec![
        Span::new(Role::Muted, "background block settled ("),
        Span::new(role, how),
        Span::new(Role::Muted, ")"),
    ];
    if !message.is_empty() {
        spans.push(Span::new(Role::Muted, format!(": {message}")));
    }
    spans
}

/// [`settled_spans`] flattened, for the two sinks that have no ink to spend:
/// the headless stderr tee and the model's wake-up notice (`surface_notice` in
/// `bus/post.rs`).
pub fn settled_text(outcome: &DoneOutcome) -> String {
    settled_spans(outcome)
        .iter()
        .map(|s| s.text.as_str())
        .collect()
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
