//! A provider failure's shared description: the one presentation-neutral
//! readout the TUI block and the headless printer both render.

use crate::agent::event::{CutShortRecord, ProviderErrorRecord};
use crate::provider;
use serde_json::Value;
use std::borrow::Cow;

/// A field's value before presentation: text, or a wait the TUI also draws as
/// a bar.
pub(crate) enum Datum {
    Text(String),
    Seconds(u64),
}

/// One labelled row of a failure's readout.
pub(crate) struct Field {
    pub(crate) label: String,
    pub(crate) datum: Datum,
}

fn text_field(label: impl Into<String>, value: impl Into<String>) -> Field {
    Field {
        label: label.into(),
        datum: Datum::Text(value.into()),
    }
}

/// A provider failure as a headline and ordered fields — the one description
/// both the TUI block and the headless printer render.
pub(crate) struct Readout {
    pub(crate) headline: String,
    pub(crate) fields: Vec<Field>,
}

impl Readout {
    /// A failure that ended its exchange: the `error: <kind>` headline, then
    /// an ordered field list.  A parsed `body` supplies the fields
    /// ([`body_fields`]); without one the free-text `cause`/`message` is
    /// shown honestly rather than dressed as structure.
    pub(crate) fn fatal(e: &ProviderErrorRecord) -> Self {
        // Cancellation folds its site into the headline and carries no body.
        if let ProviderErrorRecord::Cancelled { where_ } = e {
            return Self {
                headline: format!("cancelled ({where_})"),
                fields: Vec::new(),
            };
        }
        Self {
            headline: error_kind(e).to_string(),
            fields: error_fields(e),
        }
    }

    /// A stall the exchange survived: the same field list a fatal failure
    /// gets, under a headline that says so.  The `continuing` field is the
    /// whole distinction — without it the readout would read as the end of
    /// the run, which is precisely what a stall is not.
    pub(crate) fn stall(e: &ProviderErrorRecord) -> Self {
        let mut fields = error_fields(e);
        fields.push(text_field("continuing", "partial reply kept"));
        Self {
            headline: "stream stalled".to_string(),
            fields,
        }
    }
}

/// Body keys carrying the retry-after wait as a second count, in precedence
/// order — read by [`wait_from_body`], then suppressed from the body dump
/// (along with the absolute `resets_at` twin) once the retry-after field
/// carries them.
const RETRY_SECS_KEYS: &[&str] = &["resets_in_seconds", "retry_after_seconds", "retry_after"];

/// The ordered field list under either headline.  `Cancelled` never reaches
/// here: [`Readout::fatal`] returns before the call, and a cancel never
/// commits as a stall — `Engine::complete` exempts it by name.
fn error_fields(e: &ProviderErrorRecord) -> Vec<Field> {
    match e {
        ProviderErrorRecord::Cancelled { .. } => unreachable!("cancellation carries no fields"),
        ProviderErrorRecord::RateLimited {
            retry_after_secs,
            cause,
            body,
        } => {
            let mut fs = Vec::new();
            let wait = retry_after_secs.or_else(|| body.as_ref().and_then(wait_from_body));
            if let Some(secs) = wait {
                fs.push(Field {
                    label: "retry-after".into(),
                    datum: Datum::Seconds(secs),
                });
            }
            match body {
                // Suppress the raw keys the wait field already subsumes.
                Some(b) => {
                    let consumed: Vec<&str> = RETRY_SECS_KEYS
                        .iter()
                        .copied()
                        .chain(["resets_at"])
                        .collect();
                    fs.extend(body_fields(b, &consumed));
                }
                None => fs.push(text_field("cause", prettify(cause))),
            }
            fs
        }
        ProviderErrorRecord::Transient {
            cause,
            attempts,
            body,
            status,
        } => {
            let mut fs = vec![text_field("attempts", attempts.to_string())];
            if let Some(s) = status {
                fs.push(text_field("status", s.to_string()));
            }
            match body {
                Some(b) => fs.extend(body_fields(b, &[])),
                None => fs.push(text_field("cause", prettify(cause))),
            }
            fs
        }
        ProviderErrorRecord::Api {
            status,
            model,
            message,
            body,
        } => {
            let mut fs = Vec::new();
            if let Some(s) = status {
                fs.push(text_field("status", s.to_string()));
            }
            fs.push(text_field("model", model.clone()));
            if let Some(u) = provider::extract_url(message) {
                fs.push(text_field("url", u));
            }
            match body {
                Some(b) => fs.extend(body_fields(b, &[])),
                None => fs.push(text_field("message", message.clone())),
            }
            fs
        }
        // A stall's fields are its cause's: `record::view` routes one to
        // [`Readout::stall`] with the cause already unwrapped, so the
        // delegation here is what keeps this match total rather than a path
        // either renderer walks.
        ProviderErrorRecord::Truncated { cause } => match cause {
            CutShortRecord::OutputCap { stop_reason } => vec![
                text_field("stop_reason", stop_reason.clone()),
                text_field(
                    "remedy",
                    "raise `--max-tokens N` or split the turn into smaller writes",
                ),
            ],
            CutShortRecord::Stalled { error } => error_fields(error),
        },
        ProviderErrorRecord::Other { cause } => vec![text_field("cause", prettify(cause))],
    }
}

