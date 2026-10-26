//! Independent plan sign-ins, account credentials and issuer-specific flows.
//!
//! The grant selects the service and protocol. A service-qualified account id
//! keys persistence and live credentials; renewal and refresh never re-key it.

mod chatgpt;
pub(super) mod codex;
mod grok;
mod loopback;
mod session;
pub use session::CredentialHandle;

use crate::provider::error::body_detail;
use crate::provider::identity::{self, Account, AccountId, Login};
use crate::site::{Silent, site};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ral_core::own::{OwnDir, OwnError};
use ral_core::path::{Visibility, XdgKind};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const TOKEN_FILE: &str = "oauth.json";
/// Expiry is actual expiry; every route uses this one refresh margin.
const REFRESH_WINDOW_SECS: u64 = 5 * 60;

/// One account's credential. Never log this struct: it contains secrets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: String,
    /// Opaque issuer-local identity, qualified by service in an account id.
    pub issued: String,
    pub email: Option<String>,
    /// Actual access-token expiry, in Unix seconds.
    pub expires_at: u64,
    pub grant: Grant,
}

/// Protocol-specific credentials and naming metadata; never log this sum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grant {
    Chatgpt {
        /// Retained for a future renewal's `id_token_hint`.
        id_token: String,
        client_id: String,
    },
    Codex {
        /// Raw backend id, not the composite local `issued` identity.
        account_id: String,
        workspace: Option<String>,
        plan: Option<String>,
    },
    Grok,
}

impl OAuthToken {
    pub fn login(&self) -> Login {
        match &self.grant {
            Grant::Chatgpt { .. } => Login::Chatgpt,
            Grant::Codex { .. } => Login::Codex,
            Grant::Grok => Login::Grok,
        }
    }

    pub fn is_stale(&self) -> bool {
        self.expires_at <= crate::app::now_secs().saturating_add(REFRESH_WINDOW_SECS)
    }

    /// Email, else identity, with Codex's workspace-before-plan qualifier.
    pub fn handle(&self) -> String {
        let handle = self.email.as_deref().unwrap_or(&self.issued);
        if let Grant::Codex {
            workspace, plan, ..
        } = &self.grant
            && let Some(qualifier) = workspace.as_ref().or(plan.as_ref())
        {
            return format!("{handle} ({qualifier})");
        }
        handle.to_string()
    }

    /// Lowercase request headers; only Codex receives backend-specific ones.
    pub fn request_headers(&self, accept: &str) -> Vec<(String, String)> {
        match &self.grant {
            Grant::Codex { account_id, .. } => vec![
                (
                    "authorization".into(),
                    format!("Bearer {}", self.access_token),
                ),
                ("chatgpt-account-id".into(), account_id.clone()),
                ("openai-beta".into(), "responses=experimental".into()),
                ("originator".into(), codex::ORIGINATOR.into()),
                (
                    "user-agent".into(),
                    format!("codex_cli_rs/{}", codex::codex_client_version()),
                ),
                ("accept".into(), accept.into()),
            ],
            Grant::Chatgpt { .. } | Grant::Grok => vec![
                (
                    "authorization".into(),
                    format!("Bearer {}", self.access_token),
                ),
                ("accept".into(), accept.into()),
            ],
        }
    }

    pub fn auth_data(&self) -> genai::resolver::AuthData {
        match &self.grant {
            Grant::Chatgpt { .. } | Grant::Grok => {
                genai::resolver::AuthData::from_single(self.access_token.clone())
            }
            Grant::Codex { .. } => genai::resolver::AuthData::RequestOverride {
                url: codex::RESPONSES_URL.into(),
                headers: self.request_headers("text/event-stream").into(),
            },
        }
    }
}

pub(crate) fn to_account(token: &OAuthToken) -> Account {
    Account::login(token.login(), &token.issued, token.handle())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Browser,
    Device,
}

pub fn methods(login: Login) -> &'static [Method] {
    match login {
        Login::Chatgpt => &[Method::Browser],
        Login::Codex => &[Method::Browser, Method::Device],
        Login::Grok => &[Method::Device],
    }
}

#[derive(Clone)]
pub enum SignIn {
    New(Login, Method),
    Renew(OAuthToken, Method),
}

impl SignIn {
    pub fn login(&self) -> Login {
        match self {
            Self::New(login, _) => *login,
            Self::Renew(token, _) => token.login(),
        }
    }

    pub fn method(&self) -> Method {
        match self {
            Self::New(_, method) | Self::Renew(_, method) => *method,
        }
    }
}

pub enum LoginPhase {
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

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start runtime: {e}"))
}

fn validate_method(login: Login, method: Method) -> Result<(), String> {
    match (login, method) {
        (Login::Chatgpt | Login::Codex, Method::Browser)
        | (Login::Codex | Login::Grok, Method::Device) => Ok(()),
        (Login::Chatgpt, Method::Device) => Err(format!(
            "{} signs in through a browser; it has no device-code flow",
            login.service().name
        )),
        (Login::Grok, Method::Browser) => Err(format!(
            "{} signs in through a device code; it has no browser flow",
            login.service().name
        )),
    }
}

/// A renewal may update a credential or its label, never its full identity.
fn check_renewal(sign_in: &SignIn, new: &OAuthToken) -> Result<(), String> {
    if let SignIn::Renew(previous, _) = sign_in
        && account_id(previous) != account_id(new)
    {
        return Err(format!(
            "you signed in as {}, but this sign-in was to renew {}; choose that account when asked who is signing in, or sign in as a new account instead",
            new.handle(),
            previous.handle()
        ));
    }
    Ok(())
}

