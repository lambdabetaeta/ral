//! Synod's own provider accounts: which services it can talk to, and what
//! it authenticates to each of them with.
//!
//! Synod is a desktop application, so keys are typed into a window and
//! kept in this computer's credential manager
//! ([`exarch::provider::keychain`]).  Per account, the credential manager
//! wins — it is the one source a person can see and change from inside
//! synod — with the environment as fallback, resolved and scrubbed by
//! [`CredentialStore::resolve_and_scrub`] exactly as in exarch.  A key
//! from the environment is never silently written into the vault.
//!
//! Which services *exist* is a third thing, and not a secret: the built-in
//! ones are [`exarch::provider::identity::built_in_services`]'s own list,
//! and any further endpoint is declared in
//! `$XDG_CONFIG_HOME/synod/providers.ral`, written by the accounts screen
//! and read by exarch's one declaration decoder ([`exarch::config`]). It
//! holds addresses, never keys.
//!
//! What is provider knowledge — a service's identity, an account's id, the
//! built-in table — lives in [`exarch::provider::accounts`] and
//! [`exarch::provider::identity`]; this module holds only what is about
//! synod's own window: the row a screen draws, where a key is kept, and
//! whether a service can be withdrawn outright.

use exarch::config;
use exarch::provider::accounts::{
    checked_key, declare_endpoint, declared_endpoints, find, withdraw_endpoint,
};
use exarch::provider::credential::{Credential, CredentialStore, NO_AUTH_PLACEHOLDER};
use exarch::provider::identity::{self, Auth, Service};
use exarch::provider::keychain::Keychain;
use exarch::provider::models::{LiveSource, ModelCatalog};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::session::SYNOD;

const DECLARATIONS_FILE: &str = "providers.ral";

/// How the declarations file names itself when it has a complaint to make.
const LABEL: &str = "provider settings";

const KEYCHAIN: Keychain = Keychain::for_app(SYNOD);

fn declarations_path() -> PathBuf {
    SYNOD
        .xdg_dir(ral_core::path::basedir::XdgKind::Config)
        .join(DECLARATIONS_FILE)
}

/// Resolve every account this computer offers, once, at startup.
///
/// The environment sweep first, then the credential manager laid over the
/// top ([`CredentialStore::admit_from`]), so a key typed into the accounts
/// screen outranks a stale variable in the launching environment.
///
/// # Errors
/// Returns `Err` if the declarations file is present but cannot be read or
/// makes no sense; a vault that cannot be reached is not an error here,
/// only fewer accounts to choose from.
///
/// # Panics
/// Must be called while the process is still single-threaded: the
/// credential scrub mutates the environment.
pub fn prepare() -> Result<CredentialStore, String> {
    let declared = config::load_declared(&declarations_path(), LABEL)?;
    let mut store = CredentialStore::resolve_and_scrub(declared);
    store.admit_from(&KEYCHAIN);
    Ok(store)
}

/// Where a row's credential in force actually came from.
///
/// The one fact the screen needs that the shared [`CredentialStore`] does not
/// already carry on its face, since only the store knows which of its doors a
/// key came through.
#[derive(serde::Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// No credential at all — the row is known but keyless.
    None,
    /// A `ChatGPT` login: the one case a key is never typed, and the sign-in
    /// affordance replaces the key form.
    SignedIn,
    /// The inert bearer a keyless local server is bound to — a state, not a
    /// key anyone typed, so its tail must never be shown as one.
    NoKey,
    Keychain,
    Environment,
}

