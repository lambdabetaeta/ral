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
    /// `opened` is false when the platform launcher failed and the user must
    /// open `url` by hand.
    AwaitingBrowser {
        url: String,
        opened: bool,
    },
    AwaitingDevice {
        user_code: String,
        url: String,
        expires_in: String,
    },
    ExchangingCode,
}

impl LoginPhase {
    /// The CLI adapter's stderr line, `None` for `ExchangingCode`. The
    /// launch-failure text cannot name the underlying `open`/`xdg-open`
    /// error: `browser::open_browser` discards it before this seam.
    pub fn stderr_line(&self) -> Option<String> {
        match self {
            Self::AwaitingBrowser { opened: true, .. } => {
                Some("Waiting for sign-in to complete...".to_string())
            }
            Self::AwaitingBrowser { url, opened: false } => Some(format!(
                "could not open a browser automatically\nOpen this URL in your browser to sign in:\n  {url}\nWaiting for sign-in to complete..."
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

fn token_path() -> PathBuf {
    crate::bootstrap::EXARCH
        .xdg_dir(ral_core::path::basedir::XdgKind::State)
        .join("oauth.json")
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

/// The Windows analogue of the Unix arm's `0600` at `open`: the owner-only
/// DACL rides in on `CreateFileW`'s `SECURITY_ATTRIBUTES`, so the file never
/// exists under the parent directory's inherited ACL, not even for an instant.
#[cfg(windows)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-write-windows] creates the OAuth token file with an owner-only DACL already in force and writes it; credential store infra, not turn-time data I/O"
)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = windows_dacl::create_owner_only(path)?;
    file.write_all(bytes)
}

#[cfg(windows)]
mod windows_dacl {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_SUCCESS, FALSE, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
        TRUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SetEntriesInAclW, TRUSTEE_IS_SID,
        TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACL, InitializeSecurityDescriptor, NO_INHERITANCE, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_ALWAYS, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The only revision Win32 defines (`winnt.h`), restated rather than
    /// pulling in another `windows-sys` feature for one ABI constant.
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

    #[allow(
        clippy::cast_possible_truncation,
        reason = "a fixed Win32 layout of 24 bytes; size_of is a compile-time constant nowhere near u32::MAX"
    )]
    const SECURITY_ATTRIBUTES_LENGTH: u32 = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;

    /// Closes a process token handle exactly once, so the early returns below
    /// need no `CloseHandle` of their own.
    struct OwnedToken(HANDLE);

    impl Drop for OwnedToken {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is a handle this guard owns exclusively,
                // obtained from a successful `OpenProcessToken`.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    /// The current process's owner SID, in the layout `GetTokenInformation`
    /// hands back: a `TOKEN_USER` followed inline by its SID bytes.
    ///
    /// The buffer counts `u64`s though the API counts bytes, because the
    /// caller reads a pointer-leading `TOKEN_USER` back out of it. `Vec<u8>`
    /// is byte-aligned *as a type*; counting in `u64` makes that read sound
    /// by construction rather than by allocator luck.
    fn current_process_owner() -> std::io::Result<Vec<u64>> {
        use windows_sys::Win32::Security::GetTokenInformation;

        let mut raw_token: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle needing no
        // close; `raw_token` is an out-param this call fills on success.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw_token) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let token = OwnedToken(raw_token);

        let mut needed: u32 = 0;
        // SAFETY: a null buffer of zero length is the documented sizing
        // query; `needed` is filled despite the expected failure.
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &raw mut needed);
        }
        if needed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        // SAFETY: `buf` holds at least the `needed` bytes the sizing call
        // reported, rounded up to whole `u64`s.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &raw mut needed,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(buf)
    }

    /// A single-ACE ACL granting the process owner full control and nobody
    /// else anything. The caller owns the allocation and frees it once
    /// `CreateFileW` has copied the descriptor into the new file object.
    fn owner_only_acl() -> std::io::Result<*mut ACL> {
        let owner_buf = current_process_owner()?;
        // SAFETY: `owner_buf` holds a populated `TOKEN_USER` in a `u64`
        // buffer, so aligned for it, and outlives every use of the `PSID`.
        let owner_sid = unsafe { (*owner_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };

        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            // The trustee-by-SID form reinterprets this field as the PSID;
            // both are raw `*mut _` here, so the cast preserves bits.
            // `trustee_for` in ral-core's `sandbox::windows::dacl` does the same.
            ptstrName: owner_sid.cast::<u16>(),
        };
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: trustee,
        };

        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `ea` outlives the call; a null prior ACL builds a fresh one
        // holding only this entry, merging in nothing inherited.
        let rc = unsafe { SetEntriesInAclW(1, &raw const ea, std::ptr::null(), &raw mut acl) };
        if rc != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(rc.cast_signed()));
        }
        Ok(acl)
    }

    /// Create `path`, truncating as the Unix arm does, with the owner-only
    /// DACL and `SE_DACL_PROTECTED` already in force. Returns the open file
    /// for the caller to write through: there is no separate stamping step,
    /// hence no window in which the file carries an inherited ACL.
    pub(super) fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
        let acl = owner_only_acl()?;

        let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let sd_ptr: PSECURITY_DESCRIPTOR = std::ptr::addr_of_mut!(sd).cast();
        let result = (|| -> std::io::Result<std::fs::File> {
            // SAFETY: `sd` is a stack-allocated `SECURITY_DESCRIPTOR` that
            // `InitializeSecurityDescriptor` fills in place.
            if unsafe { InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `sd` was just initialised; `acl` outlives this call,
            // freed only after `CreateFileW` has copied the descriptor.
            if unsafe { SetSecurityDescriptorDacl(sd_ptr, TRUE, acl, FALSE) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Protecting the DACL is what stops `CreateFileW` merging in the
            // parent directory's inheritable ACEs.
            // SAFETY: `sd` carries the DACL just attached above.
            if unsafe { SetSecurityDescriptorControl(sd_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED) }
                == 0
            {
                return Err(std::io::Error::last_os_error());
            }

            let sa = SECURITY_ATTRIBUTES {
                nLength: SECURITY_ATTRIBUTES_LENGTH,
                lpSecurityDescriptor: sd_ptr,
                bInheritHandle: FALSE,
            };
            let path_w: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: `path_w` is NUL-terminated and alive for the call; `sa`
            // carries the descriptor just built, so the file is created with
            // that DACL atomically.
            let handle = unsafe {
                CreateFileW(
                    path_w.as_ptr(),
                    GENERIC_WRITE,
                    0,
                    &raw const sa,
                    CREATE_ALWAYS,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `handle` is a just-created file handle opened for
            // `GENERIC_WRITE` with no other owner.
            Ok(
                unsafe {
                    std::fs::File::from_raw_handle(handle as std::os::windows::io::RawHandle)
                },
            )
        })();

        // SAFETY: `acl` came from `SetEntriesInAclW`, which allocates through
        // `LocalAlloc`; freeing it exactly once on every path is safe because
        // `CreateFileW`, if reached, has already copied the descriptor.
        unsafe {
            if !acl.is_null() {
                LocalFree(acl.cast());
            }
        }
        result
    }
}

#[cfg(not(any(unix, windows)))]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:token-write-nonunix] oauth token store write with no owner-only mode available on this platform; credential persistence infrastructure, not turn-time model data I/O."
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
            br#"{"exp":1893456000,"email":"alex@work","https://api.openai.com/auth":{"chatgpt_account_id":"acc_123"}}"#,
        );
        let jwt = format!("hdr.{payload}.sig");
        assert_eq!(account_id_from_jwt(&jwt).as_deref(), Some("acc_123"));
        assert_eq!(email_from_jwt(&jwt).as_deref(), Some("alex@work"));
        assert_eq!(jwt_exp(&jwt), Some(1_893_456_000));
    }

    #[test]
    fn label_prefers_email_then_account_id() {
        let with_email = OAuthToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: "acc_123".into(),
            email: Some("alex@work".into()),
            expires_at: 0,
        };
        let no_email = OAuthToken {
            email: None,
            ..with_email.clone()
        };
        assert_eq!(with_email.label(), "alex@work");
        assert_eq!(no_email.label(), "acc_123");
    }

    /// The upsert-by-account-id contract multiple logins depend on: re-saving
    /// replaces in place, and the file goes only when the last account does.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:test] temp-file scaffolding"
    )]
    fn save_one_upserts_and_remove_targets_one_account() {
        let path = std::env::temp_dir().join(format!(
            "exarch-oauth-store-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_file(&path);
        let tok = |acc: &str, email: &str, at: &str| OAuthToken {
            access_token: at.into(),
            refresh_token: "rt".into(),
            account_id: acc.into(),
            email: Some(email.into()),
            expires_at: 0,
        };

        assert!(load_all_at(&path).is_empty(), "absent store is empty");
        assert!(
            !save_one_at(&path, &tok("acc_a", "a@x", "at1")).unwrap(),
            "first is new"
        );
        assert!(
            !save_one_at(&path, &tok("acc_b", "b@x", "at1")).unwrap(),
            "second is new"
        );
        assert_eq!(load_all_at(&path).len(), 2, "two accounts coexist");

        assert!(
            save_one_at(&path, &tok("acc_a", "a@x", "at2")).unwrap(),
            "existing replaced"
        );
        let all = load_all_at(&path);
        assert_eq!(all.len(), 2, "upsert, not append");
        let a = all.iter().find(|t| t.account_id == "acc_a").unwrap();
        assert_eq!(a.access_token, "at2", "token updated in place");

        assert_eq!(remove_at(&path, "a@x").unwrap().as_deref(), Some("a@x"));
        assert_eq!(load_all_at(&path), vec![tok("acc_b", "b@x", "at1")]);
        assert_eq!(remove_at(&path, "acc_b").unwrap().as_deref(), Some("b@x"));
        assert!(!path.exists(), "empty store is removed, not left as []");
        assert!(
            remove_at(&path, "acc_b").unwrap().is_none(),
            "no match → None"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Pins the user-facing sign-in copy verbatim, so it cannot drift as an
    /// incidental consequence of touching `LoginPhase` or its callers.
    #[test]
    fn stderr_line_reproduces_legacy_messages() {
        assert_eq!(
            LoginPhase::AwaitingBrowser {
                url: "https://auth.openai.com/oauth/authorize?…".into(),
                opened: true,
            }
            .stderr_line()
            .as_deref(),
            Some("Waiting for sign-in to complete...")
        );
        assert_eq!(
            LoginPhase::AwaitingBrowser {
                url: "https://auth.openai.com/oauth/authorize?…".into(),
                opened: false,
            }
            .stderr_line()
            .as_deref(),
            Some(
                "could not open a browser automatically\nOpen this URL in your browser to sign \
                 in:\n  https://auth.openai.com/oauth/authorize?…\nWaiting for sign-in to complete..."
            )
        );
        assert_eq!(
            LoginPhase::AwaitingDevice {
                user_code: "ABCD-1234".into(),
                url: format!("{ISSUER}/codex/device"),
                expires_in: "15 minutes".into(),
            }
            .stderr_line()
            .as_deref(),
            Some(
                "To sign in, open https://auth.openai.com/codex/device and enter this code \
                 (expires in 15 minutes):\n  ABCD-1234"
            )
        );
        assert_eq!(LoginPhase::ExchangingCode.stderr_line(), None);
    }
}
