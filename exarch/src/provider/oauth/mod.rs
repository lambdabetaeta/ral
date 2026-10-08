//! "Sign in with ChatGPT": `ChatGPT`-plan accounts authorised through
//! `OpenAI`'s token-sharing route for open-source apps rather than an API key.
//!
//! A browser redirect to a loopback listener ([`browser`]) ends in an
//! authorization-code exchange, yielding a JWT `access_token`, a rotating
//! refresh token, and an `id_token` naming the account. Each account is its
//! own issued client (`oaiapp_…`), registered once and presented on every
//! later request; one host id per machine ties them together. Several logins
//! coexist in one store keyed by [`identity::AccountId`], each a selectable
//! [`Account`]; [`refresh`] upserts, so renewing one never disturbs the
//! others.

mod browser;

use crate::provider::error::body_detail;
use crate::provider::identity::{self, Account, AccountId};
use crate::provider::secret_file::write_private;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ral_core::sync::LockExt;
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

const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const DISCOVERY_URL: &str = "https://auth.openai.com/.well-known/openid-configuration";
/// The first-registration client; the callback answers with the issued one.
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const AGENT_NAME: &str = "exarch";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The one scope that authorises plan usage; a grant without it is refused.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const RESOURCE: &str = "https://api.openai.com/v1";

/// How long before expiry a token is renewed.
const REFRESH_WINDOW_SECS: u64 = 5 * 60;

/// A persisted `ChatGPT` login: a short-lived JWT access token, the refresh
/// token that mints its successors, and the client `OpenAI` issued the account.
///
/// One per signed-in account, keyed by [`Self::issued`] under the `chatgpt`
/// service — see [`to_account`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: String,
    /// Retained for `id_token_hint` on a re-login; may be expired by then.
    pub id_token: String,
    /// The client `OpenAI` issued this account's registration (`oaiapp_…`):
    /// every token request for this account names it, never `DYNAMIC_CLIENT`.
    pub client_id: String,
    /// The ID token's `sub`: `AccountId::of_login`'s second half.
    pub issued: String,
    /// From the `id_token`'s `email` claim; `None` when it carried none, and
    /// then [`Self::issued`] stands in for [`Self::handle`].
    pub email: Option<String>,
    /// Unix seconds at which `access_token` expires.
    pub expires_at: u64,
}

impl OAuthToken {
    /// True when the access token has expired or is within
    /// [`REFRESH_WINDOW_SECS`] of expiring.
    pub fn is_stale(&self) -> bool {
        self.expires_at <= crate::app::now_secs() + REFRESH_WINDOW_SECS
    }

    /// This account's local handle: its login email, or the issued id when
    /// none was given. Computed from this token alone; see
    /// [`identity::label`] for how a whole set of accounts is then told apart.
    pub fn handle(&self) -> String {
        self.email.clone().unwrap_or_else(|| self.issued.clone())
    }
}

/// The `chatgpt` [`Account`] a persisted login names — the one conversion
/// every other reader of the token store goes through.
pub(crate) fn to_account(token: &OAuthToken) -> Account {
    Account::chatgpt(&token.issued, token.handle())
}

/// The token-endpoint success body of a code exchange.
#[derive(Deserialize)]
// The `_token` suffix is the token-endpoint wire format; renaming would break serde.
#[allow(clippy::struct_field_names)]
pub(super) struct RawTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    pub scope: String,
}

/// A completed authorization: the exchanged tokens and what finalising them
/// is checked against.
pub(super) struct Granted {
    pub raw: RawTokens,
    pub client_id: String,
    /// This attempt's nonce, checked against the ID token's claim.
    pub nonce: String,
}

/// Which client a sign-in presents: a new registration, or an account's own
/// issued client, so signing in again never registers exarch twice.
#[derive(Clone)]
pub enum SignIn {
    Register,
    Reauthorize(OAuthToken),
}

