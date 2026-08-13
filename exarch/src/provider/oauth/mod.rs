//! "Sign in with ChatGPT": `ChatGPT`-plan accounts authorised through
//! `OpenAI`'s OAuth issuer rather than an API key.
//!
//! Both flows — a browser redirect to a loopback listener ([`browser`]), a
//! device code typed into a verification page ([`device`]) — end in the same
//! authorization-code exchange, yielding a JWT `access_token` whose `exp` is
//! the expiry and an `id_token` carrying the account id and login email.
//! Several accounts coexist in one store keyed by account id, each a
//! selectable [`crate::provider::ProviderId::ChatGpt`]; [`refresh`] upserts,
//! so renewing one never disturbs the others.

mod browser;
mod device;

use crate::sync::LockExt;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const ORIGINATOR: &str = "codex_cli_rs";
pub(crate) const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// The Codex CLI version exarch presents as `client_version` and in the
/// `codex_cli_rs/<v>` user-agent. It must be a real, current Codex release,
/// **not** exarch's own `CARGO_PKG_VERSION`: each model carries a
/// `minimal_client_version`, so a low version is served an *empty* model list.
const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.144.3";

/// [`DEFAULT_CODEX_CLIENT_VERSION`], or a non-blank
/// `EXARCH_CODEX_CLIENT_VERSION` override — the valve for when the backend
/// raises its floor before the pinned default is bumped.
pub(crate) fn codex_client_version() -> String {
    std::env::var("EXARCH_CODEX_CLIENT_VERSION")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_CODEX_CLIENT_VERSION.to_string())
}

const ISSUER: &str = "https://auth.openai.com";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// A persisted `ChatGPT` login: a short-lived JWT access token and the refresh
/// token that mints its successors. One per signed-in account, keyed by
/// [`Self::account_id`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    /// From the `id_token`'s `email` claim; `None` when it carried none, and
    /// then [`Self::account_id`] stands in as the label.
    pub email: Option<String>,
    /// Unix seconds at which `access_token` expires (its JWT `exp`).
    pub expires_at: u64,
}

impl OAuthToken {
    /// True when the access token has expired or is within 60s of expiring.
    pub fn is_stale(&self) -> bool {
        self.expires_at <= crate::bootstrap::now_secs() + 60
    }

    /// The account's handle: its login email, or the opaque account id when
    /// none was issued. `ChatGptAccount` carries it as the `ProviderId` label,
    /// so the picker, the persisted selection, and `logout` all match on it.
    pub fn label(&self) -> String {
        self.email
            .clone()
            .unwrap_or_else(|| self.account_id.clone())
    }
}

/// The token-endpoint success body shared by both interactive flows.
#[derive(Deserialize)]
// The `_token` suffix is the token-endpoint wire format; renaming would break serde.
#[allow(clippy::struct_field_names)]
pub(super) struct RawTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

/// Which interactive flow to run. Mirrors the CLI's `--device-auth` toggle.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LoginMethod {
    Browser,
    Device,
}

/// One staged report from a running login flow.
///
/// The CLI adapter ([`Self::stderr_line`]) and the TUI's `/login` overlay both
/// render exactly these three phases; there is no percentage or elapsed clock.
pub enum LoginPhase {
    /// `url` is offered whatever the launcher did with it: a launch that
    /// reports success still shows nothing when the browser it started lives
    /// on a machine the user is not sitting at.
    AwaitingBrowser {
        url: String,
    },
    AwaitingDevice {
        user_code: String,
        url: String,
        expires_in: String,
    },
    ExchangingCode,
}

impl LoginPhase {
    /// The `exarch login` stderr line, `None` for `ExchangingCode`.
    pub fn stderr_line(&self) -> Option<String> {
        match self {
            Self::AwaitingBrowser { url } => Some(format!(
                "Open this URL in your browser to sign in:\n  {url}\nWaiting for sign-in to complete..."
            )),
            Self::AwaitingDevice {
                user_code,
                url,
                expires_in,
            } => Some(format!(
                "To sign in, open {url} and enter this code (expires in {expires_in}):\n  {user_code}"
            )),
            Self::ExchangingCode => None,
        }
    }
}

