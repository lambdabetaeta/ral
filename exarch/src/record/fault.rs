//! A provider failure's shared description: the one presentation-neutral
//! readout the TUI block and the headless printer both render.

use crate::clock;
use crate::provider::{self, CutShort, ProviderError, Recovery};
use serde_json::Value;
use std::borrow::Cow;

/// A field's value before presentation: text, or a wait the TUI also draws as
/// a bar.
pub enum Datum {
    Text(String),
    Seconds(u64),
}

/// One labelled row of a failure's readout.
pub struct Field {
    pub label: String,
    pub datum: Datum,
}

fn text_field(label: impl Into<String>, value: impl Into<String>) -> Field {
    Field {
        label: label.into(),
        datum: Datum::Text(value.into()),
    }
}

/// What the reader can do about a failure.  Each surface spells it its own
/// way — a slash command in the TUI, a flag headless — or not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remedy {
    Evict,
    Reroute,
    RaiseMaxTokens,
    SignIn,
}

/// A provider failure as a headline and ordered fields — the one description
/// both the TUI block and the headless printer render.
pub struct Readout {
    pub headline: String,
    pub fields: Vec<Field>,
    pub remedies: Vec<Remedy>,
}

impl Readout {
    /// A failure that ended its exchange: the `error: <kind>` headline, then
    /// an ordered field list.  A parsed `body` supplies the fields
    /// ([`body_fields`]); without one the free-text `cause`/`message` is
    /// shown honestly rather than dressed as structure.
    pub fn fatal(e: &ProviderError) -> Self {
        let headline = if let ProviderError::Cancelled(site) = e {
            format!("cancelled ({site})")
        } else {
            e.kind().to_string()
        };
        Self {
            headline,
            fields: error_fields(e),
            remedies: remedies(e),
        }
    }

    /// A stall that kept its partial reply: the same field list a fatal failure
    /// gets, under a headline that says so.  Whether the exchange goes on is
    /// the nudge's record to say, not this readout's.
    pub fn stall(e: &ProviderError) -> Self {
        let mut fields = error_fields(e);
        fields.push(text_field("kept", "partial reply"));
        Self {
            headline: "stream stalled".to_string(),
            fields,
            remedies: vec![],
        }
    }
}

/// What can be done about `e`, derived from it and never stored.
fn remedies(e: &ProviderError) -> Vec<Remedy> {
    match e {
        ProviderError::Overflow { .. } => vec![Remedy::Evict, Remedy::Reroute],
        ProviderError::Truncated { cause } => match **cause {
            CutShort::OutputCap { .. } => vec![Remedy::RaiseMaxTokens],
            CutShort::Stalled(_) => vec![],
        },
        ProviderError::Api {
            status: Some(401 | 403),
            ..
        } => vec![Remedy::SignIn],
        ProviderError::Cancelled(_)
        | ProviderError::Transient { .. }
        | ProviderError::Refused(_)
        | ProviderError::Api { .. }
        | ProviderError::Other(_) => vec![],
    }
}

/// The ordered field list under either headline.
fn error_fields(e: &ProviderError) -> Vec<Field> {
    match e {
        ProviderError::Cancelled(_) => Vec::new(),
        ProviderError::Refused(r) => match r.recovery() {
            Recovery::InPlace(Some(wait)) => Some(Field {
                label: "retry-after".into(),
                datum: Datum::Seconds(wait.get().as_secs()),
            }),
            Recovery::InPlace(None) => None,
            Recovery::Deferred(at) => Some(text_field("resets", clock::local(at))),
        }
        .into_iter()
        .chain(body_or_cause(
            r.body.as_deref(),
            provider::reset::BODY_KEYS,
            &r.cause,
        ))
        .collect(),
        ProviderError::Transient {
            cause,
            attempts,
            body,
            status,
        } => std::iter::once(text_field("attempts", attempts.to_string()))
            .chain(status.map(status_field))
            .chain(body_or_cause(body.as_deref(), &[], cause))
            .collect(),
        ProviderError::Api {
            status,
            model,
            message,
            body,
        } => {
            let mut fs: Vec<Field> = status.map(status_field).into_iter().collect();
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
        ProviderError::Overflow {
            provider,
            model,
            sent,
            limit,
            listed,
            body,
        } => {
            let tokens = |n: u64| format!("{n} tokens");
            [
                provider.clone().map(|p| text_field("provider", p)),
                Some(text_field("model", model.clone())),
                sent.map(|n| text_field("sent", tokens(n))),
                limit.map(|n| text_field("limit", tokens(n))),
                listed
                    .filter(|l| Some(*l) != *limit)
                    .map(|n| text_field("listed", tokens(n))),
                body.as_deref()
                    .filter(|_| limit.is_none())
                    .and_then(provider::error_object)
                    .and_then(|o| o.get("message"))
                    .and_then(value_display)
                    .map(|m| text_field("message", m)),
            ]
            .into_iter()
            .flatten()
            .collect()
        }
        // A stall's fields are its cause's: `record::view` routes one to
        // [`Readout::stall`] with the cause already unwrapped, so the
        // delegation here is what keeps this match total rather than a path
        // either renderer walks.
        ProviderError::Truncated { cause } => match cause.as_ref() {
            CutShort::OutputCap { stop_reason } => {
                vec![text_field("stop_reason", stop_reason.clone())]
            }
            CutShort::Stalled(error) => error_fields(error),
        },
        ProviderError::Other(cause) => vec![text_field("cause", prettify(cause))],
    }
}

fn status_field(status: u16) -> Field {
    text_field("status", status.to_string())
}

/// A failure's body fields, the `consumed` keys its dedicated fields already
/// carry suppressed; the free-text `cause` when there is no body.
fn body_or_cause(body: Option<&Value>, consumed: &[&str], cause: &str) -> Vec<Field> {
    match body {
        Some(b) => body_fields(b, consumed),
        None => vec![text_field("cause", prettify(cause))],
    }
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

/// Flatten a parsed error `body` into ordered fields: `type` and `code` lead as
/// the machine-readable class, then the rest in the `Map`'s sorted order,
/// skipping nulls and anything already `consumed` by a dedicated field, with
/// `message` last because it is the one that wraps.
fn body_fields(body: &Value, consumed: &[&str]) -> Vec<Field> {
    let Some(obj) = provider::error_object(body) else {
        return vec![];
    };
    let mut fs = Vec::new();
    for key in ["type", "code"] {
        if let Some(v) = obj.get(key).and_then(value_display) {
            fs.push(text_field(key, v));
        }
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