/// One staged report from a running login flow.
///
/// The CLI adapter ([`Self::stderr_line`]) and the TUI's `/login` overlay both
/// render exactly these two phases; there is no percentage or elapsed clock.
pub enum LoginPhase {
    /// `url` is offered whatever the launcher did with it: a launch that
    /// reports success still shows nothing when the browser it started lives
    /// on a machine the user is not sitting at.
    AwaitingBrowser {
        url: String,
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
            Self::ExchangingCode => None,
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start runtime: {e}"))
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
    sign_in: &SignIn,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<(OAuthToken, bool), String> {
    let rt = runtime()?;
    let client = http_client()?;
    let host = host_id()?;
    let granted = rt.block_on(browser::run(&client, &host, sign_in, on_phase, cancel))?;
    // The exchange itself does not poll the flag, so a cancel landing after
    // the wait loop returned `Ok` would otherwise still persist a token.
    if cancel.load(Ordering::Relaxed) {
        return Err("sign-in cancelled".to_string());
    }
    let token = finalize(granted, sign_in)?;
    let replaced = save_one(&token)?;
    Ok((token, replaced))
}

/// The `exarch login` command: [`login_flow`] with its phases rendered to
/// stderr, and no cancellation — a CLI run has no overlay to Esc out of, and
/// Ctrl-C kills the process.
///
/// # Errors
/// Returns `Err` if `account` names no signed-in account or several, if the
/// runtime or HTTP client cannot be built, if the flow fails, or if finalising
/// or persisting the token fails.
pub fn login(account: Option<String>) -> Result<(), String> {
    let sign_in = match account {
        None => SignIn::Register,
        Some(name) => {
            let tokens = load_all();
            let matched: Vec<_> = tokens.iter().filter(|t| names(t, &name)).collect();
            match matched.as_slice() {
                [token] => SignIn::Reauthorize((*token).clone()),
                [] => {
                    return Err(format!(
                        "no ChatGPT account matches '{name}' (signed in: {})",
                        joined_labels(&tokens),
                    ));
                }
                many => return Err(ambiguous(&name, "sign in", many)),
            }
        }
    };
    let (token, replaced) = login_flow(
        &sign_in,
        |phase| {
            if let Some(line) = phase.stderr_line() {
                ral_core::errln!("{line}");
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
    ral_core::errln!("{verb} ChatGPT account {named}.");
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
        revoke_all(&load_all());
        clear_at(&token_path())?;
        ral_core::errln!("Logged out of every ChatGPT account.");
        return Ok(());
    }
    let tokens = load_all();
    let target = match (account, tokens.as_slice()) {
        (Some(name), _) => name,
        (None, []) => {
            ral_core::errln!("No ChatGPT account to log out of.");
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
    if let [token] = tokens
        .iter()
        .filter(|t| names(t, &target))
        .collect::<Vec<_>>()[..]
    {
        revoke_all(std::slice::from_ref(token));
    }
    match sign_out(&target)? {
        Some(removed) => {
            let among: Vec<_> = tokens.iter().map(to_account).collect();
            let label = identity::label(&to_account(&removed), &among);
            ral_core::errln!("Logged out of ChatGPT account {label}.");
            Ok(())
        }
        None => Err(format!(
            "no ChatGPT account matches '{target}' (signed in: {})",
            joined_labels(&tokens),
        )),
    }
}

/// Remove the login matched by `account` (handle, issued id, or account id)
/// from the local store, returning it, or `None` when nothing matched.
/// Revoking it at the issuer is [`revoke_blocking`]'s job.
///
/// # Errors
/// Returns `Err` when the handle names more than one account, or when the
/// store cannot be rewritten.
pub(crate) fn sign_out(account: &str) -> Result<Option<OAuthToken>, String> {
    remove_at(&token_path(), account)
}

/// Revoke `token` at the issuer, blocking on the network: never call it from
/// the UI thread.
pub(crate) fn revoke_blocking(token: &OAuthToken) -> Result<(), String> {
    runtime()?.block_on(revoke(token))
}

/// Best-effort revocation at the issuer: the local record goes regardless.
fn revoke_all(tokens: &[OAuthToken]) {
    for token in tokens {
        if let Err(e) = revoke_blocking(token) {
            ral_core::errln!(
                "warning: could not revoke the ChatGPT token for {}: {e}",
                token.handle()
            );
        }
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

// The `*_at` core takes the path as an argument so tests drive it against a
// temp file without mutating the process environment.

/// Serializes `save_one_at`, `remove_at` and `host_id_at`'s load-modify-write: two
/// concurrent refreshes, one per stale account, would otherwise each write
/// back a stale copy of the *other*, silently reverting it. `load_all_at`
/// stays lock-free — a bare read is never part of that race.
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `oauth.json`: the host id and every account, keyed by [`AccountId`].
#[derive(Serialize, Deserialize, Default)]
struct Store {
    /// `ext_agent_host_id`: opaque, minted once per host, never user-identifying.
    host: Option<String>,
    accounts: BTreeMap<String, StoredToken>,
}

/// The object [`Store`] keeps under one account's key. `service` and
/// `issued` are read back only to check them against that key (see
/// [`read_at`]), never to mint one: the key is the index, these fields
/// are the truth it is checked against.
#[derive(Serialize, Deserialize)]
struct StoredToken {
    service: String,
    issued: String,
    email: Option<String>,
    id_token: String,
    client_id: String,
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
            id_token: token.id_token.clone(),
            client_id: token.client_id.clone(),
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
            id_token: stored.id_token,
            client_id: stored.client_id,
            issued: stored.issued,
            email: stored.email,
            expires_at: stored.expires_at,
        }
    }
}

/// The store's host id and its trustworthy accounts. An absent or unreadable
/// store reads as empty.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:token-read] reads the persisted OAuth tokens; credential store infra, not turn-time data I/O"
)]
fn read_at(path: &std::path::Path) -> (Option<String>, Vec<OAuthToken>) {
    let Some(store) = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Store>(&bytes).ok())
    else {
        return (None, Vec::new());
    };
    let chatgpt = identity::chatgpt_service().name;
    let tokens = store
        .accounts
        .into_iter()
        .filter_map(|(key, entry)| {
            let expected = AccountId::of_login(&chatgpt, &entry.issued);
            if expected.as_str() == key && entry.service == chatgpt.as_str() {
                return Some(OAuthToken::from(entry));
            }
            ral_core::errln!(
                "warning: {} names an entry as '{key}', but its own fields say '{expected}'; \
                 dropping it rather than trusting a key that disagrees with the record.",
                path.display(),
            );
            None
        })
        .collect();
    (store.host, tokens)
}

fn load_all_at(path: &std::path::Path) -> Vec<OAuthToken> {
    read_at(path).1
}

fn save_one_at(path: &std::path::Path, token: &OAuthToken) -> Result<bool, String> {
    let _guard = STORE_LOCK.lock_ignore_poison();
    let (host, mut all) = read_at(path);
    let replaced = if let Some(existing) = all.iter_mut().find(|t| t.issued == token.issued) {
        *existing = token.clone();
        true
    } else {
        all.push(token.clone());
        false
    };
    write_all_at(path, host.as_deref(), &all)?;
    Ok(replaced)
}

/// This host's id, minting and persisting one on first use.
fn host_id_at(path: &std::path::Path) -> Result<String, String> {
    let _guard = STORE_LOCK.lock_ignore_poison();
    let (host, all) = read_at(path);
    if let Some(host) = host {
        return Ok(host);
    }
    let host = uuid::Uuid::new_v4().urn().to_string();
    write_all_at(path, Some(&host), &all)?;
    Ok(host)
}

/// This host's id: opaque, minted on first use and kept in the token store.
///
/// # Errors
/// Returns `Err` when a freshly minted id cannot be persisted.
pub fn host_id() -> Result<String, String> {
    host_id_at(&token_path())
}

/// Whether `account` (handle, issued id, or account id) names `token`. The
/// account-id arm accepts what a disambiguated label ends with, so a name
/// copied off `exarch accounts` logs out the account it names.
fn names(token: &OAuthToken, account: &str) -> bool {
    token.issued == account || account_id(token).as_str() == account || token.handle() == account
}

fn remove_at(path: &std::path::Path, account: &str) -> Result<Option<OAuthToken>, String> {
    let _guard = STORE_LOCK.lock_ignore_poison();
    let (host, mut all) = read_at(path);
    let matched: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, t)| names(t, account))
        .map(|(i, _)| i)
        .collect();
    let [pos] = matched[..] else {
        if matched.is_empty() {
            return Ok(None);
        }
        let among: Vec<&OAuthToken> = matched.iter().map(|&i| &all[i]).collect();
        return Err(ambiguous(account, "log out", &among));
    };
    let removed = all.remove(pos);
    // A fully logged-out machine carries no store at all, not an empty one.
    if all.is_empty() {
        clear_at(path)?;
    } else {
        write_all_at(path, host.as_deref(), &all)?;
    }
    Ok(Some(removed))
}

/// One handle, several accounts: only an id says which.
fn ambiguous(account: &str, action: &str, among: &[&OAuthToken]) -> String {
    format!(
        "'{account}' names {} signed-in accounts; {action} by account id instead ({})",
        among.len(),
        among
            .iter()
            .map(|t| account_id(t).to_string())
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn account_id(token: &OAuthToken) -> AccountId {
    AccountId::of_login(&identity::chatgpt_service().name, &token.issued)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:token-dir] creates the OAuth token store dir; credential store infra, not turn-time data I/O"
)]
fn write_all_at(
    path: &std::path::Path,
    host: Option<&str>,
    all: &[OAuthToken],
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let store = Store {
        host: host.map(str::to_string),
        accounts: all
            .iter()
            .map(|token| {
                (
                    account_id(token).as_str().to_string(),
                    StoredToken::from(token),
                )
            })
            .collect(),
    };
    let json = serde_json::to_string_pretty(&store)
        .map_err(|e| format!("could not serialize tokens: {e}"))?;
    write_private(path, json.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Delete the store; an absent one is not an error.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:token-remove] deletes the stored OAuth tokens; credential store infra, not turn-time data I/O"
)]
fn clear_at(path: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not remove {}: {e}", path.display())),
    }
}

