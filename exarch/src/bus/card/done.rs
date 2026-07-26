//! How a detached worker's completion reads.
//!
//! A background block settles one of three closed ways — a clean return, a
//! raised error, a panic — and core flushes that outcome as a single
//! `` `done `` value at the boundary.  [`value_to_done`] decodes it once into
//! a [`DoneOutcome`]; [`done_card`] composes the matching one-line report.

use ral_core::Value as RalValue;
use std::fmt::Write;

use super::value::{int_field, map_of, str_field};
use super::{Card, Mark, Role, Span};

/// How a detached `spawn` worker settled, as the final `` `done `` event core
/// appends to the worker's deferred buffer at completion.
///
/// Like an [`crate::bus::card::IoEvent`]
/// it is the raw record — [`value_to_done`] decodes it once and [`done_card`]
/// composes the matching one-line card.  Core names the event (`` `ok ``,
/// `` `err ``, `` `panic ``); exarch names its appearance: a fixed-position
/// outcome mark roled by result, never an animation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DoneOutcome {
    /// The worker returned cleanly.
    Ok,
    /// The worker raised — the caught error's `message` and `status`, the same
    /// fields `try`/`poll` surface.
    Err { message: String, status: i64 },
    /// The worker panicked — the panic message.
    Panic { message: String },
}

/// Decode the `` `done `` value a detached worker flushes at completion into
/// its [`DoneOutcome`].
///
/// The shape is `` `done [cmd: "…", outcome: …] `` where
/// `outcome` is the closed `` `ok ``/`` `err ``/`` `panic `` variant core mints;
/// `err` carries the `{cmd, status, message, line, col}` error record.  `cmd`
/// rides the value but is not decoded here — [`done_card`] names the worker
/// generically, not by which one it was, so only `outcome` is read.  Anything
/// else returns `None`; the decoder seam falls through.
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

/// Compose a `` `done `` event into a one-line [`Card`] using only the
/// existing [`Mark::Text`] vocabulary.
///
/// The card is an outcome span roled by how it settled — a clean
/// return is `Ok`, a raise is `Bad` carrying the message and status, a panic is
/// `Bad` carrying the message — followed by a plain gloss naming it as a
/// background block.
///
/// The outcome is a fixed-position value mark, not an
/// animation — the worker has already settled when this renders.
pub(crate) fn done_card(outcome: &DoneOutcome) -> Card {
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
    use super::super::testkit::{card_value, io_value, s};
    use super::*;

    /// Build the `` `done `` value a detached worker flushes — `cmd` plus a
    /// closed `` `ok ``/`` `err ``/`` `panic `` outcome — the way core mints it.
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

    /// The three outcome classes decode into their typed [`DoneOutcome`]: a
    /// clean `` `ok ``, an `` `err `` carrying the error record's message and
    /// status, and a `` `panic `` carrying its message.
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

    /// A non-`done` value is not a done event: a `` `card `` variant, an io
    /// `Map`, and a plain string all return `None` so the decoder seam drops
    /// them onto the next branch.
    #[test]
    fn value_to_done_rejects_non_done_values() {
        assert!(value_to_done(&card_value(vec![])).is_none());
        assert!(value_to_done(&io_value(vec![("io", s("read"))])).is_none());
        assert!(value_to_done(&s("plain")).is_none());
    }
}
