//! Presentation views; retained response evidence is never rewritten.

use super::ServiceFailure;
use serde_json::{Map, Value};

impl ServiceFailure {
    pub const BODY_DETAIL_CAP: usize = 200;

    pub fn response_error_object(body: &Value) -> Option<&Map<String, Value>> {
        body.get("error")
            .and_then(Value::as_object)
            .or_else(|| body.as_object())
    }

    pub fn response_message(body: &Value) -> Option<&str> {
        Self::response_error_object(body)?.get("message")?.as_str()
    }

    pub fn response_body(body: Value) -> Value {
        let Some(metadata) = Self::response_error_object(&body).and_then(|o| o.get("metadata"))
        else {
            return body;
        };
        let Some(inner) = metadata
            .get("raw")
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter(Value::is_object)
        else {
            return body;
        };
        let provider = metadata.get("provider_name").cloned();
        let mut inner = Self::response_body(inner);
        if let Some(provider) = provider {
            let object = if inner.get("error").is_some_and(Value::is_object) {
                inner.get_mut("error").and_then(Value::as_object_mut)
            } else {
                inner.as_object_mut()
            };
            if let Some(object) = object {
                object.insert("provider".into(), provider);
            }
        }
        inner
    }

    /// Normalize line boundaries before capping, including parsed messages.
    pub fn response_summary(text: &str) -> String {
        let mut chars = text
            .split(['\r', '\n'])
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .enumerate()
            .flat_map(|(index, line)| {
                let separator = if index == 0 { "" } else { "; " };
                separator.chars().chain(line.chars())
            });
        let prefix: String = chars.by_ref().take(Self::BODY_DETAIL_CAP).collect();
        if chars.next().is_some() {
            format!("{prefix}…")
        } else {
            prefix
        }
    }

    pub fn response_detail(body: &str) -> String {
        let trimmed = body.trim();
        let parsed = serde_json::from_str::<Value>(body)
            .ok()
            .map(Self::response_body);
        let message = parsed.as_ref().and_then(Self::response_message);
        let detail = match message {
            Some(message) if !message.trim().is_empty() => message,
            _ if trimmed.starts_with('<') => {
                "the endpoint returned a web page, not an error message"
            }
            _ if trimmed.is_empty() => "the endpoint sent no detail",
            _ if parsed.is_some() => "the endpoint returned JSON without an error message",
            _ => trimmed
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .trim(),
        };
        Self::response_summary(detail)
    }
}