/// The token-endpoint success body of a refresh.
#[derive(Deserialize)]
// The `_token` suffix is the token-endpoint wire format; renaming would break serde.
#[allow(clippy::struct_field_names)]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

pub(crate) async fn refresh(current: &OAuthToken) -> Result<OAuthToken, String> {
    let client = http_client()?;
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", current.client_id.as_str()),
            ("refresh_token", current.refresh_token.as_str()),
            ("resource", RESOURCE),
        ])
        .send()
        .await
        .map_err(|e| format!("token refresh request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(refresh_failure(status, &body, current));
    }
    let resp: RefreshResponse = resp
        .json()
        .await
        .map_err(|e| format!("could not parse token refresh response: {e}"))?;
    Ok(renewed(current, resp))
}

/// Best-effort revocation of `token`'s refresh token at the issuer's
/// advertised revocation endpoint.
async fn revoke(token: &OAuthToken) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Discovery {
        revocation_endpoint: String,
    }

    let client = http_client()?;
    let discovery = client
        .get(DISCOVERY_URL)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("discovery request failed: {e}"))?;
    let Discovery {
        revocation_endpoint,
    } = json_or_error(discovery, "discovery").await?;
    let resp = client
        .post(revocation_endpoint)
        .timeout(Duration::from_secs(10))
        .form(&[
            ("token", token.refresh_token.as_str()),
            ("token_type_hint", "refresh_token"),
            ("client_id", token.client_id.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("issuer answered {status}"))
    }
}

