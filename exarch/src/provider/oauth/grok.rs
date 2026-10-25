//! xAI's RFC 8628 device sign-in, adapted from pi-ai 1.4.0's
//! `dist/auth/oauth/{xai,device-code}.js`.
//!
//! Initial identity requires the issuer's ID-token subject. The payload is
//! decoded, not signature-verified: the TLS token endpoint supplies it, and it
//! names an account rather than granting additional authority.
//
// Upstream: https://github.com/earendil-works/pi/tree/v1.4.0/packages/ai
// MIT License
//
// Copyright (c) 2025 Mario Zechner
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use super::{
    Grant, LoginPhase, OAuthToken, RefreshResponse, Revocation, SignIn, json_or_error, jwt_payload,
    refresh_failure, revoke_at, wait,
};
use crate::provider::error::body_detail;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::Instant;

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const EXPIRED: &str = "the device code expired; start the sign-in again";
const DENIED: &str = "xAI refused the sign-in; start again if you want to choose another account";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    #[serde(default, deserialize_with = "device_interval")]
    interval: Option<u64>,
    #[serde(deserialize_with = "device_lifetime")]
    expires_in: u64,
}

#[derive(Debug)]
struct DeviceCode {
    code: String,
    user_code: String,
    url: String,
    interval: u64,
    expires_in: u64,
}

impl DeviceResponse {
    fn validate(self) -> Result<DeviceCode, String> {
        required(&self.device_code, "device_code")?;
        required(&self.user_code, "user_code")?;
        required(&self.verification_uri, "verification_uri")?;
        if self.expires_in == 0 {
            return Err(invalid_field("expires_in"));
        }
        let ordinary = verification_url(&self.verification_uri)?;
        let url = match self.verification_uri_complete {
            Some(complete) if !complete.is_empty() => verification_url(&complete)?,
            _ => ordinary,
        };
        Ok(DeviceCode {
            code: self.device_code,
            user_code: self.user_code,
            url,
            interval: self.interval.filter(|value| *value > 0).unwrap_or(5),
            expires_in: self.expires_in,
        })
    }
}

fn invalid_field(field: &str) -> String {
    format!("invalid xAI OAuth response field: {field}")
}

fn device_interval<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    u64::deserialize(deserializer)
        .map(Some)
        .map_err(|_| serde::de::Error::custom(invalid_field("interval (expected an integer)")))
}

fn device_lifetime<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    u64::deserialize(deserializer)
        .map_err(|_| serde::de::Error::custom(invalid_field("expires_in (expected an integer)")))
}

fn required(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        Err(invalid_field(field))
    } else {
        Ok(())
    }
}

fn verification_url(raw: &str) -> Result<String, String> {
    let url = Url::parse(raw)
        .map_err(|_| "untrusted verification URI in xAI OAuth response".to_string())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("untrusted verification URI in xAI OAuth response".to_string());
    }
    Ok(url.to_string())
}

fn expiry_display(seconds: u64) -> String {
    if seconds.is_multiple_of(60) {
        let minutes = seconds / 60;
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    } else {
        format!("{seconds} second{}", if seconds == 1 { "" } else { "s" })
    }
}

fn remaining(
    start: Instant,
    lifetime: Duration,
    cancel: &Arc<AtomicBool>,
) -> Result<Duration, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("sign-in cancelled".to_string());
    }
    lifetime
        .checked_sub(start.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| EXPIRED.to_string())
}

pub(super) async fn device(
    client: &Client,
    _sign_in: &SignIn,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<OAuthToken, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("sign-in cancelled".to_string());
    }
    // The code's lifetime includes the authorization request's latency.
    let start = Instant::now();
    let device = request_device_code(client, DEVICE_CODE_URL).await?;
    remaining(start, Duration::from_secs(device.expires_in), cancel)?;
    on_phase(LoginPhase::AwaitingDevice {
        user_code: device.user_code.clone(),
        url: device.url.clone(),
        expires_in: expiry_display(device.expires_in),
    });
    poll_for_tokens(client, TOKEN_URL, &device, start, on_phase, cancel).await
}

async fn request_device_code(client: &Client, url: &str) -> Result<DeviceCode, String> {
    let response = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .form(&[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("referrer", "exarch"),
        ])
        .send()
        .await
        .map_err(|error| format!("xAI device authorization request failed: {error}"))?;
    json_or_error::<DeviceResponse>(response, "xAI device authorization")
        .await?
        .validate()
}

