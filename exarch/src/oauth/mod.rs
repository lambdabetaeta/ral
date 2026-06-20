//! "Sign in with ChatGPT" for the OpenAI provider.
//!
//! A ChatGPT plan subscription is authorised through OpenAI's OAuth issuer
//! rather than an API key. Two interactive flows obtain the initial token:
//! a browser redirect to a loopback listener ([`browser`]), and a device
//! code typed into a verification page ([`device`]). Both end in an
//! authorization-code exchange against the token endpoint, yielding an
//! id_token, an access_token, and a refresh_token. The access_token is a
//! JWT whose `exp` claim sets the expiry; the id_token carries the ChatGPT
//! account id. The resulting [`OAuthToken`] is persisted to the XDG state
//! directory and reloaded on later runs; the provider refreshes it through
//! [`refresh`] when it is near expiry.

mod browser;
mod device;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const ORIGINATOR: &str = "codex_cli_rs";
pub(crate) const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// The models the ChatGPT plan serves through the Codex backend, newest
/// first. The plan exposes no catalog endpoint, so this curated list is
/// the picker's source for a subscription provider's models — the
/// API-key path lists live via genai instead.
pub(crate) const PLAN_MODELS: &[&str] = &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"];

/// How a provider's plan reads, for both metering and labelling. A turn
/// under either subscription flavour is unmetered; the flavour only changes
/// the decoration [`provider_label`] applies.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Subscription {
    /// A metered API key — turns are billed per token.
    Metered,
    /// A ChatGPT plan login authorised over OAuth.
    ChatGpt,
    /// A flat-rate plan declared by the provider's [`crate::provider::ProviderId`]
    /// (opencode Go's $10/mo gateway).
    FlatRate,
}

/// A provider's status/picker label: the subscription-decorated form when it
/// is on a plan, else the bare provider name. The single place the "decorate
/// when subscription" decision is made — and the only place the per-flavour
/// suffix is spelled — so the status bar, the `/model` switch, and the picker
/// rows cannot drift.
pub(crate) fn provider_label(subscription: Subscription, base: &str) -> String {
    match subscription {
        Subscription::Metered => base.to_string(),
        Subscription::ChatGpt => format!("{base} (ChatGPT subscription)"),
        Subscription::FlatRate => format!("{base} (subscription)"),
    }
}

const ISSUER: &str = "https://auth.openai.com";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// A persisted ChatGPT OAuth token. The access token is a short-lived JWT;
/// the refresh token mints fresh access tokens once it expires.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    /// Unix seconds at which `access_token` expires (its JWT `exp`).
    pub expires_at: u64,
}

impl OAuthToken {
    /// True when the access token has expired or is within 60s of expiring.
    pub fn is_stale(&self) -> bool {
        self.expires_at <= crate::bootstrap::now_secs() + 60
    }
}

/// The token-endpoint success body shared by both interactive flows.
#[derive(Deserialize)]
pub(super) struct RawTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

/// Run interactive login (browser flow, or device code when `device`), then
/// persist the token. Progress is printed to stderr.
pub fn login(device: bool) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start runtime: {e}"))?;
    let client = http_client()?;
    let raw = rt.block_on(async {
        if device {
            device::run(&client).await
        } else {
            browser::run(&client).await
        }
    })?;
    let token = finalize(raw)?;
    save(&token)?;
    eprintln!("Signed in to ChatGPT (account {}).", token.account_id);
    Ok(())
}

/// Delete the persisted token. Succeeds when no token is stored.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-remove] deletes the stored OAuth token; credential store infra, not turn-time data I/O"
)]
pub fn logout() -> Result<(), String> {
    let path = token_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not remove {}: {e}", path.display())),
    }
}

/// Load the persisted token, or `None` if absent or unparseable.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-read] reads the persisted OAuth token; credential store infra, not turn-time data I/O"
)]
pub fn load() -> Option<OAuthToken> {
    let bytes = std::fs::read(token_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist a token. On Unix the file is created with mode 0600.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-dir] creates the OAuth token store dir; credential store infra, not turn-time data I/O"
)]
pub(crate) fn save(token: &OAuthToken) -> Result<(), String> {
    let path = token_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(token)
        .map_err(|e| format!("could not serialize token: {e}"))?;
    write_private(&path, json.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Exchange the refresh token for a fresh [`OAuthToken`].
pub(crate) async fn refresh(current: &OAuthToken) -> Result<OAuthToken, String> {
    #[derive(Deserialize)]
    struct RefreshResponse {
        id_token: Option<String>,
        access_token: Option<String>,
        refresh_token: Option<String>,
    }

    let client = http_client()?;
    let resp = client
        .post(token_endpoint())
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": current.refresh_token,
        }))
        .send()
        .await
        .map_err(|e| format!("token refresh request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token refresh failed ({status}): {body}"));
    }
    let resp: RefreshResponse = resp
        .json()
        .await
        .map_err(|e| format!("could not parse token refresh response: {e}"))?;

    let access_token = resp
        .access_token
        .ok_or_else(|| "token refresh did not return an access token".to_string())?;
    let refresh_token = resp
        .refresh_token
        .unwrap_or_else(|| current.refresh_token.clone());
    let expires_at = expiry_secs(&access_token, resp.id_token.as_deref());
    let account_id = resp
        .id_token
        .and_then(|jwt| account_id_from_jwt(&jwt))
        .unwrap_or_else(|| current.account_id.clone());

    Ok(OAuthToken {
        access_token,
        refresh_token,
        account_id,
        expires_at,
    })
}