/// Error codes after which the refresh token will never work again.
const DEAD_REFRESH_CODES: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

/// The message for a non-success refresh response: a login the issuer has
/// killed says to sign in again; anything else keeps the transient wording.
fn refresh_failure(status: reqwest::StatusCode, body: &str, current: &OAuthToken) -> String {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let code = [
        json.get("error").filter(|e| e.is_string()),
        json.pointer("/error/code"),
        json.get("error_code"),
        json.get("code"),
    ]
    .into_iter()
    .flatten()
    .find_map(|c| c.as_str());
    let dead = status == reqwest::StatusCode::UNAUTHORIZED
        || code.is_some_and(|c| DEAD_REFRESH_CODES.contains(&c));
    if dead {
        let name = current.email.as_deref().unwrap_or(&current.issued);
        let code = code.unwrap_or("unauthorized");
        format!(
            "the ChatGPT login for {name} has expired or been revoked ({code}); \
             run `exarch login {name}` to sign in again"
        )
    } else {
        format!("token refresh failed ({status}): {}", body_detail(body))
    }
}

/// Fold a token-endpoint refresh into `current`'s successor.
///
/// The issued id and client are pinned to `current`'s: a refresh renews a
/// credential, it never changes who the account is; every map keys on that
/// id, and adopting a fresh claim's would strand the live cell under a key
/// its own token disputes. A fresh ID token may update the email; a response
/// omitting one keeps the current, so a refresh never loses the account's
/// name either.
fn renewed(current: &OAuthToken, resp: RefreshResponse) -> OAuthToken {
    let email = resp
        .id_token
        .as_deref()
        .and_then(|jwt| jwt_payload::<IdClaims>(jwt).ok())
        .and_then(|claims| claims.email)
        .or_else(|| current.email.clone());
    OAuthToken {
        access_token: resp.access_token,
        refresh_token: resp
            .refresh_token
            .unwrap_or_else(|| current.refresh_token.clone()),
        id_token: resp.id_token.unwrap_or_else(|| current.id_token.clone()),
        client_id: current.client_id.clone(),
        issued: current.issued.clone(),
        email,
        expires_at: crate::app::now_secs() + resp.expires_in.unwrap_or(3600),
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

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:oauth-client] the client every login flow and token refresh talks to the issuer's endpoints with. Sign-in machinery the user started, carrying credentials and no session data; raises no card for the same reason the loopback receiver does not."
)]
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
        return Err(format!(
            "{action} failed ({status}): {}",
            body_detail(&body)
        ));
    }
    resp.json()
        .await
        .map_err(|e| format!("could not parse {action} response: {e}"))
}