/// One row of the accounts screen.
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// The identifier command payloads name this row by — an `AccountId`
    /// rendering, resolved back through [`exarch::provider::accounts::find`].
    /// Never shown; [`Self::label`] is what the screen draws.
    pub id: String,
    /// This account's name among every account currently known —
    /// [`identity::label`], set-relative, so a shared login email still
    /// tells two accounts apart.
    pub label: String,
    pub source: Source,
    /// The last four characters of the key in force, never more.
    pub hint: Option<String>,
    /// The environment variable this account would read from, for the
    /// screen to name when there is no key yet.
    pub env_var: Option<String>,
    /// A declared endpoint's address and protocol; `None` for a built-in
    /// service, whose address is not the user's business.
    pub endpoint: Option<String>,
    pub protocol: Option<String>,
    /// Whether this row's service can be withdrawn outright, rather than
    /// merely having its key taken back — true for a declared endpoint,
    /// never for a built-in one.
    pub withdrawable: bool,
}

/// The accounts screen: every service, whether or not it has a key, and a
/// plain sentence naming where a key typed here would be kept.
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccountList {
    pub accounts: Vec<Account>,
    /// "the macOS Keychain", "the Windows Credential Manager", or the
    /// owner-only file a computer with no credential manager falls back to.
    pub vault: String,
    /// The protocols an added endpoint may speak.
    pub protocols: Vec<String>,
}