/// Run one interactive flow and persist only a completed, uncancelled login.
///
/// # Errors
/// Unsupported methods and already-cancelled requests fail before any runtime,
/// network or state access. Flow, renewal and persistence failures are errors.
pub fn login_flow(
    sign_in: &SignIn,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<(CredentialHandle, bool), String> {
    validate_method(sign_in.login(), sign_in.method())?;
    cancelled(cancel)?;
    let rt = runtime()?;
    let client = http_client()?;
    let token = match (sign_in.login(), sign_in.method()) {
        (Login::Chatgpt, Method::Browser) => {
            let host = host_id()?;
            rt.block_on(chatgpt::sign_in(&client, &host, sign_in, on_phase, cancel))
        }
        (Login::Codex, Method::Browser) => {
            rt.block_on(codex::browser(&client, sign_in, on_phase, cancel))
        }
        (Login::Codex, Method::Device) => {
            rt.block_on(codex::device(&client, sign_in, on_phase, cancel))
        }
        (Login::Grok, Method::Device) => {
            rt.block_on(grok::device(&client, sign_in, on_phase, cancel))
        }
        (Login::Chatgpt, Method::Device) | (Login::Grok, Method::Browser) => {
            unreachable!("unsupported methods were rejected before runtime construction")
        }
    }?;
    // HTTP exchange does not poll the flag. A late cancellation must not save.
    cancelled(cancel)?;
    check_renewal(sign_in, &token)?;
    let state = state()?;
    let cancel = Arc::clone(cancel);
    session::transaction(&state, move |lease| {
        cancelled(&cancel)?;
        lease.commit_login(&token)
    })
}

/// CLI sign-in, with a service-scoped optional renewal target.
///
/// # Errors
/// Returns flow/store errors, or a missing or ambiguous renewal target.
pub fn login(service: Login, account: Option<String>, device: bool) -> Result<(), String> {
    let method = if device {
        Method::Device
    } else {
        methods(service)[0]
    };
    validate_method(service, method)?;
    let sign_in = match account {
        None => SignIn::New(service, method),
        Some(name) => SignIn::Renew(renewal_target(&load_all(), service, &name)?, method),
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
    let named = identity::label(&to_account(&token.snapshot()), &accounts());
    ral_core::errln!("{verb} {named}.");
    Ok(())
}

fn renewal_target(tokens: &[OAuthToken], login: Login, name: &str) -> Result<OAuthToken, String> {
    let matched = matching_indices(tokens, name, Some(login));
    match matched.as_slice() {
        [pos] => Ok(tokens[*pos].clone()),
        [] => {
            let available: Vec<_> = tokens
                .iter()
                .filter(|token| token.login() == login)
                .map(to_account)
                .collect();
            Err(format!(
                "no {} account matches '{name}' (signed in: {})",
                login.service().name,
                identity::roster(&available)
            ))
        }
        many => Err(ambiguous(
            name,
            "sign in",
            &many.iter().map(|&i| &tokens[i]).collect::<Vec<_>>(),
        )),
    }
}

/// Remove local credentials before best-effort remote revocation.
///
/// # Errors
/// Missing/ambiguous targets and local persistence failure have no remote side effects.
pub fn logout(account: Option<String>, all: bool) -> Result<(), String> {
    let message = logout_at(&state()?, account, all, revoke_all)?;
    ral_core::errln!("{message}");
    Ok(())
}

/// The local command core takes its state and post-removal action explicitly,
/// so lifecycle tests never read the user's store or contact a live issuer.
fn logout_at(
    state: &OwnDir,
    account: Option<String>,
    all: bool,
    on_removed: impl Fn(&[OAuthToken]),
) -> Result<String, String> {
    if all {
        let tokens = clear_all_at(state)?;
        on_removed(&tokens);
        return Ok("Logged out of every signed-in account.".to_string());
    }
    let (_, tokens) = read_at(state)?;
    let target = match (account, tokens.as_slice()) {
        (Some(name), _) => name,
        (None, []) => return Ok("No signed-in account to log out of.".to_string()),
        (None, [only]) => account_id(only).to_string(),
        (None, _) => {
            return Err(format!(
                "multiple accounts signed in ({}); name one to log out, or pass --all",
                joined_labels(&tokens)
            ));
        }
    };
    match remove_at(state, &target)? {
        Some(removed) => {
            on_removed(std::slice::from_ref(&removed));
            let among: Vec<_> = tokens.iter().map(to_account).collect();
            let label = identity::label(&to_account(&removed), &among);
            Ok(format!("Logged out of {label}."))
        }
        None => Err(format!(
            "no signed-in account matches '{target}' (signed in: {})",
            joined_labels(&tokens)
        )),
    }
}

/// Remove a unique local account; remote revocation is a separate worker job.
pub(crate) fn sign_out(account: &str) -> Result<Option<OAuthToken>, String> {
    remove_at(&state()?, account)
}

/// What the issuer accepted, not a promise about already-issued access tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Revocation {
    AtIssuer,
    LocalOnly,
}

/// Blocking network operation: never invoke it from a UI thread.
pub(crate) fn revoke_blocking(token: &OAuthToken) -> Result<Revocation, String> {
    if matches!(&token.grant, Grant::Codex { .. }) {
        return Ok(Revocation::LocalOnly);
    }
    runtime()?.block_on(revoke(token))
}

async fn revoke(token: &OAuthToken) -> Result<Revocation, String> {
    match &token.grant {
        Grant::Chatgpt { client_id, .. } => chatgpt::revoke(token, client_id).await,
        Grant::Codex { .. } => Ok(Revocation::LocalOnly),
        Grant::Grok => grok::revoke(token).await,
    }
}

fn revoke_all(tokens: &[OAuthToken]) {
    let accounts: Vec<_> = tokens.iter().map(to_account).collect();
    for token in tokens {
        let label = identity::label(&to_account(token), &accounts);
        match revoke_blocking(token) {
            Ok(Revocation::AtIssuer) => {}
            Ok(Revocation::LocalOnly) => ral_core::errln!(
                "{label}: removed here only; exarch does not revoke this login at the issuer, so its credentials may still be usable. Revoke app access in the issuer's account settings if available."
            ),
            Err(error) => ral_core::errln!(
                "warning: {label}: removed here, but revocation at the issuer failed: {error}"
            ),
        }
    }
}

fn joined_labels(tokens: &[OAuthToken]) -> String {
    identity::roster(&tokens.iter().map(to_account).collect::<Vec<_>>())
}

/// Queries offer no accounts when the store cannot be read. Writers do not
/// use this forgiving path, so they can never replace an unreadable store.
pub fn load_all() -> Vec<OAuthToken> {
    state().map_or_else(|_| Vec::new(), |state| load_all_at(&state))
}

pub fn accounts() -> Vec<Account> {
    load_all().iter().map(to_account).collect()
}

/// Seed an isolated credential fixture.
///
/// # Errors
/// Reading or replacing the store failed.
#[cfg(any(test, feature = "test-util"))]
pub fn save_one(token: &OAuthToken) -> Result<bool, String> {
    save_one_at(&state()?, token)
}

/// Current persisted accounts, attached to their canonical live owners.
pub fn credentials() -> Vec<CredentialHandle> {
    let Ok(state) = state() else {
        return Vec::new();
    };
    session::transaction(&state, session::StoreLease::credentials).unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn commit_for_test(state: &OwnDir, token: &OAuthToken) -> CredentialHandle {
    let token = token.clone();
    session::transaction(state, move |lease| {
        lease.commit_login(&token).map(|(handle, _)| handle)
    })
    .unwrap()
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    host: Option<String>,
    accounts: BTreeMap<String, StoredToken>,
}

/// Flat disk schema: semantic validation is deliberately separate from typed
/// deserialization. A bad required field makes the entire store unreadable.
#[derive(Serialize, Deserialize)]
struct StoredToken {
    service: String,
    issued: String,
    email: Option<String>,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
}

impl From<&OAuthToken> for StoredToken {
    fn from(token: &OAuthToken) -> Self {
        let mut stored = Self {
            service: token.login().service().name.as_str().to_string(),
            issued: token.issued.clone(),
            email: token.email.clone(),
            access_token: token.access_token.clone(),
            refresh_token: token.refresh_token.clone(),
            expires_at: token.expires_at,
            id_token: None,
            client_id: None,
            account_id: None,
            workspace: None,
            plan: None,
        };
        match &token.grant {
            Grant::Chatgpt {
                id_token,
                client_id,
            } => {
                stored.id_token = Some(id_token.clone());
                stored.client_id = Some(client_id.clone());
            }
            Grant::Codex {
                account_id,
                workspace,
                plan,
            } => {
                stored.account_id = Some(account_id.clone());
                stored.workspace.clone_from(workspace);
                stored.plan.clone_from(plan);
            }
            Grant::Grok => {}
        }
        stored
    }
}

impl TryFrom<StoredToken> for OAuthToken {
    type Error = String;

    fn try_from(stored: StoredToken) -> Result<Self, Self::Error> {
        let login =
            Login::named(&stored.service).map_err(|_| "unknown login service".to_string())?;
        for (name, value) in [
            ("issued", &stored.issued),
            ("access_token", &stored.access_token),
            ("refresh_token", &stored.refresh_token),
        ] {
            if value.is_empty() {
                return Err(format!("empty {name}"));
            }
        }
        let required = |value: Option<String>, name: &str| {
            value
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("missing or empty {name}"))
        };
        let grant = match login {
            Login::Chatgpt => Grant::Chatgpt {
                id_token: required(stored.id_token, "ChatGPT ID token")?,
                client_id: required(stored.client_id, "ChatGPT issued client id")?,
            },
            Login::Codex => Grant::Codex {
                account_id: required(stored.account_id, "Codex backend account id")?,
                workspace: stored.workspace,
                plan: stored.plan,
            },
            Login::Grok => Grant::Grok,
        };
        Ok(Self {
            access_token: stored.access_token,
            refresh_token: stored.refresh_token,
            issued: stored.issued,
            email: stored.email,
            expires_at: stored.expires_at,
            grant,
        })
    }
}

fn read_at(state: &OwnDir) -> Result<(Option<String>, Vec<OAuthToken>), OwnError> {
    let Some(store) = state.load::<Store>(TOKEN_FILE)? else {
        return Ok((None, Vec::new()));
    };
    let tokens = store
        .accounts
        .into_iter()
        .filter_map(|(key, entry)| {
            let converted = OAuthToken::try_from(entry).and_then(|token| {
                if account_id(&token).as_str() == key {
                    Ok(token)
                } else {
                    Err("account key disagrees with the service and issued identity".to_string())
                }
            });
            match converted {
                Ok(token) => Some(token),
                Err(reason) => {
                    ral_core::errln!(
                        "warning: {} entry '{key}': {reason}; dropping it",
                        ral_core::path::shown(state.path().join(TOKEN_FILE))
                    );
                    None
                }
            }
        })
        .collect();
    Ok((store.host, tokens))
}

fn load_all_at(state: &OwnDir) -> Vec<OAuthToken> {
    read_at(state).map_or_else(|_| Vec::new(), |(_, tokens)| tokens)
}

#[cfg(any(test, feature = "test-util"))]
fn save_one_at(state: &OwnDir, token: &OAuthToken) -> Result<bool, String> {
    let token = token.clone();
    session::transaction(state, move |lease| {
        lease.commit_login(&token).map(|(_, replaced)| replaced)
    })
}

fn host_id_at(state: &OwnDir) -> Result<String, String> {
    session::transaction(state, session::StoreLease::host_id)
}

/// `ChatGPT` registration's host identity. Other flows never need to mint it.
///
/// # Errors
/// Returns an error if the store cannot be read or the host cannot be persisted.
pub fn host_id() -> Result<String, String> {
    host_id_at(&state()?)
}

/// Exact full ids win even when another account's handle spells that id.
/// Renewal filters by service before performing either matching pass.
fn matching_indices(tokens: &[OAuthToken], account: &str, login: Option<Login>) -> Vec<usize> {
    let candidates = || {
        tokens
            .iter()
            .enumerate()
            .filter(move |(_, token)| login.is_none_or(|login| token.login() == login))
    };
    let exact: Vec<_> = candidates()
        .filter(|(_, token)| account_id(token).as_str() == account)
        .map(|(i, _)| i)
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    candidates()
        .filter(|(_, token)| token.issued == account || token.handle() == account)
        .map(|(i, _)| i)
        .collect()
}

fn remove_at(state: &OwnDir, account: &str) -> Result<Option<OAuthToken>, String> {
    let account = account.to_string();
    session::transaction(state, move |lease| lease.remove(&account))
}

fn ambiguous(account: &str, action: &str, among: &[&OAuthToken]) -> String {
    format!(
        "'{account}' is ambiguous: it names {} signed-in accounts; {action} by account id instead ({})",
        among.len(),
        among
            .iter()
            .map(|token| account_id(token).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn account_id(token: &OAuthToken) -> AccountId {
    AccountId::of_login(&token.login().service().name, &token.issued)
}

fn write_all_at(state: &OwnDir, host: Option<&str>, all: &[OAuthToken]) -> Result<(), String> {
    let store = Store {
        host: host.map(str::to_string),
        accounts: all
            .iter()
            .map(|token| (account_id(token).to_string(), StoredToken::from(token)))
            .collect(),
    };
    Ok(state.store(TOKEN_FILE, &store, Visibility::Secret)?)
}

/// Read and successfully delete under the same lock before returning a
/// revocation snapshot. Unreadable stores are not silently removed.
fn clear_all_at(state: &OwnDir) -> Result<Vec<OAuthToken>, String> {
    session::transaction(state, session::StoreLease::clear)
}

fn state() -> Result<OwnDir, OwnError> {
    crate::app::EXARCH.own(XdgKind::State)
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
pub(super) struct RefreshResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: Option<u64>,
}

#[derive(Debug)]
struct RefreshUpdate<M> {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    email: Option<String>,
    metadata: M,
}

impl<M> RefreshUpdate<M> {
    fn finish(self, current: &OAuthToken, grant: impl FnOnce(M) -> Grant) -> OAuthToken {
        OAuthToken {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: self.expires_at,
            email: self.email,
            issued: current.issued.clone(),
            grant: grant(self.metadata),
        }
    }
}

async fn refresh(current: &OAuthToken) -> Result<OAuthToken, String> {
    let client = http_client()?;
    match &current.grant {
        Grant::Chatgpt {
            client_id,
            id_token,
        } => chatgpt::refresh(&client, current, client_id, id_token)
            .await
            .map(|update| {
                update.finish(current, |id_token| Grant::Chatgpt {
                    client_id: client_id.clone(),
                    id_token,
                })
            }),
        Grant::Codex {
            account_id,
            workspace,
            plan,
        } => codex::refresh(&client, current).await.map(|update| {
            update.finish(current, |claims| Grant::Codex {
                account_id: account_id.clone(),
                workspace: claims.workspace.or_else(|| workspace.clone()),
                plan: claims.plan.or_else(|| plan.clone()),
            })
        }),
        Grant::Grok => grok::refresh(&client, current)
            .await
            .map(|update| update.finish(current, |()| Grant::Grok)),
    }
}

/// Issuer discovery and revocation retain their independent ten-second bounds.
pub(super) async fn revoke_at(
    discovery_url: &str,
    token: &OAuthToken,
    client_id: &str,
) -> Result<Revocation, String> {
    #[derive(Deserialize)]
    struct Discovery {
        revocation_endpoint: String,
    }
    let client = http_client()?;
    let discovery = client
        .get(discovery_url)
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
            ("client_id", client_id),
        ])
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(Revocation::AtIssuer)
    } else {
        Err(format!("issuer answered {status}"))
    }
}

const DEAD_REFRESH_CODES: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

pub(super) fn refresh_failure(
    status: reqwest::StatusCode,
    body: &str,
    current: &OAuthToken,
) -> String {
    let mut redacted = serde_json::from_str::<serde_json::Value>(body)
        .map_or_else(|_| body.to_string(), |value| value.to_string());
    let id_token = match &current.grant {
        Grant::Chatgpt { id_token, .. } => Some(id_token.as_str()),
        Grant::Codex { .. } | Grant::Grok => None,
    };
    for secret in [
        Some(current.access_token.as_str()),
        Some(current.refresh_token.as_str()),
        id_token,
    ]
    .into_iter()
    .flatten()
    .filter(|secret| !secret.is_empty())
    {
        let quoted = serde_json::to_string(secret).expect("string JSON");
        redacted = redacted
            .replace(&quoted[1..quoted.len() - 1], "[redacted]")
            .replace(secret, "[redacted]");
    }
    let body = redacted.as_str();
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let code = [
        json.get("error").filter(|e| e.is_string()),
        json.pointer("/error/code"),
        json.get("error_code"),
        json.get("code"),
    ]
    .into_iter()
    .flatten()
    .find_map(|code| code.as_str());
    let service = current.login().service().name;
    let handle = format!("{:?}", current.handle());
    if status == reqwest::StatusCode::UNAUTHORIZED
        || code.is_some_and(|code| DEAD_REFRESH_CODES.contains(&code))
    {
        let code = code.unwrap_or("unauthorized");
        format!(
            "the {service} login for {handle} has expired or been revoked ({code}); {} to sign in again",
            renewal_hint(current)
        )
    } else {
        format!(
            "the {service} token refresh for {handle} failed ({status}): {}",
            body_detail(body)
        )
    }
}

fn renewal_hint(token: &OAuthToken) -> String {
    format!(
        "use account {:?} on {}",
        account_id(token).as_str(),
        token.login().service().name
    )
}

pub(super) fn http_client() -> Result<reqwest::Client, String> {
    site!(Silent::OauthClient, reqwest::Client::builder())
        .use_preconfigured_tls(crate::provider::tls::config())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("could not build HTTP client: {e}"))
}

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

