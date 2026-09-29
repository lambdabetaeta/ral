//! What a 429 says: when the provider may be asked again, read from every
//! convention that names it, and what ran out. `at` takes the latest instant
//! any names: asking before the last named clock runs out is refused again.

use super::error::{Limit, error_object};
use jiff::Timestamp;
use jiff::fmt::rfc2822::DateTimeParser;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value};
use std::time::Duration;

/// The error-object keys that name a reset, each read by [`epoch_or_delta`].
pub(crate) const BODY_KEYS: &[&str] = &[
    "resets_at",
    "resets_in_seconds",
    "retry_after_seconds",
    "retry_after",
];

/// Error `type`s and `code`s of a quota or credit spent, which no wait clears.
const UNWAITABLE: &[&str] = &[
    "insufficient_quota",
    "usage_not_included",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
];

/// Error `type`s and `code`s of a plan's allowance spent, account-wide.
const ALLOWANCE: &[&str] = &["usage_limit_reached"];

pub(super) fn at(
    headers: Option<&HeaderMap>,
    body: Option<&Value>,
    msg: &str,
    now: Timestamp,
) -> Option<Timestamp> {
    let from_headers = headers.into_iter().flat_map(|h| {
        [
            retry_after_ms(h, now),
            retry_after(h, now),
            ratelimit_reset(h, now),
        ]
    });
    let from_body = body
        .and_then(error_object)
        .into_iter()
        .flat_map(|o| [named(o, now), retry_info(o, now), relayed_limit(o, now)]);
    from_headers
        .chain(from_body)
        .flatten()
        .max()
        .or_else(|| parse_retry_after(msg).and_then(|wait| now.checked_add(wait).ok()))
}

/// A quota or credit spent rather than a rate exceeded.
pub(super) fn unwaitable(body: &Value) -> bool {
    names(body, UNWAITABLE)
}

/// What `body` names as run out: the plan's allowance, or else a rate.
pub(super) fn limit(body: &Value) -> Limit {
    if names(body, ALLOWANCE) {
        Limit::Allowance
    } else {
        Limit::Rate
    }
}

/// Whether the error's `type` or `code` is one of `list`.
fn names(body: &Value, list: &[&str]) -> bool {
    error_object(body).is_some_and(|o| {
        ["type", "code"]
            .iter()
            .filter_map(|k| o.get(*k)?.as_str())
            .any(|s| list.contains(&s))
    })
}

fn retry_after_ms(headers: &HeaderMap, now: Timestamp) -> Option<Timestamp> {
    after(now, header_number(headers, "retry-after-ms")? / 1000.0)
}

/// Delta seconds, or the HTTP-date RFC 9110 also allows.
fn retry_after(headers: &HeaderMap, now: Timestamp) -> Option<Timestamp> {
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    match value.parse::<f64>() {
        Ok(secs) => after(now, secs),
        Err(_) => DateTimeParser::new().parse_timestamp(value).ok(),
    }
}

fn ratelimit_reset(headers: &HeaderMap, now: Timestamp) -> Option<Timestamp> {
    ["x-ratelimit-reset", "ratelimit-reset"]
        .iter()
        .filter_map(|name| epoch_or_delta(header_number(headers, name)?, now))
        .max()
}

fn named(obj: &Map<String, Value>, now: Timestamp) -> Option<Timestamp> {
    BODY_KEYS
        .iter()
        .filter_map(|k| epoch_or_delta(number(obj.get(*k)?)?, now))
        .max()
}

/// Gemini's `google.rpc.RetryInfo` detail, whose `retryDelay` reads `"38s"`.
fn retry_info(obj: &Map<String, Value>, now: Timestamp) -> Option<Timestamp> {
    obj.get("details")?
        .as_array()?
        .iter()
        .filter(|d| {
            d.get("@type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.ends_with("google.rpc.RetryInfo"))
        })
        .filter_map(|d| {
            let secs = d.get("retryDelay")?.as_str()?.strip_suffix('s')?;
            after(now, secs.trim().parse().ok()?)
        })
        .max()
}

/// The upstream's `x-ratelimit-reset`, as a gateway relays it in `metadata`.
fn relayed_limit(obj: &Map<String, Value>, now: Timestamp) -> Option<Timestamp> {
    obj.get("metadata")?
        .get("headers")?
        .as_object()?
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("x-ratelimit-reset"))
        .filter_map(|(_, v)| epoch_or_delta(number(v)?, now))
        .max()
}

/// Magnitude alone tells the three vendor conventions apart, since no real
/// wait is 31 years long: epoch milliseconds, epoch seconds, or a delta.
fn epoch_or_delta(n: f64, now: Timestamp) -> Option<Timestamp> {
    if n >= 1e12 {
        after(Timestamp::UNIX_EPOCH, n / 1000.0)
    } else if n >= 1e9 {
        after(Timestamp::UNIX_EPOCH, n)
    } else {
        after(now, n)
    }
}

fn after(now: Timestamp, secs: f64) -> Option<Timestamp> {
    now.checked_add(Duration::try_from_secs_f64(secs).ok()?)
        .ok()
}

/// A JSON number, or a string holding one.
fn number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| parse(v.as_str()?))
}

fn header_number(headers: &HeaderMap, name: &str) -> Option<f64> {
    parse(headers.get(name)?.to_str().ok()?)
}

fn parse(text: &str) -> Option<f64> {
    text.trim().parse().ok().filter(|n: &f64| n.is_finite())
}

