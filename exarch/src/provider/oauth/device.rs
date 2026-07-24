//! Device-code login: the user opens a verification page and types a code
//! while exarch polls the token endpoint. The poll response carries the PKCE
//! verifier used in the final authorization-code exchange.

use super::{CLIENT_ID, ISSUER, LoginPhase};
use serde::Deserialize;
use serde::Deserializer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::time::Instant;

/// The device-code poll deadline; the browser flow's own callback wait
/// mirrors this value. Enforced client-side — the device-code response
/// carries no `expires_in` of its own.
const MAX_WAIT: Duration = Duration::from_mins(15);

/// [`MAX_WAIT`] rendered for the user-facing sign-in messages, so the shown
/// duration cannot drift from the constant. `pub(super)`: the CLI adapter's
/// [`LoginPhase::stderr_line`] reproduces this exact text.
pub(super) fn max_wait_label() -> String {
    format!("{} minutes", MAX_WAIT.as_secs() / 60)
}

#[derive(Deserialize)]
// The `user_code` field repeats the struct name because that is the wire
// field name, hence the lint allow — not the serde aliases below.
#[allow(clippy::struct_field_names)]
struct UserCode {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default = "default_interval", deserialize_with = "interval_secs")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// Accept the polling interval whether the server sends it as a number or a
/// string.
fn interval_secs<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Interval {
        Num(u64),
        Str(String),
    }
    match Interval::deserialize(deserializer)? {
        Interval::Num(n) => Ok(n),
        Interval::Str(s) => s.trim().parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Deserialize)]
struct PollSuccess {
    authorization_code: String,
    code_verifier: String,
}

/// Drive the device-code flow to completion and return the issued tokens.
pub(super) async fn run(
    client: &reqwest::Client,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<super::RawTokens, String> {
    let code = request_user_code(client).await?;

    on_phase(LoginPhase::AwaitingDevice {
        user_code: code.user_code.clone(),
        url: format!("{ISSUER}/codex/device"),
        expires_in: max_wait_label(),
    });

    let poll = poll_for_code(client, &code, cancel).await?;
    on_phase(LoginPhase::ExchangingCode);
    let redirect_uri = format!("{ISSUER}/deviceauth/callback");
    super::exchange_code(
        client,
        &redirect_uri,
        &poll.authorization_code,
        &poll.code_verifier,
    )
    .await
}

/// Request a one-time user code and the device auth id that identifies it.
async fn request_user_code(client: &reqwest::Client) -> Result<UserCode, String> {
    let resp = client
        .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;
    super::json_or_error(resp, "device code request").await
}

/// Poll the token endpoint until the user authorises the code or [`MAX_WAIT`]
/// elapses. A 403 or 404 means the user has not finished yet. `cancel` is
/// checked before each request; an in-flight request remains bounded by the
/// HTTP client's own timeout.
async fn poll_for_code(
    client: &reqwest::Client,
    code: &UserCode,
    cancel: &Arc<AtomicBool>,
) -> Result<PollSuccess, String> {
    let url = format!("{ISSUER}/api/accounts/deviceauth/token");
    let start = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("sign-in cancelled".to_string());
        }
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "device_auth_id": code.device_auth_id,
                "user_code": code.user_code,
            }))
            .send()
            .await
            .map_err(|e| format!("device token poll failed: {e}"))?;

        // A 403/404 means the user has not finished; keep polling until the
        // deadline.  Any other outcome — success or a hard failure — is the
        // shared status/decode handling's to resolve.
        if matches!(resp.status().as_u16(), 403 | 404) {
            if start.elapsed() >= MAX_WAIT {
                return Err(format!(
                    "device sign-in timed out after {}",
                    max_wait_label()
                ));
            }
            tokio::time::sleep(Duration::from_secs(code.interval)).await;
            continue;
        }

        return super::json_or_error(resp, "device token poll").await;
    }
}
