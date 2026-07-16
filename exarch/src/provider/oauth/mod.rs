//! "Sign in with ChatGPT": ChatGPT-plan accounts as OAuth-backed providers.
//!
//! A `ChatGPT` plan subscription is authorised through `OpenAI`'s OAuth issuer
//! rather than an API key. Two interactive flows obtain the initial token:
//! a browser redirect to a loopback listener ([`browser`]), and a device
//! code typed into a verification page ([`device`]). Both end in an
//! authorization-code exchange against the token endpoint, yielding an
//! `id_token`, an `access_token`, and a `refresh_token`. The `access_token` is a
//! JWT whose `exp` claim sets the expiry; the `id_token` carries the `ChatGPT`
//! account id and the login email.
//!
//! Several accounts can be signed in at once. The store ([`load_all`] /
//! [`save_one`] / [`remove`]) holds a list of [`OAuthToken`]s keyed by
//! account id, persisted under the XDG state directory and reloaded on later
//! runs; each becomes a selectable [`crate::provider::ProviderId::ChatGpt`]
//! in the credential store. The provider refreshes a token through
//! [`refresh`] when it is near expiry, upserting it back into the store so a
//! refresh never disturbs the other accounts.

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

const ISSUER: &str = "https://auth.openai.com";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// A persisted `ChatGPT` OAuth token.
///
/// The access token is a short-lived JWT;
/// the refresh token mints fresh access tokens once it expires. Several of
/// these can be stored at once — one per signed-in `ChatGPT` account — keyed
/// by [`Self::account_id`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    /// The account's login email, from the `id_token`'s `email` claim. The
    /// human-readable handle the picker and `/model` switch select by;
    /// `None` when the `id_token` carried none, in which case the opaque
    /// [`Self::account_id`] stands in (see [`Self::label`]).
    pub email: Option<String>,
    /// Unix seconds at which `access_token` expires (its JWT `exp`).
    pub expires_at: u64,
}

impl OAuthToken {
    /// True when the access token has expired or is within 60s of expiring.
    pub fn is_stale(&self) -> bool {
        self.expires_at <= crate::bootstrap::now_secs() + 60
    }

    /// The account's stable, human-readable handle: its login email, or the
    /// opaque account id when no email was issued. This is the label exarch's
    /// selection layer keys the account by — the picker row, the persisted
    /// selection, and the `logout`/`accounts` commands all read it.
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

/// Run interactive login (browser flow, or device code when `device`), then
/// persist the token. Progress is printed to stderr.
///
/// # Errors
/// Returns `Err` if the tokio runtime or HTTP client cannot be built, if the
/// browser/device flow fails, or if finalising or persisting the token fails.
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
    let replaced = save_one(&token)?;
    let verb = if replaced {
        "Updated the login for"
    } else {
        "Signed in to"
    };
    eprintln!("{verb} ChatGPT account {}.", token.label());
    Ok(())
}

/// Remove one signed-in account (matched by its label or account id), or
/// every account when `all`.
///
/// With neither an account nor `--all`: removes the
/// sole account when exactly one is signed in, and otherwise errors asking
/// which — so a stray `logout` cannot silently drop the wrong account when
/// several are present. Progress is printed to stderr.
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