/// Drive one interactive login to a persisted token, reporting whether an
/// existing account was replaced.
///
/// Blocking: builds its own current-thread runtime. The flows' wait loops poll
/// `cancel`, so tripping it (on Esc, say) frees the loopback port promptly.
///
/// # Errors
/// Returns `Err` if the runtime or HTTP client cannot be built, if the flow
/// fails or is cancelled, or if finalising or persisting the token fails.
pub fn login_flow(
    method: LoginMethod,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<(OAuthToken, bool), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start runtime: {e}"))?;
    let client = http_client()?;
    let raw = rt.block_on(async move {
        match method {
            LoginMethod::Device => device::run(&client, on_phase, cancel).await,
            LoginMethod::Browser => browser::run(&client, on_phase, cancel).await,
        }
    })?;
    // The exchange itself does not poll the flag, so a cancel landing after
    // the wait loops returned `Ok` would otherwise still persist a token.
    if cancel.load(Ordering::Relaxed) {
        return Err("sign-in cancelled".to_string());
    }
    let token = finalize(raw)?;
    let replaced = save_one(&token)?;
    Ok((token, replaced))
}

/// The `exarch login` command: [`login_flow`] with its phases rendered to
/// stderr, and no cancellation — a CLI run has no overlay to Esc out of, and
/// Ctrl-C kills the process.
///
/// # Errors
/// Returns `Err` if the runtime or HTTP client cannot be built, if the flow
/// fails, or if finalising or persisting the token fails.
pub fn login(device: bool) -> Result<(), String> {
    let method = if device {
        LoginMethod::Device
    } else {
        LoginMethod::Browser
    };
    let (token, replaced) = login_flow(
        method,
        |phase| {
            if let Some(line) = phase.stderr_line() {
                eprintln!("{line}");
            }
        },
        &Arc::new(AtomicBool::new(false)),
    )?;
    let verb = if replaced {
        "Updated the login for"
    } else {
        "Signed in to"
    };
    eprintln!("{verb} ChatGPT account {}.", token.label());
    Ok(())
}

/// Remove one signed-in account (by label or account id), or every account
/// when `all`.
///
/// Named neither, it drops the sole account and otherwise errors asking which,
/// so a stray `logout` cannot silently take the wrong one.
///
/// # Errors
/// Returns `Err` if rewriting the token store fails, if several accounts are
/// signed in but none is named, or if the named account matches nothing.
pub fn logout(account: Option<String>, all: bool) -> Result<(), String> {
    if all {
        clear_at(&token_path())?;
        eprintln!("Logged out of every ChatGPT account.");
        return Ok(());
    }
    let accounts = load_all();
    let target = match (account, accounts.as_slice()) {
        (Some(name), _) => name,
        (None, []) => {
            eprintln!("No ChatGPT account to log out of.");
            return Ok(());
        }
        (None, [only]) => only.account_id.clone(),
        (None, many) => {
            return Err(format!(
                "multiple ChatGPT accounts signed in ({}); name one to log out, \
                 or pass --all",
                labels(many),
            ));
        }
    };
    match remove(&target)? {
        Some(label) => {
            eprintln!("Logged out of ChatGPT account {label}.");
            Ok(())
        }
        None => Err(format!(
            "no ChatGPT account matches '{target}' (signed in: {})",
            labels(&accounts),
        )),
    }
}