/// Short human label for the error headline.
fn error_kind(e: &ProviderErrorRecord) -> &'static str {
    match e {
        ProviderErrorRecord::Cancelled { .. } => "cancelled",
        ProviderErrorRecord::Transient { status, .. } => provider::transient_label(*status),
        ProviderErrorRecord::RateLimited { .. } => "rate limited",
        ProviderErrorRecord::Api { .. } => "api error",
        ProviderErrorRecord::Truncated { .. } => "truncated",
        ProviderErrorRecord::Other { .. } => "provider error",
    }
}

/// The retry-after wait carried by a parsed `body`: the first
/// [`RETRY_SECS_KEYS`] entry reading as a number.  Consulted only when the
/// response header did not already supply the wait.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "float->int cast saturates: a negative or absurd retry-seconds pins to 0 / u64::MAX, both acceptable for a wait readout"
)]
fn wait_from_body(body: &Value) -> Option<u64> {
    let obj = provider::error_object(body)?;
    RETRY_SECS_KEYS.iter().find_map(|k| {
        obj.get(*k)
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
    })
}

/// One JSON value as the text a field row shows, syntax stripped.  `Null`
/// carries no action, so it renders as nothing and the field is dropped.
fn value_display(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(_) | Value::Number(_) => Some(v.to_string()),
        Value::Array(_) | Value::Object(_) => Some(serde_json::to_string(v).unwrap_or_default()),
    }
}

/// Flatten a parsed error `body` into ordered fields: `type`/`code` leads as
/// the machine-readable class, then the rest in the `Map`'s sorted order,
/// skipping nulls and anything already `consumed` by a dedicated field, with
/// `message` last because it is the one that wraps.
fn body_fields(body: &Value, consumed: &[&str]) -> Vec<Field> {
    let Some(obj) = provider::error_object(body) else {
        return vec![];
    };
    let mut fs = Vec::new();
    if let Some(v) = obj
        .get("type")
        .or_else(|| obj.get("code"))
        .and_then(value_display)
    {
        fs.push(text_field("type", v));
    }
    for (k, v) in obj {
        if matches!(k.as_str(), "type" | "code" | "message") || consumed.contains(&k.as_str()) {
            continue;
        }
        if let Some(v) = value_display(v) {
            fs.push(text_field(k.clone(), v));
        }
    }
    if let Some(v) = obj.get("message").and_then(value_display) {
        fs.push(text_field("message", v));
    }
    fs
}

/// Owned-`String` form of [`prettify_embedded_json`].
fn prettify(s: &str) -> String {
    prettify_embedded_json(s).into_owned()
}

/// Re-indent the first embedded JSON object or array in `s`, leaving the
/// surrounding text intact: providers splice a single-line body into a
/// free-text `cause`, and pretty-printing turns that wall into a nested block
/// whose newlines the wrapper honours as hard breaks.
fn prettify_embedded_json(s: &str) -> Cow<'_, str> {
    let Some(start) = s.find(['{', '[']) else {
        return Cow::Borrowed(s);
    };
    let mut stream =
        serde_json::Deserializer::from_str(&s[start..]).into_iter::<serde_json::Value>();
    let Some(Ok(value)) = stream.next() else {
        return Cow::Borrowed(s);
    };
    let Ok(pretty) = serde_json::to_string_pretty(&value) else {
        return Cow::Borrowed(s);
    };
    let end = start + stream.byte_offset();
    Cow::Owned(format!("{}{}{}", &s[..start], pretty, &s[end..]))
}