/// The last resort when nothing structural names a reset: genai surfaces some
/// 429s with the wait only in their text. Missing is fine — the retry loop
/// then backs off exponentially.
fn parse_retry_after(msg: &str) -> Option<Duration> {
    let needle = "retry-after";
    // Slice the lowercased copy, never the original with an offset taken from
    // it: a char whose lowercasing changes byte length (`İ`) shifts every later
    // offset, so `&msg[i..]` could land mid-character and panic.
    let lower = msg.to_lowercase();
    let i = lower.find(needle)?;
    let tail = &lower[i + needle.len()..];
    let digits: String = tail
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    let secs: u64 = digits.parse().ok()?;
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use serde_json::json;

    const NOW: i64 = 1_700_000_000;

    fn now() -> Timestamp {
        Timestamp::from_second(NOW).unwrap()
    }

    fn secs_after_now(secs: i64) -> Timestamp {
        Timestamp::from_second(NOW + secs).unwrap()
    }

    fn header(name: &'static str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_str(value).unwrap());
        headers
    }

    fn from_header(name: &'static str, value: &str) -> Option<Timestamp> {
        at(Some(&header(name, value)), None, "", now())
    }

    fn from_body(body: &Value) -> Option<Timestamp> {
        at(None, Some(body), "", now())
    }

    #[test]
    fn retry_after_ms_is_a_fractional_delta() {
        assert_eq!(
            from_header("retry-after-ms", "1500.5"),
            Some(now().checked_add(Duration::from_micros(1_500_500)).unwrap())
        );
    }

    #[test]
    fn retry_after_is_seconds_or_an_http_date() {
        assert_eq!(from_header("retry-after", "7"), Some(secs_after_now(7)));
        assert_eq!(
            from_header("retry-after", "Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(Timestamp::from_second(784_111_777).unwrap()),
            "a date already past is read as that instant, not floored here"
        );
    }

    #[test]
    fn ratelimit_reset_headers_are_epoch_or_delta() {
        assert_eq!(
            from_header("x-ratelimit-reset", &(NOW + 90).to_string()),
            Some(secs_after_now(90))
        );
        assert_eq!(
            from_header("ratelimit-reset", "45"),
            Some(secs_after_now(45))
        );
    }

    #[test]
    fn body_resets_at_is_epoch_seconds() {
        let body = json!({"error": {"type": "usage_limit_reached", "resets_at": NOW + 18_000}});
        assert_eq!(from_body(&body), Some(secs_after_now(18_000)));
    }

    #[test]
    fn body_waits_are_delta_seconds_as_numbers_or_strings() {
        for key in &BODY_KEYS[1..] {
            assert_eq!(
                from_body(&json!({"error": {(*key): 120}})),
                Some(secs_after_now(120)),
                "{key}"
            );
        }
        assert_eq!(
            from_body(&json!({"retry_after": "30"})),
            Some(secs_after_now(30)),
            "a flat body with a numeric string"
        );
    }

    #[test]
    fn gemini_retry_info_reads_its_delay() {
        let body = json!({"error": {"code": 429, "details": [
            {"@type": "type.googleapis.com/google.rpc.Help"},
            {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "38s"},
        ]}});
        assert_eq!(from_body(&body), Some(secs_after_now(38)));
        let fractional = json!({"error": {"details": [
            {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "0.5s"},
        ]}});
        assert_eq!(
            from_body(&fractional),
            Some(now().checked_add(Duration::from_millis(500)).unwrap())
        );
    }

    #[test]
    fn a_relayed_limit_header_matches_case_insensitively() {
        let epoch_ms = (NOW + 60) * 1000;
        let body = json!({"error": {"metadata": {"headers": {"X-RateLimit-Reset": epoch_ms.to_string()}}}});
        assert_eq!(from_body(&body), Some(secs_after_now(60)));
    }

    #[test]
    fn the_latest_named_instant_wins() {
        let headers = header("retry-after", "10");
        let body = json!({"error": {"resets_at": NOW + 3600, "retry_after": 5}});
        assert_eq!(
            at(Some(&headers), Some(&body), "retry-after: 99999", now()),
            Some(secs_after_now(3600))
        );
    }

    #[test]
    fn the_text_scrape_answers_only_when_nothing_structural_does() {
        assert_eq!(
            at(None, None, "429 retry-after: 12", now()),
            Some(secs_after_now(12))
        );
        assert_eq!(at(None, None, "429 too many", now()), None);
    }

    #[test]
    fn unwaitable_reads_type_and_code() {
        assert!(unwaitable(
            &json!({"error": {"type": "insufficient_quota"}})
        ));
        assert!(unwaitable(
            &json!({"error": {"code": "usage_not_included"}})
        ));
        assert!(!unwaitable(
            &json!({"error": {"type": "usage_limit_reached"}})
        ));
        assert!(!unwaitable(&json!({"error": {"code": 429}})));
    }

    #[test]
    fn limit_reads_the_allowance_on_type_and_code() {
        assert_eq!(
            limit(&json!({"error": {"type": "usage_limit_reached"}})),
            Limit::Allowance
        );
        assert_eq!(
            limit(&json!({"error": {"code": "usage_limit_reached"}})),
            Limit::Allowance
        );
        assert_eq!(
            limit(&json!({"error": {"type": "rate_limit_exceeded"}})),
            Limit::Rate
        );
        assert_eq!(limit(&json!({"error": {"code": 429}})), Limit::Rate);
    }

    /// `İ` → `i̇` is one byte longer, so a slice taken from the shorter original
    /// would land mid-character and panic.
    #[test]
    fn parse_retry_after_survives_length_changing_lowercase() {
        assert_eq!(
            parse_retry_after("İ retry-after: 9 seconds"),
            Some(Duration::from_secs(9))
        );
    }
}