/// Every account, in the store's own order: built-in services first, then
/// signed-in `ChatGPT` accounts, then declared endpoints.
pub fn list(store: &Mutex<CredentialStore>) -> AccountList {
    let accounts = {
        let store = lock(store);
        let available = store.available();
        store
            .known()
            .iter()
            .map(|account| row(&store, account, &available))
            .collect()
    };
    AccountList {
        accounts,
        vault: KEYCHAIN.vault().to_string(),
        protocols: config::protocols()
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

fn row(
    store: &CredentialStore,
    account: &identity::Account,
    available: &[identity::Account],
) -> Account {
    let credential = store.get(&account.id);
    let source = match credential {
        None => Source::None,
        Some(Credential::OAuth(_)) => Source::SignedIn,
        // The inert bearer a keyless local server is bound to is a state,
        // not a key anyone typed; showing its tail would read as one.
        Some(Credential::ApiKey(key)) if key == NO_AUTH_PLACEHOLDER => Source::NoKey,
        // The store remembers which door each key came through, so drawing
        // this list costs the vault no round trip and no unlock prompt.
        Some(Credential::ApiKey(_)) if store.was_admitted(&account.id) => Source::Keychain,
        Some(Credential::ApiKey(_)) => Source::Environment,
    };
    // A built-in service's address is baked in and not the user's business;
    // only a declared endpoint's is shown.
    let withdrawable = identity::built_in(&account.service.name).is_none();
    let (endpoint, protocol) = if withdrawable {
        (
            account.service.endpoint.clone(),
            config::protocol_for_adapter(account.service.adapter).map(str::to_string),
        )
    } else {
        (None, None)
    };
    Account {
        id: account.id.as_str().to_string(),
        label: identity::label(account, available),
        source,
        hint: credential.and_then(hint),
        env_var: env_var_of(&account.service),
        endpoint,
        protocol,
        withdrawable,
    }
}

/// The environment variable an account would read a key from, for the
/// screen to name when there is no key yet — `None` for a `ChatGPT` login
/// and for a declaration that names no variable of its own.
fn env_var_of(service: &Service) -> Option<String> {
    match &service.auth {
        Auth::Env(var) => Some(var.clone()),
        Auth::OAuth | Auth::Unnamed => None,
    }
}

/// The last four characters of an API key: enough to recognise which key is
/// set, useless to anyone reading over a shoulder.
///
/// A plan login has none — the user never typed its token and cannot retype
/// it — and neither has the inert bearer of a keyless server, whose tail
/// would read as a key it does not have.
fn hint(credential: &Credential) -> Option<String> {
    let Credential::ApiKey(key) = credential else {
        return None;
    };
    if key == NO_AUTH_PLACEHOLDER {
        return None;
    }
    let tail = key.char_indices().rev().take(4).last()?.0;
    Some(key[tail..].to_string())
}

/// Keep `key` for the account named `id`, and put it to work at once, with
/// no restart.
///
/// # Errors
/// Returns a plain sentence if no such account is known, if the key is
/// blank, or if this computer's credential manager would not keep it.
pub fn set_key(
    store: &Mutex<CredentialStore>,
    catalog: &Mutex<ModelCatalog<LiveSource>>,
    id: &str,
    key: &str,
) -> Result<(), String> {
    let (account, display) = {
        let store = lock(store);
        let account = find(&store, id)?;
        let available = store.available();
        drop(store);
        let display = identity::label(&account, &available);
        (account, display)
    };
    let key = checked_key(&display, key)?;
    KEYCHAIN.store(account.id.as_str(), &key)?;
    let credential = lock(store).admit_key(&account, key);
    lock(catalog).add_credential(account, credential);
    Ok(())
}

/// Take back the key this window put away for the account named `id`.
///
/// The account does not disappear: it is left known but keyless, or bound
/// again to the key the launching environment supplied all along, which the
/// store kept underneath rather than letting the admission destroy.
///
/// Total in every state, so no caller has to ask first: an id the vault
/// never held is not an error, and revealing the environment's key is the
/// same act as unbinding when there is none to reveal.
///
/// # Errors
/// Returns a plain sentence if no such account is known, or if the
/// credential manager would not give the key up.
pub fn forget_key(store: &Mutex<CredentialStore>, id: &str) -> Result<(), String> {
    let account = find(&lock(store), id)?;
    KEYCHAIN.forget(account.id.as_str())?;
    lock(store).forget(&account.id);
    Ok(())
}

/// Declare another service to talk to: a name, an address, the protocol it
/// speaks, and (unless it wants none) a key.
///
/// # Errors
/// Returns a plain sentence if the name is taken, the address is not one,
/// the protocol is not one of [`config::protocols`], or the declarations
/// file or credential manager refused the write.
pub fn add_endpoint(
    store: &Mutex<CredentialStore>,
    catalog: &Mutex<ModelCatalog<LiveSource>>,
    name: &str,
    endpoint: &str,
    protocol: &str,
    key: Option<&str>,
) -> Result<(), String> {
    let service = declare_endpoint(&lock(store), name, endpoint, protocol, LABEL)?;

    // Checked before a line is written: a key refused after the declaration
    // was saved would leave the service in the file and unusable.
    let typed = match key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(key) => Some(checked_key(service.name.as_str(), key)?),
        None => None,
    };

    let mut declared = declared_endpoints(&lock(store));
    declared.push(service.clone());
    declared.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    config::save_declared(&declarations_path(), &declared, LABEL)?;

    let account = identity::Account::of_service(service);
    // No key at all is a local server that wants none, admitted with the
    // inert bearer exarch already keeps for exactly that.
    let secret = match typed {
        Some(key) => {
            KEYCHAIN.store(account.id.as_str(), &key)?;
            key
        }
        None => NO_AUTH_PLACEHOLDER.to_string(),
    };
    let credential = lock(store).admit_key(&account, secret);
    lock(catalog).add_credential(account, credential);
    Ok(())
}

/// Withdraw a declared endpoint: out of the live store, out of the file, out
/// of the credential manager.
///
/// # Errors
/// Returns a plain sentence if `id` names no known account, if it names a
/// built-in service rather than a declared endpoint (its key can be taken
/// back, but the service itself cannot be removed), or if the file or
/// credential manager refused the write.
pub fn forget_endpoint(store: &Mutex<CredentialStore>, id: &str) -> Result<(), String> {
    let declared = {
        let mut store = lock(store);
        withdraw_endpoint(&mut store, id)?;
        declared_endpoints(&store)
    };
    config::save_declared(&declarations_path(), &declared, LABEL)?;
    // `id` is the account id's own rendering — `withdraw_endpoint` resolved
    // it — and the vault entry is named by exactly that string.
    KEYCHAIN.forget(id)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