async fn poll_for_tokens(
    client: &Client,
    token_url: &str,
    device: &DeviceCode,
    start: Instant,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<OAuthToken, String> {
    let lifetime = Duration::from_secs(device.expires_in);
    let mut interval = device.interval;
    loop {
        let left = remaining(start, lifetime, cancel)?;
        wait(Duration::from_secs(interval).min(left), cancel).await?;
        let timeout = remaining(start, lifetime, cancel)?.min(REQUEST_TIMEOUT);
        // One bound covers both sending the request and reading its body.
        let result = tokio::time::timeout(timeout, async {
            let response = client
                .post(token_url)
                .header(reqwest::header::ACCEPT, "application/json")
                .timeout(timeout)
                .form(&[
                    ("grant_type", DEVICE_GRANT),
                    ("client_id", CLIENT_ID),
                    ("device_code", device.code.as_str()),
                ])
                .send()
                .await
                .map_err(|error| format!("xAI device token request failed: {error}"))?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|error| format!("could not read xAI device token response: {error}"))?;
            Ok::<_, String>((status, body))
        })
        .await;
        remaining(start, lifetime, cancel)?;
        let (status, body) =
            result.map_err(|_| "xAI device token request timed out".to_string())??;
        if status.is_success() {
            let raw = serde_json::from_str(&body)
                .map_err(|error| format!("could not parse xAI device token response: {error}"))?;
            on_phase(LoginPhase::ExchangingCode);
            let token = finalize(raw)?;
            remaining(start, lifetime, cancel)?;
            return Ok(token);
        }
        interval = poll_interval(status, &body, interval)?;
    }
}

#[derive(Deserialize)]
struct PollError {
    error: String,
    #[serde(rename = "error_description")]
    _description: Option<String>,
    interval: Option<u64>,
}

/// RFC 8628's mandatory five-second increase wins over a smaller server hint.
fn poll_interval(status: StatusCode, body: &str, previous: u64) -> Result<u64, String> {
    let failure = || {
        format!(
            "xAI device token polling failed ({status}): {}",
            body_detail(body)
        )
    };
    if status.is_server_error() {
        return Err(failure());
    }
    let error: PollError = serde_json::from_str(body).map_err(|_| failure())?;
    match error.error.as_str() {
        "authorization_pending" => Ok(previous),
        "slow_down" => Ok(previous
            .saturating_add(5)
            .max(error.interval.unwrap_or_default())),
        "access_denied" | "authorization_denied" => Err(DENIED.to_string()),
        "expired_token" => Err(EXPIRED.to_string()),
        _ => Err(failure()),
    }
}

#[derive(Deserialize)]
struct IdClaims {
    sub: String,
    email: Option<String>,
}

fn expires_at(expires_in: Option<u64>, now: u64) -> Result<u64, String> {
    let seconds = expires_in.unwrap_or(3600);
    if seconds == 0 {
        return Err(invalid_field("expires_in"));
    }
    now.checked_add(seconds)
        .ok_or_else(|| invalid_field("expires_in (overflow)"))
}

fn finalize(raw: RefreshResponse) -> Result<OAuthToken, String> {
    required(&raw.access_token, "access_token")?;
    let refresh_token = raw
        .refresh_token
        .ok_or_else(|| invalid_field("refresh_token"))?;
    required(&refresh_token, "refresh_token")?;
    let id_token = raw.id_token.filter(|token| !token.is_empty()).ok_or_else(|| {
        "xAI's sign-in named no account: its response carried no ID token, and accounts are told apart by it"
            .to_string()
    })?;
    let claims: IdClaims = jwt_payload(&id_token)
        .map_err(|error| format!("xAI's sign-in named no account: invalid ID token: {error}"))?;
    if claims.sub.is_empty() {
        return Err(
            "xAI's sign-in named no account: its ID token carried an empty subject".to_string(),
        );
    }
    Ok(OAuthToken {
        access_token: raw.access_token,
        refresh_token,
        issued: claims.sub,
        email: claims.email,
        expires_at: expires_at(raw.expires_in, crate::app::now_secs())?,
        grant: Grant::Grok,
    })
}