/// Every signed-in account's label, comma-joined — for the disambiguating
/// messages `logout`/`accounts` print.
fn labels(accounts: &[OAuthToken]) -> String {
    accounts
        .iter()
        .map(OAuthToken::label)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every persisted `ChatGPT` login, in stored order. An absent or unparseable
/// store yields an empty list.
pub fn load_all() -> Vec<OAuthToken> {
    load_all_at(&token_path())
}

/// Persist `token`, replacing any existing login for the same account id and
/// appending it otherwise. Returns whether an existing account was replaced.
pub(crate) fn save_one(token: &OAuthToken) -> Result<bool, String> {
    save_one_at(&token_path(), token)
}

/// Remove the login matched by `account` (its label or account id). Returns
/// the removed account's label, or `None` when nothing matched.
pub(crate) fn remove(account: &str) -> Result<Option<String>, String> {
    remove_at(&token_path(), account)
}

// The storage core is a pure function of a path, so it is exercised in tests
// against a temp file without mutating the process environment.

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
    let mut all = load_all_at(path);
    let replaced = if let Some(existing) = all.iter_mut().find(|t| t.account_id == token.account_id) {
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
    let mut all = load_all_at(path);
    let Some(pos) = all
        .iter()
        .position(|t| t.account_id == account || t.label() == account)
    else {
        return Ok(None);
    };
    let removed = all.remove(pos);
    // Delete the store outright when the last account goes, so a fully
    // logged-out machine carries no empty file.
    if all.is_empty() {
        clear_at(path)?;
    } else {
        write_all_at(path, &all)?;
    }
    Ok(Some(removed.label()))
}

/// Write the whole account set to `path`, creating its dir as needed. On Unix
/// the file is created with mode 0600.
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

/// Delete the whole store at `path`. Succeeds when no store is present.
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

/// Exchange the refresh token for a fresh [`OAuthToken`].
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
    // The refresh response may omit the id_token; keep the account's existing
    // identity (id and email) when it does, so a refresh never loses the label.
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
/// Both inference and catalog requests enter through this door, so neither can
/// accidentally authenticate with a stale token merely because it happened
/// first in a session. Persistence is best-effort: a fresh in-memory token is
/// immediately useful even when the state directory cannot be written.
pub(crate) async fn refresh_cell_if_stale(
    cell: &std::sync::Arc<std::sync::Mutex<OAuthToken>>,
) -> Result<(), String> {
    let current = {
        let token = cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !token.is_stale() {
            return Ok(());
        }
        token.clone()
    };
    let fresh = refresh(&current).await?;
    let _ = save_one(&fresh);
    *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh;
    Ok(())
}

/// The headers a Codex-backend model request carries for this token,
/// returned as lowercase `(name, value)` pairs.
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
            format!("codex_cli_rs/{}", env!("CARGO_PKG_VERSION")),
        ),
        ("accept".into(), accept.into()),
    ]
}

fn token_endpoint() -> String {
    format!("{ISSUER}/oauth/token")
}

/// The HTTP client shared by the login flows and refresh.
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .use_preconfigured_tls(crate::provider::tls::config())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("could not build HTTP client: {e}"))
}

/// Turn a token-endpoint response into a decoded `T`: a non-2xx status
/// becomes an `Err` carrying the status and the body text, a success decodes
/// the JSON body. `action` names the request for both messages
/// (`"{action} failed (…)"` / `"could not parse {action} response"`). Shared
/// by every login flow and by [`refresh`].
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
    json_or_error(resp, "token exchange").await
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
    getrandom::fill(&mut bytes).expect("OS randomness");
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
    /// The standard OIDC `email` claim — the account's login email, used as
    /// its human-readable label. Requested via the `email` scope.
    email: Option<String>,
}

#[derive(Deserialize)]
struct ExpClaims {
    exp: Option<i64>,
}

/// The `ChatGPT` account id carried by an `id_token`'s auth claim.
fn account_id_from_jwt(jwt: &str) -> Option<String> {
    jwt_payload::<IdClaims>(jwt).ok()?.auth?.chatgpt_account_id
}

/// The login email carried by an `id_token`'s standard `email` claim.
fn email_from_jwt(jwt: &str) -> Option<String> {
    jwt_payload::<IdClaims>(jwt).ok()?.email
}

/// The `exp` claim of a JWT, as unix seconds.
fn jwt_exp(jwt: &str) -> Option<u64> {
    let exp = jwt_payload::<ExpClaims>(jwt).ok()?.exp?;
    u64::try_from(exp).ok()
}

/// When the access token expires, as unix seconds: its own JWT `exp`, then the
/// `id_token`'s, then one hour out when neither carries one.
fn expiry_secs(access_token: &str, id_token: Option<&str>) -> u64 {
    jwt_exp(access_token)
        .or_else(|| id_token.and_then(jwt_exp))
        .unwrap_or_else(|| crate::bootstrap::now_secs() + 3600)
}