pub(super) fn jwt_payload<T: DeserializeOwned>(jwt: &str) -> Result<T, String> {
    let payload = jwt
        .split('.')
        .nth(1)
        .ok_or_else(|| "malformed JWT".to_string())?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| format!("could not decode JWT payload: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("could not parse JWT payload: {e}"))
}

fn cancelled(cancel: &Arc<AtomicBool>) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("sign-in cancelled".to_string())
    } else {
        Ok(())
    }
}

/// Cooperative async wait, measured by elapsed time rather than rounded ticks.
pub(super) async fn wait(duration: Duration, cancel: &Arc<AtomicBool>) -> Result<(), String> {
    let start = tokio::time::Instant::now();
    loop {
        cancelled(cancel)?;
        let remaining = duration.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
        cancelled(cancel)?;
    }
}

pub(crate) fn token_path() -> Option<PathBuf> {
    crate::app::EXARCH
        .xdg_dir(XdgKind::State)
        .map(|dir| dir.join(TOKEN_FILE))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use ral_core::own::TestDir;
    use serde_json::json;

    fn store() -> TestDir {
        OwnDir::for_test("oauth").expect("test state directory")
    }

    fn token(login: Login, issued: &str, email: Option<&str>) -> OAuthToken {
        OAuthToken {
            access_token: format!("at-{login:?}-{issued}"),
            refresh_token: "rt".into(),
            issued: issued.into(),
            email: email.map(str::to_string),
            expires_at: 0,
            grant: match login {
                Login::Chatgpt => Grant::Chatgpt {
                    id_token: "id".into(),
                    client_id: "oaiapp_test".into(),
                },
                Login::Codex => Grant::Codex {
                    account_id: "raw-backend-id".into(),
                    workspace: None,
                    plan: None,
                },
                Login::Grok => Grant::Grok,
            },
        }
    }

    #[test]
    fn methods_and_sign_in_are_service_specific() {
        assert_eq!(methods(Login::Chatgpt), &[Method::Browser]);
        assert_eq!(methods(Login::Codex), &[Method::Browser, Method::Device]);
        assert_eq!(methods(Login::Grok), &[Method::Device]);
        for login in Login::ALL {
            for &method in methods(login) {
                for sign_in in [
                    SignIn::New(login, method),
                    SignIn::Renew(token(login, "same", None), method),
                ] {
                    assert_eq!(sign_in.login(), login);
                    assert_eq!(sign_in.method(), method);
                    assert!(validate_method(login, method).is_ok());
                }
            }
        }
    }

    #[test]
    fn unsupported_or_cancelled_flows_fail_before_runtime_or_state_access() {
        // Calling from inside a runtime would panic if login_flow reached block_on.
        runtime().unwrap().block_on(async {
            let cancel = Arc::new(AtomicBool::new(false));
            let error = login_flow(
                &SignIn::New(Login::Chatgpt, Method::Device),
                |_| panic!("no phase"),
                &cancel,
            )
            .unwrap_err();
            assert!(error.contains("openai-chatgpt") && error.contains("no device-code flow"));
            let error = login_flow(
                &SignIn::New(Login::Grok, Method::Browser),
                |_| panic!("no phase"),
                &cancel,
            )
            .unwrap_err();
            assert!(error.contains("xai-grok") && error.contains("no browser flow"));
            cancel.store(true, Ordering::Relaxed);
            for login in Login::ALL {
                let error = login_flow(
                    &SignIn::New(login, methods(login)[0]),
                    |_| panic!("no phase"),
                    &cancel,
                )
                .unwrap_err();
                assert_eq!(error, "sign-in cancelled");
            }
        });
    }

    #[test]
    fn six_accounts_share_issued_ids_without_overwriting_other_services() {
        let state = store();
        let host = host_id_at(&state).unwrap();
        let mut expected = Vec::new();
        for login in Login::ALL {
            for issued in ["same-1", "same-2"] {
                let token = token(login, issued, Some("same@work"));
                assert!(!save_one_at(&state, &token).unwrap());
                expected.push(token);
            }
        }
        assert_eq!(load_all_at(&state).len(), 6);
        for index in 0..expected.len() {
            let original = &expected[index];
            let fresh = OAuthToken {
                access_token: format!("fresh-{:?}-{}", original.login(), original.issued),
                email: Some("renamed@work".into()),
                ..original.clone()
            };
            assert!(save_one_at(&state, &fresh).unwrap());
            expected[index] = fresh;
            let loaded = load_all_at(&state);
            assert_eq!(loaded.len(), expected.len());
            for other in &expected {
                let found = loaded
                    .iter()
                    .find(|token| account_id(token) == account_id(other))
                    .unwrap();
                assert_eq!(
                    found, other,
                    "updating one full identity preserves every sibling exactly"
                );
            }
        }
        assert_eq!(read_at(&state).unwrap().0.as_deref(), Some(host.as_str()));
        for original in &expected {
            let removed = remove_at(&state, account_id(original).as_str())
                .unwrap()
                .unwrap();
            assert_eq!(account_id(&removed), account_id(original));
        }
        assert!(
            !state.path().join(TOKEN_FILE).exists(),
            "last removal clears host too"
        );
    }

    #[test]
    fn same_account_upserts_even_when_email_changes() {
        for login in Login::ALL {
            let state = store();
            let original = token(login, "one", Some("old@work"));
            assert!(!save_one_at(&state, &original).unwrap());
            let renamed = OAuthToken {
                email: Some("new@work".into()),
                access_token: "new".into(),
                ..original
            };
            assert!(save_one_at(&state, &renamed).unwrap());
            assert_eq!(load_all_at(&state), [renamed]);
            assert!(!save_one_at(&state, &token(login, "two", Some("new@work"))).unwrap());
            assert_eq!(load_all_at(&state).len(), 2);
        }
    }

    #[test]
    fn shared_names_are_ambiguous_and_full_ids_win() {
        let state = store();
        for login in Login::ALL {
            save_one_at(&state, &token(login, "same", Some("same@work"))).unwrap();
        }
        for name in ["same", "same@work"] {
            let error = remove_at(&state, name).unwrap_err();
            assert!(error.contains("ambiguous"), "{error}");
            for login in Login::ALL {
                assert!(error.contains(login.service().name.as_str()));
            }
            assert_eq!(load_all_at(&state).len(), 3);
        }
        let collision = token(Login::Grok, "other", Some("openai-chatgpt:same"));
        save_one_at(&state, &collision).unwrap();
        let removed = remove_at(&state, "openai-chatgpt:same").unwrap().unwrap();
        assert_eq!(removed.login(), Login::Chatgpt);
        assert_eq!(load_all_at(&state).len(), 3);
        assert!(load_all_at(&state).contains(&collision));
    }

    #[test]
    fn renewal_filters_service_before_matching_and_prefers_exact_ids() {
        let tokens = vec![
            token(Login::Chatgpt, "same", Some("same@work")),
            token(Login::Chatgpt, "other", Some("openai-chatgpt:same")),
            token(Login::Codex, "same", Some("same@work")),
            token(Login::Grok, "same", Some("same@work")),
        ];
        for login in Login::ALL {
            let selected = renewal_target(&tokens, login, "same@work").unwrap();
            assert_eq!(selected.login(), login);
        }
        assert_eq!(
            renewal_target(&tokens, Login::Chatgpt, "openai-chatgpt:same")
                .unwrap()
                .issued,
            "same"
        );
        assert!(renewal_target(&tokens, Login::Grok, "openai-chatgpt:same").is_err());
        let mut ambiguous = tokens;
        ambiguous.push(token(Login::Grok, "second", Some("same@work")));
        assert!(
            renewal_target(&ambiguous, Login::Grok, "same@work")
                .unwrap_err()
                .contains("ambiguous")
        );
    }

    #[test]
    fn pure_renewal_checks_the_full_identity_for_every_method() {
        for login in Login::ALL {
            let previous = token(login, "subject:workspace", Some("previous@work"));
            for &method in methods(login) {
                let sign_in = SignIn::Renew(previous.clone(), method);
                let renamed = OAuthToken {
                    email: Some("renamed@work".into()),
                    ..previous.clone()
                };
                check_renewal(&sign_in, &renamed).unwrap();
                let different = OAuthToken {
                    issued: "other-subject:workspace".into(),
                    ..renamed.clone()
                };
                let error = check_renewal(&sign_in, &different).unwrap_err();
                assert!(error.contains("previous@work") && error.contains("renamed@work"));
                let different_workspace = OAuthToken {
                    issued: "subject:other-workspace".into(),
                    ..renamed
                };
                assert!(check_renewal(&sign_in, &different_workspace).is_err());
                for other in Login::ALL.into_iter().filter(|other| *other != login) {
                    assert!(
                        check_renewal(&sign_in, &token(other, &previous.issued, None)).is_err()
                    );
                }
            }
        }
    }

    fn write_json(state: &OwnDir, value: &serde_json::Value) {
        std::fs::write(state.path().join(TOKEN_FILE), value.to_string()).unwrap();
    }

    #[test]
    fn semantic_rejections_drop_only_invalid_entries_and_queries_do_not_rewrite() {
        let state = store();
        let valid = token(Login::Grok, "valid", Some("same@work"));
        let base = serde_json::to_value(StoredToken::from(&token(Login::Chatgpt, "subject", None)))
            .unwrap();
        let mut entries = serde_json::Map::new();
        entries.insert(
            account_id(&valid).to_string(),
            serde_json::to_value(StoredToken::from(&valid)).unwrap(),
        );
        let mut cases = Vec::new();
        for service in ["unknown", "chatgpt"] {
            let mut entry = base.clone();
            entry["service"] = json!(service);
            cases.push(entry);
        }
        for field in [
            "issued",
            "access_token",
            "refresh_token",
            "id_token",
            "client_id",
        ] {
            let mut entry = base.clone();
            entry[field] = json!("");
            cases.push(entry);
        }
        for field in ["id_token", "client_id"] {
            let mut entry = base.clone();
            entry.as_object_mut().unwrap().remove(field);
            cases.push(entry);
        }
        let codex =
            serde_json::to_value(StoredToken::from(&token(Login::Codex, "subject", None))).unwrap();
        for value in [None, Some("")] {
            let mut entry = codex.clone();
            match value {
                Some(value) => {
                    entry["account_id"] = json!(value);
                }
                None => {
                    entry.as_object_mut().unwrap().remove("account_id");
                }
            }
            cases.push(entry);
        }
        for (i, entry) in cases.into_iter().enumerate() {
            entries.insert(format!("invalid:{i}"), entry);
        }
        entries.insert("openai-chatgpt:wrong-key".into(), base.clone());
        entries.insert("chatgpt:subject".into(), base);
        write_json(&state, &json!({"host":"retained-host", "accounts":entries}));
        let file = state.path().join(TOKEN_FILE);
        let before = std::fs::read(&file).unwrap();
        assert_eq!(load_all_at(&state), std::slice::from_ref(&valid));
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert!(save_one_at(&state, &valid).unwrap());
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        assert_eq!(persisted["accounts"].as_object().unwrap().len(), 1);
        assert_eq!(persisted["host"], "retained-host");
    }

    #[test]
    fn canonical_serialization_omits_irrelevant_grant_fields() {
        for login in Login::ALL {
            let original = token(login, "same", None);
            let mut wire = serde_json::to_value(StoredToken::from(&original)).unwrap();
            let expected: &[&str] = match login {
                Login::Chatgpt => &["id_token", "client_id"],
                Login::Codex => &["account_id"],
                Login::Grok => &[],
            };
            for field in ["id_token", "client_id", "account_id", "workspace", "plan"] {
                assert_eq!(wire.get(field).is_some(), expected.contains(&field));
                if !expected.contains(&field) {
                    wire[field] = json!("irrelevant");
                }
            }
            let stored: StoredToken = serde_json::from_value(wire).unwrap();
            let loaded = OAuthToken::try_from(stored).unwrap();
            // Codex workspace and plan are relevant metadata, unlike foreign grant fields.
            if login == Login::Codex {
                assert!(matches!(
                    &loaded.grant,
                    Grant::Codex {
                        workspace: Some(_),
                        plan: Some(_),
                        ..
                    }
                ));
            } else {
                assert_eq!(loaded, original);
            }
            let canonical = serde_json::to_value(StoredToken::from(&loaded)).unwrap();
            if login != Login::Chatgpt {
                assert!(canonical.get("id_token").is_none());
                assert!(canonical.get("client_id").is_none());
            }
            if login != Login::Codex {
                assert!(canonical.get("account_id").is_none());
                assert!(canonical.get("workspace").is_none());
                assert!(canonical.get("plan").is_none());
            }
        }
    }

    #[test]
    fn structurally_unreadable_stores_are_never_replaced() {
        for text in [
            "{ not json".to_string(),
            "[]".to_string(),
            json!({"host":null, "accounts": {"xai-grok:bad": {"service":"xai-grok", "issued":"bad", "access_token":123, "refresh_token":"rt", "expires_at":0}}}).to_string(),
            json!({"host":null, "accounts": {"xai-grok:bad": {"service":"xai-grok", "issued":"bad", "refresh_token":"rt", "expires_at":0}}}).to_string(),
        ] {
            let state = store();
            let file = state.path().join(TOKEN_FILE);
            std::fs::write(&file, &text).unwrap();
            assert!(save_one_at(&state, &token(Login::Grok, "new", None)).is_err());
            assert!(remove_at(&state, "bad").is_err());
            assert!(host_id_at(&state).is_err());
            assert!(clear_all_at(&state).is_err());
            assert_eq!(load_all_at(&state), Vec::<OAuthToken>::new());
            assert_eq!(std::fs::read_to_string(file).unwrap(), text);
        }
    }

    #[test]
    fn host_is_minted_once_and_preserves_every_account() {
        let state = store();
        for login in Login::ALL {
            save_one_at(&state, &token(login, "same", None)).unwrap();
        }
        assert_eq!(
            read_at(&state).unwrap().0,
            None,
            "ordinary writes do not mint a host"
        );
        let host = host_id_at(&state).unwrap();
        assert!(host.starts_with("urn:uuid:"));
        assert_eq!(host_id_at(&state).unwrap(), host);
        assert_eq!(load_all_at(&state).len(), 3);
        let snapshot = clear_all_at(&state).unwrap();
        assert_eq!(snapshot.len(), 3);
        assert!(!state.path().join(TOKEN_FILE).exists());
    }

    #[test]
    fn missing_or_ambiguous_logout_does_not_change_local_records() {
        let state = store();
        for login in Login::ALL {
            save_one_at(&state, &token(login, "same", Some("same@work"))).unwrap();
        }
        let file = state.path().join(TOKEN_FILE);
        let before = std::fs::read(&file).unwrap();
        assert_eq!(remove_at(&state, "absent").unwrap(), None);
        assert!(remove_at(&state, "same@work").is_err());
        assert_eq!(std::fs::read(file).unwrap(), before);
    }

    #[test]
    fn refresh_diagnostics_do_not_echo_credential_fields() {
        let mut current = token(Login::Chatgpt, "subject", Some("name\nnext line"));
        current.access_token = "access-secret".into();
        current.refresh_token = "refresh-secret".into();
        current.grant = Grant::Chatgpt {
            id_token: "id-secret".into(),
            client_id: "client".into(),
        };
        for body in [
            r#"{"error":"access-secret refresh-secret id-secret"}"#,
            r#"{"error":"\u0061ccess-secret refresh-secret id-secret"}"#,
        ] {
            let error = refresh_failure(reqwest::StatusCode::SERVICE_UNAVAILABLE, body, &current);
            for secret in ["access-secret", "refresh-secret", "id-secret"] {
                assert!(!error.contains(secret));
            }
            assert!(!error.contains('\n'));
        }
    }

    #[test]
    fn only_codex_uses_backend_headers_and_request_override() {
        for login in Login::ALL {
            let token = token(login, "local-composite-id", Some("same@work"));
            let headers: BTreeMap<_, _> = token
                .request_headers("application/json")
                .into_iter()
                .collect();
            assert_eq!(
                headers["authorization"],
                format!("Bearer {}", token.access_token)
            );
            assert_eq!(headers["accept"], "application/json");
            match token.auth_data() {
                genai::resolver::AuthData::Key(key) => {
                    assert_ne!(login, Login::Codex);
                    assert_eq!(key, token.access_token);
                    assert_eq!(headers.len(), 2);
                }
                genai::resolver::AuthData::RequestOverride { url, .. } => {
                    assert_eq!(login, Login::Codex);
                    assert_eq!(url, codex::RESPONSES_URL);
                    assert_eq!(headers.len(), 6);
                    assert_eq!(headers["chatgpt-account-id"], "raw-backend-id");
                    assert_eq!(headers["openai-beta"], "responses=experimental");
                    assert_eq!(headers["originator"], codex::ORIGINATOR);
                    assert_eq!(
                        headers["user-agent"],
                        format!("codex_cli_rs/{}", codex::codex_client_version())
                    );
                }
                other => panic!("unexpected auth data: {other:?}"),
            }
        }
    }

    #[test]
    fn codex_revocation_is_local_only_without_a_runtime_or_network() {
        let current = token(Login::Codex, "same", None);
        assert_eq!(revoke_blocking(&current).unwrap(), Revocation::LocalOnly);
        assert_eq!(
            runtime().unwrap().block_on(revoke(&current)).unwrap(),
            Revocation::LocalOnly
        );
    }

    #[test]
    fn shared_email_labels_remain_disambiguated_by_full_account_id() {
        let alone = [token(Login::Chatgpt, "personal", Some("same@work"))];
        let accounts: Vec<_> = alone.iter().map(to_account).collect();
        assert_eq!(
            identity::label(&accounts[0], &accounts),
            "openai-chatgpt · same@work"
        );
        let siblings = [
            token(Login::Chatgpt, "personal", Some("same@work")),
            token(Login::Chatgpt, "team", Some("same@work")),
        ];
        let accounts: Vec<_> = siblings.iter().map(to_account).collect();
        for (account, token) in accounts.iter().zip(&siblings) {
            assert!(identity::label(account, &accounts).ends_with(account_id(token).as_str()));
        }
    }

    #[test]
    fn logout_removes_locally_before_revocation_and_retains_the_snapshot() {
        for all in [false, true] {
            let state = store();
            host_id_at(&state).unwrap();
            let original = token(Login::Codex, "subject", None);
            save_one_at(&state, &original).unwrap();
            let called = std::cell::Cell::new(false);
            let result = logout_at(&state, None, all, |snapshot| {
                called.set(true);
                assert_eq!(snapshot, std::slice::from_ref(&original));
                assert!(!state.path().join(TOKEN_FILE).exists());
                assert_eq!(load_all_at(&state), Vec::<OAuthToken>::new());
            });
            assert!(result.is_ok());
            assert!(called.get());
        }
    }

    #[test]
    fn failed_or_unresolved_local_logout_never_attempts_revocation() {
        let state = store();
        let file = state.path().join(TOKEN_FILE);
        std::fs::write(&file, "not json").unwrap();
        for all in [false, true] {
            assert!(
                logout_at(&state, Some("subject".into()), all, |_| panic!(
                    "no revocation on local failure"
                ))
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "not json");
        }
        std::fs::remove_file(file).unwrap();
        for login in Login::ALL {
            save_one_at(&state, &token(login, "shared", Some("same@work"))).unwrap();
        }
        for target in [
            None,
            Some("missing".into()),
            Some("shared".into()),
            Some("same@work".into()),
        ] {
            assert!(
                logout_at(&state, target, false, |_| panic!(
                    "no revocation without a unique target"
                ))
                .is_err()
            );
            assert_eq!(load_all_at(&state).len(), 3);
        }
    }

    /// A finite local discovery/revocation server records method, path and form.
    async fn revocation_fixture(status: u16) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let endpoint = format!("{base}/revoke");
        let worker = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in [
                (200, json!({"revocation_endpoint": endpoint}).to_string()),
                (status, "{}".to_string()),
            ] {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .expect("local revocation request")
                        .unwrap();
                let mut bytes = Vec::new();
                let end = loop {
                    let mut chunk = [0u8; 1024];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|chunk| chunk == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let head = std::str::from_utf8(&bytes[..end]).unwrap();
                let length = head
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
                while bytes.len() - end < length {
                    let mut chunk = [0u8; 1024];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                requests.push(String::from_utf8(bytes).unwrap());
                socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            requests
        });
        (format!("{base}/discovery"), worker)
    }

    #[test]
    fn revocation_uses_discovery_and_refresh_token_forms_after_local_success() {
        for login in [Login::Chatgpt, Login::Grok] {
            for status in [200, 503] {
                let state = store();
                let original = token(login, "subject", None);
                save_one_at(&state, &original).unwrap();
                let rt = runtime().unwrap();
                let (discovery, worker) = rt.block_on(revocation_fixture(status));
                let client_id = if login == Login::Chatgpt {
                    "oaiapp_test"
                } else {
                    "grok-public-client"
                };
                let result = logout_at(&state, None, false, |snapshot| {
                    assert!(
                        !state.path().join(TOKEN_FILE).exists(),
                        "local removal precedes HTTP"
                    );
                    let revoked = rt.block_on(revoke_at(&discovery, &snapshot[0], client_id));
                    if status == 200 {
                        assert_eq!(revoked.unwrap(), Revocation::AtIssuer);
                    } else {
                        assert!(revoked.unwrap_err().contains("503"));
                    }
                });
                assert!(result.is_ok(), "remote failure cannot undo local success");
                let requests = rt.block_on(worker).unwrap();
                assert!(requests[0].starts_with("GET /discovery HTTP/1.1\r\n"));
                assert!(requests[1].starts_with("POST /revoke HTTP/1.1\r\n"));
                assert!(requests[1].contains("application/x-www-form-urlencoded"));
                let body = requests[1].split_once("\r\n\r\n").unwrap().1;
                let form: BTreeMap<_, _> = reqwest::Url::parse(&format!("http://fixture/?{body}"))
                    .unwrap()
                    .query_pairs()
                    .into_owned()
                    .collect();
                assert_eq!(
                    form,
                    BTreeMap::from([
                        ("token".to_string(), "rt".to_string()),
                        ("token_type_hint".to_string(), "refresh_token".to_string()),
                        ("client_id".to_string(), client_id.to_string())
                    ])
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wait_observes_cancellation_between_ticks_and_zero_duration() {
        let cancel = Arc::new(AtomicBool::new(false));
        wait(Duration::ZERO, &cancel).await.unwrap();
        let flag = Arc::clone(&cancel);
        let trigger = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            flag.store(true, Ordering::Relaxed);
        });
        let error = wait(Duration::from_mins(15), &cancel).await.unwrap_err();
        assert_eq!(error, "sign-in cancelled");
        trigger.await.unwrap();
        assert_eq!(
            wait(Duration::ZERO, &cancel).await.unwrap_err(),
            "sign-in cancelled"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_preserves_the_requested_elapsed_duration() {
        let start = tokio::time::Instant::now();
        wait(
            Duration::from_millis(250),
            &Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        assert_eq!(start.elapsed(), Duration::from_millis(250));
    }

    #[test]
    fn phase_messages_preserve_device_code_and_exchange_silence() {
        let line = LoginPhase::AwaitingDevice {
            user_code: "ABCD-EFGH".into(),
            url: "https://example.test/device".into(),
            expires_in: "15 minutes".into(),
        }
        .stderr_line()
        .unwrap();
        assert_eq!(
            line,
            "To sign in, open https://example.test/device and enter this code (expires in 15 minutes):\n  ABCD-EFGH"
        );
        assert!(LoginPhase::ExchangingCode.stderr_line().is_none());
    }
}