/// Every signed-in account's label, comma-joined for the messages that ask
/// which one was meant.
fn labels(accounts: &[OAuthToken]) -> String {
    accounts
        .iter()
        .map(OAuthToken::label)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every persisted login, in stored order. An absent or corrupt store reads
/// as no accounts rather than an error.
pub fn load_all() -> Vec<OAuthToken> {
    load_all_at(&token_path())
}

/// Upsert `token` by account id, reporting whether an existing login was
/// replaced. `pub` so the `credential_env` integration test seeds logins
/// through the same door the flows use.
///
/// # Errors
/// Returns `Err` when the token store cannot be written.
pub fn save_one(token: &OAuthToken) -> Result<bool, String> {
    save_one_at(&token_path(), token)
}

/// Remove the login matched by `account` (label or account id), returning its
/// label, or `None` when nothing matched.
pub(crate) fn remove(account: &str) -> Result<Option<String>, String> {
    remove_at(&token_path(), account)
}

// The `*_at` core takes the path as an argument so tests drive it against a
// temp file without mutating the process environment.

/// Serializes `save_one_at` and `remove_at`'s load-modify-write: two
/// concurrent refreshes, one per stale account, would otherwise each write
/// back a stale copy of the *other*, silently reverting it. `load_all_at`
/// stays lock-free — a bare read is never part of that race.
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-read] reads the persisted OAuth tokens; credential store infra, not turn-time data I/O"
)]
fn load_all_at(path: &std::path::Path) -> Vec<OAuthToken> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_one_at(path: &std::path::Path, token: &OAuthToken) -> Result<bool, String> {
    let _guard = STORE_LOCK.lock_ignore_poison();
    let mut all = load_all_at(path);
    let replaced = if let Some(existing) = all.iter_mut().find(|t| t.account_id == token.account_id)
    {
        *existing = token.clone();
        true
    } else {
        all.push(token.clone());
        false
    };
    write_all_at(path, &all)?;
    Ok(replaced)
}

fn remove_at(path: &std::path::Path, account: &str) -> Result<Option<String>, String> {
    let _guard = STORE_LOCK.lock_ignore_poison();
    let mut all = load_all_at(path);
    let Some(pos) = all
        .iter()
        .position(|t| t.account_id == account || t.label() == account)
    else {
        return Ok(None);
    };
    let removed = all.remove(pos);
    // A fully logged-out machine carries no store at all, not an empty one.
    if all.is_empty() {
        clear_at(path)?;
    } else {
        write_all_at(path, &all)?;
    }
    Ok(Some(removed.label()))
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-dir] creates the OAuth token store dir; credential store infra, not turn-time data I/O"
)]
fn write_all_at(path: &std::path::Path, all: &[OAuthToken]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(all)
        .map_err(|e| format!("could not serialize tokens: {e}"))?;
    write_private(path, json.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Delete the store; an absent one is not an error.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-remove] deletes the stored OAuth tokens; credential store infra, not turn-time data I/O"
)]
fn clear_at(path: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not remove {}: {e}", path.display())),
    }
}

pub(crate) async fn refresh(current: &OAuthToken) -> Result<OAuthToken, String> {
    #[derive(Deserialize)]
    // The `_token` suffix is the token-endpoint wire format; renaming would break serde.
    #[allow(clippy::struct_field_names)]
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
    let resp: RefreshResponse = json_or_error(resp, "token refresh").await?;

    let access_token = resp
        .access_token
        .ok_or_else(|| "token refresh did not return an access token".to_string())?;
    let refresh_token = resp
        .refresh_token
        .unwrap_or_else(|| current.refresh_token.clone());
    let id_token = resp.id_token;
    let expires_at = expiry_secs(&access_token, id_token.as_deref());
    // The refresh response may omit the id_token; keeping the existing
    // identity then is what stops a refresh from losing the account's label.
    let account_id = id_token
        .as_deref()
        .and_then(account_id_from_jwt)
        .unwrap_or_else(|| current.account_id.clone());
    let email = id_token
        .as_deref()
        .and_then(email_from_jwt)
        .or_else(|| current.email.clone());

    Ok(OAuthToken {
        access_token,
        refresh_token,
        account_id,
        email,
        expires_at,
    })
}