/// The headers a Codex-backend model request carries for this token,
/// returned as lowercase `(name, value)` pairs.
pub(crate) fn request_headers(token: &OAuthToken) -> Vec<(String, String)> {
    vec![
        (
            "authorization".into(),
            format!("Bearer {}", token.access_token),
        ),
        ("chatgpt-account-id".into(), token.account_id.clone()),
        ("openai-beta".into(), "responses=experimental".into()),
        ("originator".into(), ORIGINATOR.into()),
        (
            "user-agent".into(),
            format!("codex_cli_rs/{}", env!("CARGO_PKG_VERSION")),
        ),
        ("accept".into(), "text/event-stream".into()),
    ]
}

fn token_endpoint() -> String {
    format!("{ISSUER}/oauth/token")
}

/// The HTTP client shared by the login flows and refresh.
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::config())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("could not build HTTP client: {e}"))
}

/// Exchange an authorization code for tokens at the token endpoint.
pub(super) async fn exchange_code(
    client: &reqwest::Client,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<RawTokens, String> {
    let resp = client
        .post(token_endpoint())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|e| format!("token exchange request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token exchange failed ({status}): {body}"));
    }
    resp.json()
        .await
        .map_err(|e| format!("could not parse token exchange response: {e}"))
}

/// A PKCE verifier/challenge pair. The verifier is 64 random bytes encoded
/// as URL-safe base64; the challenge is the URL-safe base64 of its SHA-256.
pub(super) fn pkce() -> (String, String) {
    let verifier = random_b64url(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// `n_bytes` of OS randomness encoded as URL-safe base64 without padding.
pub(super) fn random_b64url(n_bytes: usize) -> String {
    let mut bytes = vec![0u8; n_bytes];
    getrandom::getrandom(&mut bytes).expect("OS randomness");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode and deserialize a JWT payload (the second dot-separated segment).
fn jwt_payload<T: DeserializeOwned>(jwt: &str) -> Result<T, String> {
    let payload = jwt
        .split('.')
        .nth(1)
        .ok_or_else(|| "malformed JWT".to_string())?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| format!("could not decode JWT payload: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("could not parse JWT payload: {e}"))
}

#[derive(Deserialize)]
struct AuthClaims {
    chatgpt_account_id: Option<String>,
}

#[derive(Deserialize)]
struct IdClaims {
    #[serde(rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
}

#[derive(Deserialize)]
struct ExpClaims {
    exp: Option<i64>,
}

/// The ChatGPT account id carried by an id_token's auth claim.
fn account_id_from_jwt(jwt: &str) -> Option<String> {
    jwt_payload::<IdClaims>(jwt).ok()?.auth?.chatgpt_account_id
}

/// The `exp` claim of a JWT, as unix seconds.
fn jwt_exp(jwt: &str) -> Option<u64> {
    let exp = jwt_payload::<ExpClaims>(jwt).ok()?.exp?;
    u64::try_from(exp).ok()
}

/// When the access token expires, as unix seconds: its own JWT `exp`, then the
/// id_token's, then one hour out when neither carries one.
fn expiry_secs(access_token: &str, id_token: Option<&str>) -> u64 {
    jwt_exp(access_token)
        .or_else(|| id_token.and_then(jwt_exp))
        .unwrap_or_else(|| crate::bootstrap::now_secs() + 3600)
}

/// Turn a token-endpoint response into a persisted [`OAuthToken`]. The
/// account id comes from the id_token; the expiry is the access token's
/// `exp`, falling back to the id_token's `exp`, then to one hour out.
fn finalize(raw: RawTokens) -> Result<OAuthToken, String> {
    let account_id = account_id_from_jwt(&raw.id_token)
        .ok_or_else(|| "login did not return a ChatGPT account id".to_string())?;
    let expires_at = expiry_secs(&raw.access_token, Some(&raw.id_token));
    Ok(OAuthToken {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        account_id,
        expires_at,
    })
}

fn token_path() -> PathBuf {
    crate::bootstrap::xdg_app_dir(ral_core::path::basedir::XdgKind::State).join("oauth.json")
}

#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-write] opens the OAuth token file 0600 for write; credential store infra, not turn-time data I/O"
)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-write-nonunix] oauth token store write on non-unix (no owner-only mode to set); credential persistence infrastructure, not turn-time model data I/O."
)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_length_and_challenge() {
        let (verifier, challenge) = pkce();
        assert!((43..=128).contains(&verifier.len()));
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
    }

    #[test]
    fn jwt_claims_extraction() {
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"exp":1893456000,"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123"}}"#,
        );
        let jwt = format!("hdr.{payload}.sig");
        assert_eq!(account_id_from_jwt(&jwt).as_deref(), Some("acc_123"));
        assert_eq!(jwt_exp(&jwt), Some(1893456000));
    }

    /// The single source of truth for the subscription decoration: a metered
    /// provider keeps its bare name, a ChatGPT login carries the OpenAI plan
    /// suffix, and a flat-rate plan the generic one — so the status bar, the
    /// `/model` switch, and the picker cannot drift across flavours.
    #[test]
    fn provider_label_decorates_per_flavour() {
        assert_eq!(provider_label(Subscription::Metered, "deepseek"), "deepseek");
        assert_eq!(
            provider_label(Subscription::ChatGpt, "openai"),
            "openai (ChatGPT subscription)"
        );
        assert_eq!(
            provider_label(Subscription::FlatRate, "opencode-go"),
            "opencode-go (subscription)"
        );
    }
}