/// Turn a token-endpoint response into a persisted [`OAuthToken`]. The
/// account id comes from the `id_token`; the expiry is the access token's
/// `exp`, falling back to the `id_token`'s `exp`, then to one hour out.
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

/// Create `path` with an owner-only DACL already in force, then write
/// `bytes` to it — the Windows analogue of the Unix arm's mode `0600` at
/// `open`.  The DACL (one explicit ACE granting the current process's
/// owner full control, no inherited ACEs, built via `SetEntriesInAclW`) is
/// carried in the `SECURITY_ATTRIBUTES` passed to `CreateFileW` itself, so
/// the file never exists with the parent directory's inherited ACL even
/// for an instant — unlike stamping a DACL on afterwards, there is no
/// window and no permanently-over-permissioned file if a later step fails.
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
        CREATE_ALWAYS, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL, CreateFileW,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The only security-descriptor revision Win32 defines
    /// (`SECURITY_DESCRIPTOR_REVISION` in `winnt.h`, stable since NT) — not
    /// worth a whole extra `windows-sys` feature for one ABI constant.
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

    /// RAII wrapper closing a process token handle exactly once, so every
    /// early return below (a query failure, an ACL-build failure) doesn't
    /// need its own `CloseHandle` call.
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

    /// Fetch the current process's owner SID as an owned buffer (a
    /// `TOKEN_USER` followed inline by its SID bytes, per
    /// `GetTokenInformation`'s contract): query once to size the buffer,
    /// once more to fill it.
    fn current_process_owner() -> std::io::Result<Vec<u8>> {
        use windows_sys::Win32::Security::GetTokenInformation;

        let mut raw_token: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle needing no
        // close; `raw_token` is an out-param this call fills on success.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let token = OwnedToken(raw_token);

        let mut needed: u32 = 0;
        // SAFETY: a null buffer with zero length is the documented way to
        // size a `GetTokenInformation` query; `needed` is filled with the
        // required size regardless of the (expected) failure.
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = vec![0u8; needed as usize];
        // SAFETY: `buf` is sized exactly to `needed`, as just reported by
        // the sizing call above.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(buf)
    }

    /// Build a single-ACE ACL granting the current process's owner full
    /// control — the same access an owner-only file would have — and no
    /// other entries, so nothing the parent directory would otherwise hand
    /// down survives.  The caller wraps it in a security descriptor and
    /// frees it after `CreateFileW` returns (the kernel copies the
    /// descriptor's contents into the new file object at creation time, so
    /// nothing here needs to outlive that call).
    fn owner_only_acl() -> std::io::Result<*mut ACL> {
        let owner_buf = current_process_owner()?;
        // SAFETY: `owner_buf` holds a fully-populated `TOKEN_USER` from the
        // successful `GetTokenInformation` call above; `owner_buf` outlives
        // every use of the `PSID` it hands out below.
        let owner_sid = unsafe { (*owner_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };

        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            // The trustee-by-SID form reinterprets this field as the PSID
            // pointer per the Win32 contract; both are raw `*mut _` under
            // windows-sys, so the cast is a bit-preserving reinterpret (see
            // `core::sandbox::windows::dacl::trustee_for` for the same
            // pattern).
            ptstrName: owner_sid as *mut u16,
        };
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: trustee,
        };

        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `ea` outlives the call; passing a null prior ACL builds a
        // fresh ACL containing only this one entry — no merge with
        // whatever the parent directory would otherwise hand down.
        let rc = unsafe { SetEntriesInAclW(1, &ea, std::ptr::null(), &mut acl) };
        if rc != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        Ok(acl)
    }

    /// Create `path` (truncating any existing file, matching the Unix
    /// arm's `create + truncate`) with an owner-only DACL already in
    /// force: one explicit ACE granting the current process's owner full
    /// control, no inherited ACEs, `SE_DACL_PROTECTED` so the object never
    /// picks up inheritable ACEs from the parent either.  Returns the open
    /// file for the caller to write through — there is no separate "stamp
    /// the DACL" step, so no window exists where the file carries whatever
    /// the parent directory would otherwise hand down.
    pub(super) fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
        let acl = owner_only_acl()?;

        let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let sd_ptr: PSECURITY_DESCRIPTOR = std::ptr::addr_of_mut!(sd).cast();
        let result = (|| -> std::io::Result<std::fs::File> {
            // SAFETY: `sd` is a local, stack-allocated `SECURITY_DESCRIPTOR`
            // that `InitializeSecurityDescriptor` fills in place.
            if unsafe { InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `sd` was just initialised; `acl` is a live ACL from
            // `SetEntriesInAclW` that outlives this call (freed below,
            // after `CreateFileW` has copied the descriptor's contents).
            if unsafe { SetSecurityDescriptorDacl(sd_ptr, TRUE, acl, FALSE) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Mark the DACL protected so `CreateFileW` does not merge in
            // any inheritable ACEs from the parent directory — the same
            // guarantee `PROTECTED_DACL_SECURITY_INFORMATION` gave the
            // old stamp-after-write path, applied at creation instead.
            // SAFETY: `sd` carries the DACL just attached above.
            if unsafe { SetSecurityDescriptorControl(sd_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED) }
                == 0
            {
                return Err(std::io::Error::last_os_error());
            }

            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd_ptr,
                bInheritHandle: FALSE,
            };
            let path_w: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: `path_w` is NUL-terminated and kept alive for the
            // call; `sa` carries the owner-only, protected descriptor just
            // built, so the file is created with that DACL atomically —
            // no window where a default or inherited DACL applies.
            let handle = unsafe {
                CreateFileW(
                    path_w.as_ptr(),
                    GENERIC_WRITE,
                    0,
                    &sa,
                    CREATE_ALWAYS,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `handle` is a just-created, valid file handle opened
            // for `GENERIC_WRITE` with no other owner.
            Ok(unsafe { std::fs::File::from_raw_handle(handle as std::os::windows::io::RawHandle) })
        })();

        // SAFETY: `acl` was allocated by `SetEntriesInAclW` (which uses
        // `LocalAlloc` internally); free it exactly once, regardless of
        // which step above failed or succeeded — `CreateFileW` (if
        // reached) has already copied the descriptor's contents into the
        // new file object by the time this runs.
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

    /// An account labels by its email, falling back to the opaque account id
    /// when the `id_token` carried none.
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

    /// `save_one` upserts by account id: a second account appends, and
    /// re-saving an existing account replaces its tokens in place rather than
    /// duplicating it. `remove` drops one account by label and deletes the
    /// store only once the last account is gone. Exercised against a temp file
    /// so no process-environment mutation is involved.
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

        // Re-saving acc_a replaces its token rather than duplicating it.
        assert!(
            save_one_at(&path, &tok("acc_a", "a@x", "at2")).unwrap(),
            "existing replaced"
        );
        let all = load_all_at(&path);
        assert_eq!(all.len(), 2, "upsert, not append");
        let a = all.iter().find(|t| t.account_id == "acc_a").unwrap();
        assert_eq!(a.access_token, "at2", "token updated in place");

        // Remove by label leaves the other account and keeps the file.
        assert_eq!(remove_at(&path, "a@x").unwrap().as_deref(), Some("a@x"));
        assert_eq!(load_all_at(&path), vec![tok("acc_b", "b@x", "at1")]);
        // Removing the last account deletes the store entirely.
        assert_eq!(remove_at(&path, "acc_b").unwrap().as_deref(), Some("b@x"));
        assert!(!path.exists(), "empty store is removed, not left as []");
        assert!(
            remove_at(&path, "acc_b").unwrap().is_none(),
            "no match → None"
        );
        let _ = std::fs::remove_file(&path);
    }

}