pub(super) async fn refresh(client: &Client, previous: &OAuthToken) -> Result<OAuthToken, String> {
    refresh_at(client, TOKEN_URL, previous).await
}

async fn refresh_at(
    client: &Client,
    url: &str,
    previous: &OAuthToken,
) -> Result<OAuthToken, String> {
    let response = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", previous.refresh_token.as_str()),
        ])
        .send()
        .await
        .map_err(|error| format!("xAI token refresh request failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(refresh_failure(status, &body, previous));
    }
    let raw = response
        .json()
        .await
        .map_err(|error| format!("could not parse xAI token refresh response: {error}"))?;
    renewed(previous, raw)
}

fn renewed(previous: &OAuthToken, raw: RefreshResponse) -> Result<OAuthToken, String> {
    required(&raw.access_token, "access_token")?;
    let refresh_token = raw
        .refresh_token
        .unwrap_or_else(|| previous.refresh_token.clone());
    required(&refresh_token, "refresh_token")?;
    if let Some(id_token) = &raw.id_token {
        required(id_token, "id_token")?;
    }
    let email = raw
        .id_token
        .as_deref()
        .and_then(|token| jwt_payload::<IdClaims>(token).ok())
        .and_then(|claims| claims.email)
        .or_else(|| previous.email.clone());
    Ok(OAuthToken {
        access_token: raw.access_token,
        refresh_token,
        issued: previous.issued.clone(),
        email,
        expires_at: expires_at(raw.expires_in, crate::app::now_secs())?,
        grant: previous.grant.clone(),
    })
}

