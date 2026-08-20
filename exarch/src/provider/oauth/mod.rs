//! "Sign in with ChatGPT": `ChatGPT`-plan accounts authorised through
//! `OpenAI`'s OAuth issuer rather than an API key.
//!
//! Both flows — a browser redirect to a loopback listener ([`browser`]), a
//! device code typed into a verification page ([`device`]) — end in the same
//! authorization-code exchange, yielding a JWT `access_token` whose `exp` is
//! the expiry and an `id_token` carrying the issued account id, the login
//! email, and (when the claims name one) a plan type or workspace. Several
//! logins coexist in one store keyed by [`identity::AccountId`], each a
//! selectable [`Account`]; [`refresh`] upserts, so renewing one never
//! disturbs the others.

mod browser;
mod device;

use crate::provider::identity::{self, Account, AccountId};
use crate::provider::secret_file::write_private;
use crate::sync::LockExt;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
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
/// token that mints its successors.
///
/// One per signed-in account, keyed by [`Self::issued`] under the `chatgpt`
/// service — see [`to_account`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: String,
    /// The id the issuer minted for this account: `AccountId::of_login`'s
    /// second half.
    pub issued: String,
    /// From the `id_token`'s `email` claim; `None` when it carried none, and
    /// then [`Self::issued`] stands in for [`Self::handle`].
    pub email: Option<String>,
    /// The workspace/organisation title, when the token names one — the
    /// handle's first-choice qualifier.
    pub workspace: Option<String>,
    /// The plan type ("plus", "pro", "team", ...) — the handle's qualifier of
    /// last resort.
    pub plan: Option<String>,
    /// Unix seconds at which `access_token` expires (its JWT `exp`).
    pub expires_at: u64,
}

impl OAuthToken {
    /// True when the access token has expired or is within 60s of expiring.
    pub fn is_stale(&self) -> bool {
        self.expires_at <= crate::bootstrap::now_secs() + 60
    }

    /// This account's local handle: its login email, or the issued id when
    /// none was given, qualified by a workspace or plan when the token names
    /// one. Computed from this token alone — see [`identity::label`] for how
    /// a whole set of accounts is then told apart.
    pub fn handle(&self) -> String {
        let handle = self.email.clone().unwrap_or_else(|| self.issued.clone());
        match self.workspace.clone().or_else(|| self.plan.clone()) {
            Some(qualifier) => format!("{handle} ({qualifier})"),
            None => handle,
        }
    }
}