/// Renew the token in a shared credential cell when it is near expiry.
///
/// Both inference (`transport`) and the model catalog (`models`) enter here,
/// so neither can authenticate with a stale token merely by going first.
/// Persistence is best-effort: a fresh in-memory token still serves the
/// session when the state directory cannot be written.
pub(crate) async fn refresh_cell_if_stale(
    cell: &std::sync::Arc<std::sync::Mutex<OAuthToken>>,
) -> Result<(), String> {
    let current = {
        let token = cell.lock_ignore_poison();
        if !token.is_stale() {
            return Ok(());
        }
        token.clone()
    };
    let fresh = refresh(&current).await?;
    let _ = save_one(&fresh);
    *cell.lock_ignore_poison() = fresh;
    Ok(())
}

/// The headers a Codex-backend request carries for this token, as lowercase
/// `(name, value)` pairs.
pub(crate) fn request_headers(token: &OAuthToken, accept: &str) -> Vec<(String, String)> {
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
            format!("codex_cli_rs/{}", codex_client_version()),
        ),
        ("accept".into(), accept.into()),
    ]
}

fn token_endpoint() -> String {
    format!("{ISSUER}/oauth/token")
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .use_preconfigured_tls(crate::provider::tls::config())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("could not build HTTP client: {e}"))
}

/// Decode a token-endpoint response, turning a non-2xx status into an `Err`
/// carrying the body text. `action` names the request in both messages; every
/// login flow and [`refresh`] report through here.
pub(super) async fn json_or_error<T: DeserializeOwned>(
    resp: reqwest::Response,
    action: &str,
) -> Result<T, String> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("{action} failed ({status}): {body}"));
    }
    resp.json()
        .await
        .map_err(|e| format!("could not parse {action} response: {e}"))
}

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
    json_or_error(resp, "token exchange").await
}

/// A PKCE verifier and its S256 challenge.
pub(super) fn pkce() -> (String, String) {
    let verifier = random_b64url(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub(super) fn random_b64url(n_bytes: usize) -> String {
    let mut bytes = vec![0u8; n_bytes];
    getrandom::fill(&mut bytes).expect("OS randomness");
    URL_SAFE_NO_PAD.encode(bytes)
}

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
    /// The standard OIDC claim; present only because [`SCOPE`] asks for it.
    email: Option<String>,
}

#[derive(Deserialize)]
struct ExpClaims {
    exp: Option<i64>,
}

fn account_id_from_jwt(jwt: &str) -> Option<String> {
    jwt_payload::<IdClaims>(jwt).ok()?.auth?.chatgpt_account_id
}

fn email_from_jwt(jwt: &str) -> Option<String> {
    jwt_payload::<IdClaims>(jwt).ok()?.email
}

fn jwt_exp(jwt: &str) -> Option<u64> {
    let exp = jwt_payload::<ExpClaims>(jwt).ok()?.exp?;
    u64::try_from(exp).ok()
}

/// The access token's own `exp`, else the `id_token`'s, else an hour out.
fn expiry_secs(access_token: &str, id_token: Option<&str>) -> u64 {
    jwt_exp(access_token)
        .or_else(|| id_token.and_then(jwt_exp))
        .unwrap_or_else(|| crate::bootstrap::now_secs() + 3600)
}

/// Turn a token-endpoint response into an [`OAuthToken`]. A login without an
/// account id is rejected outright: nothing downstream can key on it.
fn finalize(raw: RawTokens) -> Result<OAuthToken, String> {
    let account_id = account_id_from_jwt(&raw.id_token)
        .ok_or_else(|| "login did not return a ChatGPT account id".to_string())?;
    let email = email_from_jwt(&raw.id_token);
    let expires_at = expiry_secs(&raw.access_token, Some(&raw.id_token));
    Ok(OAuthToken {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        account_id,
        email,
        expires_at,
    })
}

use crate::provider::secret_file::write_private;

fn token_path() -> PathBuf {
    crate::bootstrap::EXARCH
        .xdg_dir(ral_core::path::basedir::XdgKind::State)
        .join("oauth.json")
}