pub(super) async fn revoke(token: &OAuthToken) -> Result<Revocation, String> {
    revoke_at(DISCOVERY_URL, token, CLIENT_ID).await
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    fn jwt(claims: &Value) -> String {
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    fn device_json() -> Value {
        json!({
            "device_code": "device-secret", "user_code": "ABCD-EFGH",
            "verification_uri": "https://auth.x.ai/activate", "expires_in": 600
        })
    }

    fn parse_device(value: Value) -> Result<DeviceCode, String> {
        serde_json::from_value::<DeviceResponse>(value)
            .map_err(|error| format!("could not parse xAI device authorization response: {error}"))?
            .validate()
    }

    fn token_json() -> Value {
        json!({
            "access_token": "access", "refresh_token": "refresh",
            "id_token": jwt(&json!({"sub": "subject", "email": "first@example.test"}))
        })
    }

    fn parse_token(value: Value) -> Result<OAuthToken, String> {
        let raw = serde_json::from_value(value).map_err(|error| error.to_string())?;
        finalize(raw)
    }

    fn refresh_response(value: Value) -> RefreshResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn device_response_validates_required_fields_urls_and_durations() {
        let parsed = parse_device(device_json()).unwrap();
        assert_eq!(parsed.code, "device-secret");
        assert_eq!(parsed.user_code, "ABCD-EFGH");
        assert_eq!(parsed.url, "https://auth.x.ai/activate");
        assert_eq!(parsed.interval, 5);
        assert_eq!(parsed.expires_in, 600);
        for complete in [Value::Null, json!("")] {
            let mut value = device_json();
            value["verification_uri_complete"] = complete;
            assert_eq!(parse_device(value).unwrap().url, parsed.url);
        }
        let mut complete = device_json();
        complete["verification_uri_complete"] = json!("https://auth.x.ai/activate?code=ABCD");
        assert_eq!(
            parse_device(complete.clone()).unwrap().url,
            "https://auth.x.ai/activate?code=ABCD"
        );
        for field in ["device_code", "user_code", "verification_uri", "expires_in"] {
            let mut missing = device_json();
            missing.as_object_mut().unwrap().remove(field);
            assert!(parse_device(missing).unwrap_err().contains(field));
            let mut empty = device_json();
            empty[field] = if field == "expires_in" {
                json!(0)
            } else {
                json!("")
            };
            assert!(parse_device(empty).unwrap_err().contains(field));
        }
        for url in [
            "http://auth.x.ai/activate",
            "javascript:alert(1)",
            "file:///tmp/a",
            "/activate",
            "https://user@auth.x.ai/activate",
            "https://user:pass@auth.x.ai/activate",
            "https://",
        ] {
            for field in ["verification_uri", "verification_uri_complete"] {
                let mut invalid = complete.clone();
                invalid[field] = json!(url);
                assert!(
                    parse_device(invalid)
                        .unwrap_err()
                        .contains("verification URI")
                );
            }
        }
        for (wire, expected) in [(json!(0), 5), (json!(1), 1), (json!(30), 30)] {
            let mut value = device_json();
            value["interval"] = wire;
            assert_eq!(parse_device(value).unwrap().interval, expected);
        }
        for field in ["interval", "expires_in"] {
            for malformed in [json!("5"), json!(1.5), json!(-1), json!(true), Value::Null] {
                let mut value = device_json();
                value[field] = malformed;
                assert!(parse_device(value).unwrap_err().contains(field));
            }
        }
        assert_eq!(expiry_display(1), "1 second");
        assert_eq!(expiry_display(30), "30 seconds");
        assert_eq!(expiry_display(60), "1 minute");
        assert_eq!(expiry_display(600), "10 minutes");
        assert_eq!(expiry_display(61), "61 seconds");
    }

    #[test]
    fn initial_tokens_require_credentials_and_unambiguous_identity() {
        let token = parse_token(token_json()).unwrap();
        assert_eq!(token.issued, "subject");
        assert_eq!(token.email.as_deref(), Some("first@example.test"));
        assert!(matches!(token.grant, Grant::Grok));
        for field in ["access_token", "refresh_token", "id_token"] {
            let mut missing = token_json();
            missing.as_object_mut().unwrap().remove(field);
            assert!(parse_token(missing).is_err());
            let mut empty = token_json();
            empty[field] = json!("");
            assert!(parse_token(empty).is_err());
        }
        let mut no_id = token_json();
        no_id.as_object_mut().unwrap().remove("id_token");
        assert_eq!(
            parse_token(no_id).unwrap_err(),
            "xAI's sign-in named no account: its response carried no ID token, and accounts are told apart by it"
        );
        for id_token in [
            "not-a-jwt".to_string(),
            "e30.%%%._".to_string(),
            jwt(&json!({"email": "not-an-identity@example.test"})),
            jwt(&json!({"sub": ""})),
            jwt(&json!({"sub": 7})),
        ] {
            let mut value = token_json();
            value["id_token"] = json!(id_token);
            assert!(parse_token(value).unwrap_err().contains("named no account"));
        }
        let mut no_email = token_json();
        no_email["id_token"] = json!(jwt(&json!({"sub": "subject"})));
        assert_eq!(parse_token(no_email).unwrap().email, None);
    }

    #[test]
    fn token_expiry_defaults_and_rejects_zero_or_overflow() {
        assert_eq!(expires_at(None, 100).unwrap(), 3700);
        assert_eq!(expires_at(Some(42), 100).unwrap(), 142);
        assert!(expires_at(Some(0), 100).is_err());
        assert!(expires_at(Some(u64::MAX), 100).is_err());
        for wire in [
            json!(0),
            json!(u64::MAX),
            json!(-1),
            json!(1.5),
            json!("60"),
        ] {
            let mut value = token_json();
            value["expires_in"] = wire;
            assert!(parse_token(value).is_err());
        }
    }

    #[test]
    fn polling_decisions_are_finite_and_never_reduce_slowdown() {
        let status = StatusCode::BAD_REQUEST;
        assert_eq!(
            poll_interval(status, r#"{"error":"authorization_pending"}"#, 5).unwrap(),
            5
        );
        assert_eq!(
            poll_interval(status, r#"{"error":"slow_down"}"#, 5).unwrap(),
            10
        );
        assert_eq!(
            poll_interval(status, r#"{"error":"slow_down","interval":40}"#, 5).unwrap(),
            40
        );
        assert_eq!(
            poll_interval(status, r#"{"error":"slow_down","interval":2}"#, 5).unwrap(),
            10
        );
        assert_eq!(
            poll_interval(status, r#"{"error":"slow_down","interval":0}"#, 5).unwrap(),
            10
        );
        let repeated = poll_interval(status, r#"{"error":"slow_down"}"#, 10).unwrap();
        assert_eq!(repeated, 15);
        assert_eq!(
            poll_interval(status, r#"{"error":"slow_down"}"#, u64::MAX).unwrap(),
            u64::MAX
        );
        for error in ["access_denied", "authorization_denied"] {
            assert_eq!(
                poll_interval(status, &json!({"error": error}).to_string(), 5).unwrap_err(),
                DENIED
            );
        }
        assert_eq!(
            poll_interval(status, r#"{"error":"expired_token"}"#, 5).unwrap_err(),
            EXPIRED
        );
        for body in [
            "not json",
            "[]",
            "{}",
            r#"{"error":5}"#,
            r#"{"error":"slow_down","interval":"10"}"#,
            r#"{"error":"slow_down","interval":1.5}"#,
            r#"{"error":"authorization_pending","error_description":4}"#,
            r#"{"error":"server_error","error_description":"try later"}"#,
            r#"{"error":"authorization_pending"}"#,
            r#"{"error":"slow_down"}"#,
        ] {
            let error = poll_interval(StatusCode::SERVICE_UNAVAILABLE, body, 5).unwrap_err();
            assert!(error.contains("503"));
            assert!(error.contains(&body_detail(body)));
        }
    }

    #[test]
    fn refresh_retains_omitted_fields_and_pins_identity_and_grant() {
        let previous = parse_token(token_json()).unwrap();
        let retained =
            renewed(&previous, refresh_response(json!({"access_token": "new"}))).unwrap();
        assert_eq!(retained.access_token, "new");
        assert_eq!(retained.refresh_token, previous.refresh_token);
        assert_eq!(retained.email, previous.email);
        assert_eq!(retained.issued, previous.issued);
        assert_eq!(retained.grant, previous.grant);
        let rotated = renewed(
            &previous,
            refresh_response(json!({
                "access_token": "new", "refresh_token": "rotated", "expires_in": 60,
                "id_token": jwt(&json!({"sub": "different", "email": "renamed@example.test"}))
            })),
        )
        .unwrap();
        assert_eq!(rotated.issued, previous.issued);
        assert_eq!(rotated.grant, previous.grant);
        assert_eq!(rotated.email.as_deref(), Some("renamed@example.test"));
        assert_eq!(rotated.refresh_token, "rotated");
        let malformed = renewed(
            &previous,
            refresh_response(json!({
                "access_token": "new", "id_token": "malformed"
            })),
        )
        .unwrap();
        assert_eq!(malformed.email, previous.email);
        for field in ["access_token", "refresh_token", "id_token"] {
            let mut value = json!({"access_token": "new"});
            value[field] = json!("");
            assert!(renewed(&previous, refresh_response(value)).is_err());
        }
        for expiry in [0, u64::MAX] {
            assert!(
                renewed(
                    &previous,
                    refresh_response(json!({
                        "access_token": "new", "expires_in": expiry
                    }))
                )
                .is_err()
            );
        }
    }

    #[test]
    fn refresh_failure_names_grok_and_distinguishes_permanent_errors() {
        let token = parse_token(token_json()).unwrap();
        let permanent = refresh_failure(
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_grant"}"#,
            &token,
        );
        assert!(permanent.contains("xai-grok"));
        assert!(permanent.contains("first@example.test"));
        assert!(permanent.contains("exarch login xai-grok xai-grok:subject"));
        let unauthorized = refresh_failure(StatusCode::UNAUTHORIZED, "", &token);
        assert!(unauthorized.contains("sign in again"));
        let transient = refresh_failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily unavailable",
            &token,
        );
        assert!(transient.contains("503"));
        assert!(transient.contains("temporarily unavailable"));
        assert!(!transient.contains("sign in again"));
    }

    struct Request {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        form: BTreeMap<String, String>,
    }

    struct Fixture {
        url: String,
        requests: mpsc::UnboundedReceiver<Request>,
        count: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn fixture(responses: Vec<(u16, Value)>) -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (send, requests) = mpsc::unbounded_channel();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let task = tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let end = loop {
                    let mut chunk = [0; 1024];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "request ended before its headers");
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let head = std::str::from_utf8(&bytes[..end]).unwrap();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_string();
                let path = first.next().unwrap().to_string();
                let headers: BTreeMap<String, String> = lines
                    .filter_map(|line| {
                        line.split_once(':').map(|(key, value)| {
                            (key.to_ascii_lowercase(), value.trim().to_string())
                        })
                    })
                    .collect();
                let length: usize = headers.get("content-length").unwrap().parse().unwrap();
                while bytes.len() - end < length {
                    let mut chunk = [0; 1024];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "request ended before its form body");
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let body_text = std::str::from_utf8(&bytes[end..end + length]).unwrap();
                let form = Url::parse(&format!("http://fixture/?{body_text}"))
                    .unwrap()
                    .query_pairs()
                    .into_owned()
                    .collect();
                seen.fetch_add(1, Ordering::Relaxed);
                send.send(Request {
                    method,
                    path,
                    headers,
                    form,
                })
                .unwrap();
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        Fixture {
            url,
            requests,
            count,
            task,
        }
    }

    fn assert_form(request: &Request, path: &str, fields: &[(&str, &str)]) {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, path);
        assert_eq!(request.headers.get("accept").unwrap(), "application/json");
        assert_eq!(
            request.headers.get("content-type").unwrap(),
            "application/x-www-form-urlencoded"
        );
        let expected: BTreeMap<String, String> = fields
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        assert_eq!(request.form, expected);
    }

    #[tokio::test]
    async fn device_and_refresh_requests_use_exact_forms_and_paths() {
        let mut fixture = fixture(vec![
            (200, device_json()),
            (200, json!({"access_token": "new"})),
        ])
        .await;
        let client = super::super::http_client().unwrap();
        let device = request_device_code(&client, &format!("{}/oauth2/device/code", fixture.url))
            .await
            .unwrap();
        assert_eq!(device.user_code, "ABCD-EFGH");
        let request = fixture.requests.recv().await.unwrap();
        assert_form(
            &request,
            "/oauth2/device/code",
            &[
                ("client_id", CLIENT_ID),
                ("scope", SCOPE),
                ("referrer", "exarch"),
            ],
        );
        let previous = parse_token(token_json()).unwrap();
        let refreshed = refresh_at(&client, &format!("{}/oauth2/token", fixture.url), &previous)
            .await
            .unwrap();
        assert_eq!(refreshed.refresh_token, previous.refresh_token);
        assert_form(
            &fixture.requests.recv().await.unwrap(),
            "/oauth2/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", "refresh"),
            ],
        );
        (&mut fixture.task).await.unwrap();
    }

    #[tokio::test]
    async fn local_poll_sequence_pending_slowdown_success() {
        let mut fixture = fixture(vec![
            (400, json!({"error": "authorization_pending"})),
            (400, json!({"error": "slow_down", "interval": 1})),
            (200, token_json()),
        ])
        .await;
        let mut device = parse_device(device_json()).unwrap();
        // Real HTTP must not race Tokio's paused-clock auto-advance. Start
        // fixture exchanges immediately; slow_down still imposes a real five
        // seconds. Wire parsing and the initial wait are tested separately.
        device.interval = 0;
        let client = super::super::http_client().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let exchanging = std::cell::Cell::new(0);
        let start = Instant::now();
        let token = poll_for_tokens(
            &client,
            &format!("{}/oauth2/token", fixture.url),
            &device,
            start,
            |phase| {
                assert!(matches!(phase, LoginPhase::ExchangingCode));
                exchanging.set(exchanging.get() + 1);
            },
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(token.issued, "subject");
        assert_eq!(exchanging.get(), 1);
        assert!(start.elapsed() >= Duration::from_secs(5));
        assert_eq!(fixture.count.load(Ordering::Relaxed), 3);
        for _ in 0..3 {
            assert_form(
                &fixture.requests.recv().await.unwrap(),
                "/oauth2/token",
                &[
                    ("grant_type", DEVICE_GRANT),
                    ("client_id", CLIENT_ID),
                    ("device_code", "device-secret"),
                ],
            );
        }
    }

    #[tokio::test]
    async fn local_poll_denial_stops_without_retrying() {
        let fixture = fixture(vec![(400, json!({"error": "access_denied"}))]).await;
        let mut device = parse_device(device_json()).unwrap();
        device.interval = 0;
        let client = super::super::http_client().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let error = poll_for_tokens(
            &client,
            &fixture.url,
            &device,
            Instant::now(),
            |_| panic!("no finalization"),
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(error, DENIED);
        assert_eq!(fixture.count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn local_deadline_and_cancel_prevent_another_request() {
        let pending = fixture(vec![(400, json!({"error": "authorization_pending"}))]).await;
        let mut device = parse_device(device_json()).unwrap();
        device.interval = 1;
        device.expires_in = 2;
        let client = super::super::http_client().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let error = poll_for_tokens(
            &client,
            &pending.url,
            &device,
            Instant::now(),
            |_| panic!("no finalization"),
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(error, EXPIRED);
        assert_eq!(pending.count.load(Ordering::Relaxed), 1);

        let empty = fixture(vec![]).await;
        device.expires_in = 600;
        let start = Instant::now();
        let cancelled = cancel.clone();
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            cancelled.store(true, Ordering::Relaxed);
        });
        let error = poll_for_tokens(
            &client,
            &empty.url,
            &device,
            start,
            |_| panic!("no finalization"),
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(error, "sign-in cancelled");
        assert_eq!(empty.count.load(Ordering::Relaxed), 0);
        cancel_task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn elapsed_device_request_time_and_huge_durations_cannot_extend_lifetime() {
        let fixture = fixture(vec![]).await;
        let mut device = parse_device(device_json()).unwrap();
        let client = super::super::http_client().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        tokio::time::advance(Duration::from_secs(600)).await;
        assert_eq!(
            poll_for_tokens(
                &client,
                &fixture.url,
                &device,
                start,
                |_| panic!("no finalization"),
                &cancel
            )
            .await
            .unwrap_err(),
            EXPIRED
        );
        device.interval = u64::MAX;
        device.expires_in = 1;
        assert_eq!(
            poll_for_tokens(
                &client,
                &fixture.url,
                &device,
                Instant::now(),
                |_| panic!("no finalization"),
                &cancel
            )
            .await
            .unwrap_err(),
            EXPIRED
        );
        assert_eq!(fixture.count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn initial_wait_is_clamped_to_lifetime_and_prevents_a_late_first_request() {
        let fixture = fixture(vec![]).await;
        let client = super::super::http_client().unwrap();
        let mut device = parse_device(device_json()).unwrap();
        device.expires_in = 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let error = poll_for_tokens(
            &client,
            &fixture.url,
            &device,
            start,
            |_| panic!("no finalization"),
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(error, EXPIRED);
        assert!(start.elapsed() >= Duration::from_secs(1));
        assert_eq!(fixture.count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn cancellation_after_a_pending_reply_prevents_another_poll() {
        let mut fixture = fixture(vec![(400, json!({"error": "authorization_pending"}))]).await;
        let client = super::super::http_client().unwrap();
        let mut device = parse_device(device_json()).unwrap();
        device.interval = 0;
        let cancel = Arc::new(AtomicBool::new(false));
        let url = fixture.url.clone();
        let polling = poll_for_tokens(
            &client,
            &url,
            &device,
            Instant::now(),
            |_| panic!("no finalization"),
            &cancel,
        );
        let cancelling = async {
            fixture.requests.recv().await.unwrap();
            cancel.store(true, Ordering::Relaxed);
        };
        let (result, ()) = tokio::join!(polling, cancelling);
        assert_eq!(result.unwrap_err(), "sign-in cancelled");
        assert_eq!(fixture.count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cancellation_during_finalization_does_not_return_a_credential() {
        let fixture = fixture(vec![(200, token_json())]).await;
        let client = super::super::http_client().unwrap();
        let mut device = parse_device(device_json()).unwrap();
        device.interval = 0;
        let cancel = Arc::new(AtomicBool::new(false));
        let result = poll_for_tokens(
            &client,
            &fixture.url,
            &device,
            Instant::now(),
            |_| cancel.store(true, Ordering::Relaxed),
            &cancel,
        )
        .await;
        assert_eq!(result.unwrap_err(), "sign-in cancelled");
        assert_eq!(fixture.count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn authorization_deadline_also_bounds_reading_a_stalled_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (send, mut received) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .await
                .unwrap();
            send.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let client = super::super::http_client().unwrap();
        let mut device = parse_device(device_json()).unwrap();
        device.interval = 0;
        device.expires_in = 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let result = poll_for_tokens(
            &client,
            &url,
            &device,
            Instant::now(),
            |_| panic!("no finalization"),
            &cancel,
        )
        .await;
        assert_eq!(result.unwrap_err(), EXPIRED);
        received.recv().await.unwrap();
        server.abort();
    }
}