/// The `chatgpt` [`Account`] a persisted login names — the one conversion
/// every other reader of the token store goes through.
pub(super) fn to_account(token: &OAuthToken) -> Account {
    let service = identity::chatgpt_service();
    Account {
        id: AccountId::of_login(&service.name, &token.issued),
        handle: token.handle(),
        service,
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
    let accounts = accounts();
    let named = identity::label(&to_account(&token), &accounts);
    eprintln!("{verb} ChatGPT account {named}.");
    Ok(())
}

/// Remove one signed-in account (by handle, issued id, or account id), or
/// every account when `all`.
///
/// Named neither, it drops the sole account and otherwise errors asking which,
/// so a stray `logout` cannot silently take the wrong one.
///
/// # Errors
/// Returns `Err` if rewriting the token store fails, if several accounts are
/// signed in but none is named, if the name matches several accounts, or if
/// it matches none.
pub fn logout(account: Option<String>, all: bool) -> Result<(), String> {
    if all {
        clear_at(&token_path())?;
        eprintln!("Logged out of every ChatGPT account.");
        return Ok(());
    }
    let tokens = load_all();
    let target = match (account, tokens.as_slice()) {
        (Some(name), _) => name,
        (None, []) => {
            eprintln!("No ChatGPT account to log out of.");
            return Ok(());
        }
        (None, [only]) => only.issued.clone(),
        (None, _many) => {
            return Err(format!(
                "multiple ChatGPT accounts signed in ({}); name one to log out, \
                 or pass --all",
                joined_labels(&tokens),
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
            joined_labels(&tokens),
        )),
    }
}

/// Every signed-in token's disambiguated label, comma-joined for the messages
/// that ask which one was meant.
fn joined_labels(tokens: &[OAuthToken]) -> String {
    identity::roster(&tokens.iter().map(to_account).collect::<Vec<_>>())
}

/// Every persisted login, in stored order. An absent or corrupt store reads
/// as no accounts rather than an error.
pub fn load_all() -> Vec<OAuthToken> {
    load_all_at(&token_path())
}

/// Every persisted login, as the accounts the rest of exarch selects among.
pub fn accounts() -> Vec<Account> {
    load_all().iter().map(to_account).collect()
}

/// Upsert `token` by issued id, reporting whether an existing login was
/// replaced. `pub` so the `credential_env` integration test seeds logins
/// through the same door the flows use.
///
/// # Errors
/// Returns `Err` when the token store cannot be written.
pub fn save_one(token: &OAuthToken) -> Result<bool, String> {
    save_one_at(&token_path(), token)
}

/// Remove the login matched by `account` (handle, issued id, or account id),
/// returning its disambiguated label, or `None` when nothing matched.
///
/// # Errors
/// Returns `Err` when the handle names more than one account, or when the
/// store cannot be rewritten.
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

/// The object `oauth.json` keeps under one account's key. `service` and
/// `issued` are read back only to check them against that key — see
/// [`load_all_at`] — never to mint one: the key is the index, these fields
/// are the truth it is checked against.
#[derive(Serialize, Deserialize)]
struct StoredToken {
    service: String,
    issued: String,
    email: Option<String>,
    workspace: Option<String>,
    plan: Option<String>,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

impl From<&OAuthToken> for StoredToken {
    fn from(token: &OAuthToken) -> Self {
        Self {
            service: identity::chatgpt_service().name.as_str().to_string(),
            issued: token.issued.clone(),
            email: token.email.clone(),
            workspace: token.workspace.clone(),
            plan: token.plan.clone(),
            access_token: token.access_token.clone(),
            refresh_token: token.refresh_token.clone(),
            expires_at: token.expires_at,
        }
    }
}

impl From<StoredToken> for OAuthToken {
    fn from(stored: StoredToken) -> Self {
        Self {
            access_token: stored.access_token,
            refresh_token: stored.refresh_token,
            issued: stored.issued,
            email: stored.email,
            workspace: stored.workspace,
            plan: stored.plan,
            expires_at: stored.expires_at,
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-read] reads the persisted OAuth tokens; credential store infra, not turn-time data I/O"
)]
fn load_all_at(path: &std::path::Path) -> Vec<OAuthToken> {
    let Some(bytes) = std::fs::read(path).ok() else {
        return Vec::new();
    };
    let Ok(stored) = serde_json::from_slice::<BTreeMap<String, StoredToken>>(&bytes) else {
        return Vec::new();
    };
    let chatgpt = identity::chatgpt_service().name;
    stored
        .into_iter()
        .filter_map(|(key, entry)| {
            let expected = AccountId::of_login(&chatgpt, &entry.issued);
            if expected.as_str() == key && entry.service == chatgpt.as_str() {
                return Some(OAuthToken::from(entry));
            }
            eprintln!(
                "warning: {} names an entry as '{key}', but its own fields say '{expected}' — \
                 dropping it rather than trusting a key that disagrees with the record.",
                path.display(),
            );
            None
        })
        .collect()
}

fn save_one_at(path: &std::path::Path, token: &OAuthToken) -> Result<bool, String> {
    let _guard = STORE_LOCK.lock_ignore_poison();
    let mut all = load_all_at(path);
    let replaced = if let Some(existing) = all.iter_mut().find(|t| t.issued == token.issued) {
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
    let chatgpt = identity::chatgpt_service().name;
    let id_of = |token: &OAuthToken| AccountId::of_login(&chatgpt, &token.issued);
    // The account-id arm accepts what a disambiguated label ends with, so a
    // name copied off `exarch accounts` logs out the account it names.
    let matched: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            t.issued == account || id_of(t).as_str() == account || t.handle() == account
        })
        .map(|(i, _)| i)
        .collect();
    // One handle, several accounts: only an id says which to drop.
    let [pos] = matched[..] else {
        if matched.is_empty() {
            return Ok(None);
        }
        return Err(format!(
            "'{account}' names {} signed-in accounts; \
             log out by account id instead ({})",
            matched.len(),
            matched
                .iter()
                .map(|&i| id_of(&all[i]).to_string())
                .collect::<Vec<_>>()
                .join(", "),
        ));
    };
    // Named against its fellows while it is still among them, so the report
    // says which of a colliding pair went.
    let accounts: Vec<Account> = all.iter().map(to_account).collect();
    let removed = identity::label(&accounts[pos], &accounts);
    all.remove(pos);
    // A fully logged-out machine carries no store at all, not an empty one.
    if all.is_empty() {
        clear_at(path)?;
    } else {
        write_all_at(path, &all)?;
    }
    Ok(Some(removed))
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
    let keyed: BTreeMap<String, StoredToken> = all
        .iter()
        .map(|token| {
            let id = AccountId::of_login(&identity::chatgpt_service().name, &token.issued);
            (id.as_str().to_string(), StoredToken::from(token))
        })
        .collect();
    let json = serde_json::to_string_pretty(&keyed)
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
    Ok(renewed(
        current,
        access_token,
        resp.refresh_token,
        resp.id_token.as_deref(),
    ))
}

/// Fold a token-endpoint refresh into `current`'s successor.
///
/// The issued id is pinned to `current`'s: a refresh renews a credential, it
/// never changes who the account is — every map keys on that id, and adopting
/// a fresh claim's would strand the live cell under a key its own token
/// disputes. Fresh claims may update the handle's ingredients; a response
/// omitting the `id_token` keeps the current ones, so a refresh never loses
/// the account's name either.
fn renewed(
    current: &OAuthToken,
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<&str>,
) -> OAuthToken {
    let claims = id_token.and_then(decode_claims);
    let auth = claims.as_ref().and_then(|c| c.auth.as_ref());
    OAuthToken {
        expires_at: expiry_secs(&access_token, id_token),
        access_token,
        refresh_token: refresh_token.unwrap_or_else(|| current.refresh_token.clone()),
        issued: current.issued.clone(),
        email: claims
            .as_ref()
            .and_then(|c| c.email.clone())
            .or_else(|| current.email.clone()),
        workspace: claims
            .as_ref()
            .and_then(workspace_title)
            .or_else(|| current.workspace.clone()),
        plan: auth
            .and_then(|a| a.chatgpt_plan_type.clone())
            .or_else(|| current.plan.clone()),
    }
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
        ("chatgpt-account-id".into(), token.issued.clone()),
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
    /// The plan type ("plus", "pro", "team", ...) — confirmed present by
    /// decoding a live token. [`IdClaims::organizations`] sits beside it in
    /// the ladder but is not yet confirmed; correcting either's claim name is
    /// a one-line `#[serde(rename = ...)]` away.
    chatgpt_plan_type: Option<String>,
}

#[derive(Deserialize)]
struct IdClaims {
    #[serde(rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
    /// The standard OIDC claim; present only because [`SCOPE`] asks for it.
    email: Option<String>,
    /// One workspace per login, requested by `browser.rs`'s
    /// `id_token_add_organizations=true`; not yet confirmed against a live
    /// token — see [`AuthClaims::chatgpt_plan_type`].
    organizations: Option<Vec<Organization>>,
}

#[derive(Deserialize)]
struct Organization {
    title: Option<String>,
    is_default: Option<bool>,
}

/// The default organization's title, or `None` when the claim, the array, or
/// the title itself is absent — the handle ladder then falls through to the
/// plan type.
fn workspace_title(claims: &IdClaims) -> Option<String> {
    claims
        .organizations
        .as_ref()?
        .iter()
        .find(|org| org.is_default == Some(true))?
        .title
        .clone()
}

fn decode_claims(jwt: &str) -> Option<IdClaims> {
    jwt_payload(jwt).ok()
}

#[derive(Deserialize)]
struct ExpClaims {
    exp: Option<i64>,
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
    let claims = decode_claims(&raw.id_token);
    let auth = claims.as_ref().and_then(|c| c.auth.as_ref());
    let issued = auth
        .and_then(|a| a.chatgpt_account_id.clone())
        .ok_or_else(|| "login did not return a ChatGPT account id".to_string())?;
    let expires_at = expiry_secs(&raw.access_token, Some(&raw.id_token));
    Ok(OAuthToken {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        issued,
        email: claims.as_ref().and_then(|c| c.email.clone()),
        workspace: claims.as_ref().and_then(workspace_title),
        plan: auth.and_then(|a| a.chatgpt_plan_type.clone()),
        expires_at,
    })
}

fn token_path() -> PathBuf {
    crate::bootstrap::EXARCH
        .xdg_dir(ral_core::path::basedir::XdgKind::State)
        .join("oauth.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(issued: &str, email: Option<&str>) -> OAuthToken {
        OAuthToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            issued: issued.into(),
            email: email.map(str::to_string),
            workspace: None,
            plan: None,
            expires_at: 0,
        }
    }

    /// One human, one email, two accounts — a personal one and a workspace
    /// one. A handle drawn from the token is local, so it names both alike;
    /// what tells them apart is the id, and only where it has to.
    #[test]
    fn a_shared_email_is_qualified_by_the_account_id() {
        let alone = [token("acc_1", Some("alex@work"))];
        assert_eq!(
            identity::label(&to_account(&alone[0]), &accounts_of(&alone)),
            "chatgpt · alex@work"
        );

        let shared = [
            token("acc_personal", Some("alex@work")),
            token("acc_team", Some("alex@work")),
        ];
        let named = accounts_of(&shared);
        assert!(identity::label(&named[0], &named).ends_with("chatgpt:acc_personal"));
        assert!(identity::label(&named[1], &named).ends_with("chatgpt:acc_team"));
    }

    fn accounts_of(tokens: &[OAuthToken]) -> Vec<Account> {
        tokens.iter().map(to_account).collect()
    }

    #[test]
    fn logout_by_a_shared_email_asks_for_an_account_id_instead() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("oauth.json");
        for issued in ["acc_personal", "acc_team"] {
            save_one_at(&path, &token(issued, Some("alex@work"))).expect("seed");
        }

        let error = remove_at(&path, "alex@work").expect_err("the email names two accounts");
        assert!(
            error.contains("acc_personal") && error.contains("acc_team"),
            "{error}"
        );
        assert_eq!(load_all_at(&path).len(), 2, "neither was taken");

        let removed = remove_at(&path, "acc_team").expect("an account id is unambiguous");
        assert!(removed.unwrap().contains("acc_team"));
        assert_eq!(load_all_at(&path).len(), 1);
    }

    /// The id-qualified label ends with the full `AccountId` rendering, so a
    /// name copied off `exarch accounts` must log out the account it names.
    #[test]
    fn logout_accepts_the_full_account_id_rendering() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("oauth.json");
        for issued in ["acc_personal", "acc_team"] {
            save_one_at(&path, &token(issued, Some("alex@work"))).expect("seed");
        }

        let removed = remove_at(&path, "chatgpt:acc_team").expect("a rendering is unambiguous");
        assert!(removed.unwrap().contains("acc_team"));
        assert_eq!(load_all_at(&path).len(), 1);
    }

    fn fake_id_token(payload: &serde_json::Value) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(payload.to_string())
        )
    }

    /// A refresh renews the credential and the name's ingredients, never the
    /// identity: a claims set naming a different account id must not re-key
    /// the account out from under the maps that hold it.
    #[test]
    fn a_refresh_renames_but_never_rekeys_the_account() {
        let current = token("acct-1", Some("old@work"));
        let id_token = fake_id_token(&serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-other",
                "chatgpt_plan_type": "pro",
            },
            "email": "new@work",
        }));

        let fresh = renewed(&current, "fresh-at".into(), None, Some(&id_token));

        assert_eq!(
            fresh.issued, "acct-1",
            "identity is pinned across a refresh"
        );
        assert_eq!(fresh.email.as_deref(), Some("new@work"));
        assert_eq!(fresh.plan.as_deref(), Some("pro"));
        assert_eq!(
            fresh.refresh_token, "rt",
            "an omitted refresh token keeps the current one"
        );
    }

    /// A refresh response carrying no `id_token` keeps every claim it has.
    #[test]
    fn a_refresh_without_claims_keeps_the_name() {
        let current = OAuthToken {
            workspace: Some("Acme Ltd".into()),
            ..token("acct-1", Some("alex@work"))
        };
        let fresh = renewed(&current, "fresh-at".into(), Some("fresh-rt".into()), None);
        assert_eq!(fresh.email.as_deref(), Some("alex@work"));
        assert_eq!(fresh.workspace.as_deref(), Some("Acme Ltd"));
        assert_eq!(fresh.refresh_token, "fresh-rt");
    }

    #[test]
    fn two_logins_on_one_email_round_trip_through_the_keyed_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("oauth.json");
        save_one_at(&path, &token("acc_personal", Some("alex@work"))).expect("seed 1");
        save_one_at(&path, &token("acc_team", Some("alex@work"))).expect("seed 2");

        let loaded = load_all_at(&path);
        assert_eq!(loaded.len(), 2, "both logins came back");
        assert!(loaded.iter().any(|t| t.issued == "acc_personal"));
        assert!(loaded.iter().any(|t| t.issued == "acc_team"));
    }

    #[test]
    fn signing_in_twice_to_the_same_account_stays_one_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("oauth.json");
        save_one_at(&path, &token("acc_1", Some("alex@work"))).expect("seed");
        save_one_at(
            &path,
            &OAuthToken {
                access_token: "fresh".into(),
                ..token("acc_1", Some("alex@work"))
            },
        )
        .expect("re-login");

        let loaded = load_all_at(&path);
        assert_eq!(loaded.len(), 1, "a re-login updates, it does not duplicate");
        assert_eq!(loaded[0].access_token, "fresh");
    }

    /// The key is an index into the map, not a fact to trust: an entry filed
    /// under a key its own fields disagree with is dropped, not adopted.
    #[test]
    fn an_entry_whose_key_disagrees_with_its_fields_is_dropped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("oauth.json");
        std::fs::write(
            &path,
            r#"{"chatgpt:wrong-key":{"service":"chatgpt","issued":"acc_1",
                "email":null,"workspace":null,"plan":null,
                "access_token":"at","refresh_token":"rt","expires_at":0}}"#,
        )
        .expect("write a mismatched entry directly");

        assert_eq!(
            load_all_at(&path),
            Vec::new(),
            "a mismatched key is not trusted"
        );
    }
}