pub(super) async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<RawTokens, String> {
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("resource", RESOURCE),
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

/// The ID token's payload, decoded but not signature-verified: it reaches us
/// straight from the issuer's token endpoint over TLS and names the account,
/// nothing more.
#[derive(Deserialize)]
struct IdClaims {
    sub: String,
    nonce: Option<String>,
    email: Option<String>,
}

/// Turn a granted authorization into an [`OAuthToken`], refusing a grant that
/// lacks the plan scope, answers another attempt, or (for a renewal) names
/// another account.
fn finalize(granted: Granted, sign_in: &SignIn) -> Result<OAuthToken, String> {
    let Granted {
        raw,
        client_id,
        nonce,
    } = granted;
    if !raw.scope.split_whitespace().any(|s| s == PLAN_SCOPE) {
        return Err(format!(
            "the sign-in granted no ChatGPT plan usage (scope: {}); does this account's plan allow sharing with apps?",
            raw.scope
        ));
    }
    let claims: IdClaims = jwt_payload(&raw.id_token)?;
    if claims.nonce.as_deref() != Some(nonce.as_str()) {
        return Err("the sign-in's ID token answers a different attempt; try again".to_string());
    }
    if let SignIn::Reauthorize(prev) = sign_in
        && claims.sub != prev.issued
    {
        return Err(format!(
            "you signed in as {}, but this sign-in was to renew {}; pick that account in the browser, or register the new one instead",
            claims.email.as_deref().unwrap_or(&claims.sub),
            prev.handle(),
        ));
    }
    Ok(OAuthToken {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        id_token: raw.id_token,
        client_id,
        issued: claims.sub,
        email: claims.email,
        expires_at: crate::app::now_secs() + raw.expires_in,
    })
}

/// `pub(crate)` for [`super::credential_files`]: a signed-in `ChatGPT`
/// account's tokens live here, so every grant has to carve this path out.
pub(crate) fn token_path() -> PathBuf {
    crate::app::EXARCH
        .xdg_dir(ral_core::host::XdgKind::State)
        .join("oauth.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(issued: &str, email: Option<&str>) -> OAuthToken {
        OAuthToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token: "id".into(),
            client_id: "oaiapp_test".into(),
            issued: issued.into(),
            email: email.map(str::to_string),
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
        assert_eq!(removed.unwrap().issued, "acc_team");
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
        assert_eq!(removed.unwrap().issued, "acc_team");
        assert_eq!(load_all_at(&path).len(), 1);
    }

    fn fake_id_token(payload: &serde_json::Value) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(payload.to_string())
        )
    }

    fn refresh_response(id_token: Option<&str>, refresh_token: Option<&str>) -> RefreshResponse {
        RefreshResponse {
            access_token: "fresh-at".into(),
            refresh_token: refresh_token.map(str::to_string),
            id_token: id_token.map(str::to_string),
            expires_in: Some(3600),
        }
    }

    /// A refresh renews the credential and the name's ingredients, never the
    /// identity: a claims set naming a different account id must not re-key
    /// the account out from under the maps that hold it.
    #[test]
    fn a_refresh_renames_but_never_rekeys_the_account() {
        let current = token("acct-1", Some("old@work"));
        let id_token = fake_id_token(&serde_json::json!({
            "sub": "acct-other",
            "email": "new@work",
        }));

        let fresh = renewed(&current, refresh_response(Some(&id_token), None));

        assert_eq!(
            fresh.issued, "acct-1",
            "identity is pinned across a refresh"
        );
        assert_eq!(fresh.client_id, "oaiapp_test", "the client is pinned too");
        assert_eq!(fresh.email.as_deref(), Some("new@work"));
        assert_eq!(fresh.id_token, id_token);
        assert_eq!(
            fresh.refresh_token, "rt",
            "an omitted refresh token keeps the current one"
        );
    }

    /// A refresh response carrying no `id_token` keeps every claim it has.
    #[test]
    fn a_refresh_without_claims_keeps_the_name() {
        let current = token("acct-1", Some("alex@work"));
        let fresh = renewed(&current, refresh_response(None, Some("fresh-rt")));
        assert_eq!(fresh.email.as_deref(), Some("alex@work"));
        assert_eq!(fresh.id_token, "id");
        assert_eq!(fresh.refresh_token, "fresh-rt");
    }

    #[test]
    fn a_revoked_grant_asks_for_a_new_sign_in() {
        let message = refresh_failure(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_grant"}"#,
            &token("acct-1", Some("alex@work")),
        );
        assert!(message.contains("alex@work"), "{message}");
        assert!(message.contains("exarch login"), "{message}");
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
            r#"{"host":null,"accounts":{"chatgpt:wrong-key":{"service":"chatgpt",
                "issued":"acc_1","email":null,"id_token":"id","client_id":"oaiapp_test",
                "access_token":"at","refresh_token":"rt","expires_at":0}}}"#,
        )
        .expect("write a mismatched entry directly");

        assert_eq!(
            load_all_at(&path),
            Vec::new(),
            "a mismatched key is not trusted"
        );
    }

    #[test]
    fn the_host_id_is_minted_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("oauth.json");
        let first = host_id_at(&path).expect("mint");
        assert!(first.starts_with("urn:uuid:"), "{first}");
        save_one_at(&path, &token("acc_1", Some("alex@work"))).expect("seed");

        assert_eq!(host_id_at(&path).expect("read"), first);
        assert_eq!(load_all_at(&path).len(), 1, "minting kept the accounts");
    }

    fn granted(scope: &str, claims: &serde_json::Value) -> Granted {
        Granted {
            raw: RawTokens {
                id_token: fake_id_token(claims),
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_in: 3600,
                scope: scope.into(),
            },
            client_id: "oaiapp_test".into(),
            nonce: "n1".into(),
        }
    }

    const FULL_SCOPE: &str = "openid email chatgpt.tokens.use.direct";

    #[test]
    fn a_grant_without_plan_scope_is_refused() {
        let granted = granted(
            "openid email",
            &serde_json::json!({"sub": "a", "nonce": "n1"}),
        );
        let error = finalize(granted, &SignIn::Register).expect_err("no plan scope");
        assert!(error.contains("plan"), "{error}");
    }

    #[test]
    fn a_mismatched_nonce_is_refused() {
        let granted = granted(
            FULL_SCOPE,
            &serde_json::json!({"sub": "a", "nonce": "other"}),
        );
        let error = finalize(granted, &SignIn::Register).expect_err("wrong attempt");
        assert!(error.contains("different attempt"), "{error}");
    }

    #[test]
    fn renewing_a_different_account_is_refused() {
        let granted = granted(
            FULL_SCOPE,
            &serde_json::json!({"sub": "acct-2", "nonce": "n1", "email": "two@work"}),
        );
        let sign_in = SignIn::Reauthorize(token("acct-1", Some("one@work")));
        let error = finalize(granted, &sign_in).expect_err("another account");
        assert!(
            error.contains("two@work") && error.contains("one@work"),
            "{error}"
        );
    }
}
