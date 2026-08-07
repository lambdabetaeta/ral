//! Synod's own provider accounts: which services it can talk to, and what
//! it authenticates to each of them with.
//!
//! Synod is a desktop application, so keys are typed into a window and
//! kept in this computer's credential manager
//! ([`exarch::provider::keychain`]).  Per provider, the credential manager
//! wins — it is the one source a person can see and change from inside
//! synod — with the environment as fallback, resolved and scrubbed by
//! [`CredentialStore::resolve_and_scrub`] exactly as in exarch.  A key
//! from the environment is never silently written into the vault.
//!
//! Which services *exist* is a third thing, and not a secret: the famous
//! ones are [`exarch::provider::ProviderKind`]'s own list, and any further
//! endpoint is declared in `$XDG_CONFIG_HOME/synod/providers.ral`, written
//! by the accounts screen and read by exarch's one declaration decoder
//! ([`exarch::config`]).  It holds addresses, never keys.

use exarch::config;
use exarch::provider::credential::{self, Credential, CredentialStore, NO_AUTH_PLACEHOLDER};
use exarch::provider::keychain::Keychain;
use exarch::provider::models::{LiveSource, ModelCatalog};
use exarch::provider::{CustomProvider, ProviderId};
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
/// top, so a key typed into the accounts screen outranks a stale variable in
/// the launching environment.
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
    for id in store.known().to_vec() {
        if let Some(key) = KEYCHAIN.read(id.label()) {
            store.admit_key(&id, key);
        }
    }
    Ok(store)
}

/// One row of the accounts screen.
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// The provider's label — its name in the picker, and its name in the
    /// credential manager.
    pub label: String,
    /// `service` for a famous provider, `endpoint` for one declared here,
    /// `chatgpt` for a signed-in plan.
    pub kind: &'static str,
    /// `keychain`, `environment`, `signed-in`, `no-key`, or `none` — where
    /// the credential in force actually came from.
    pub source: &'static str,
    /// The last four characters of the key in force, never more.
    pub hint: Option<String>,
    /// The environment variable this provider would read from, for the
    /// screen to name when there is no key yet.
    pub env_var: Option<String>,
    /// A declared endpoint's address and protocol; `None` for the famous
    /// providers, whose addresses are not the user's business.
    pub endpoint: Option<String>,
    pub protocol: Option<String>,
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

/// Every account, in the store's own order: famous services first, then
/// signed-in plans, then declared endpoints.
pub fn list(store: &Mutex<CredentialStore>) -> AccountList {
    let accounts = {
        let store = lock(store);
        store.known().iter().map(|id| row(&store, id)).collect()
    };
    AccountList {
        accounts,
        vault: KEYCHAIN.vault().to_string(),
        protocols: config::protocols().into_iter().map(str::to_string).collect(),
    }
}

fn row(store: &CredentialStore, id: &ProviderId) -> Account {
    let credential = store.get(id);
    let source = match credential {
        None => "none",
        Some(Credential::OAuth(_)) => "signed-in",
        // The inert bearer a keyless local server is bound to is a state,
        // not a key anyone typed; showing its tail would read as one.
        Some(Credential::ApiKey(key)) if key == NO_AUTH_PLACEHOLDER => "no-key",
        // The store remembers which door each key came through, so drawing
        // this list costs the vault no round trip and no unlock prompt.
        Some(Credential::ApiKey(_)) if store.was_admitted(id) => "keychain",
        Some(Credential::ApiKey(_)) => "environment",
    };
    let (kind, endpoint, protocol) = match id {
        ProviderId::Famous(_) => ("service", None, None),
        ProviderId::ChatGpt(_) => ("chatgpt", None, None),
        ProviderId::Custom(custom) => (
            "endpoint",
            Some(custom.endpoint.clone()),
            config::protocol_for_adapter(custom.adapter).map(str::to_string),
        ),
    };
    Account {
        label: id.label().to_string(),
        kind,
        source,
        hint: credential.and_then(hint),
        env_var: id.key_env().map(str::to_string),
        endpoint,
        protocol,
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

/// Keep `key` for the service called `label`, and put it to work at once,
/// with no restart.
///
/// # Errors
/// Returns a plain sentence if no such service is known, if the key is
/// blank, or if this computer's credential manager would not keep it.
pub fn set_key(
    store: &Mutex<CredentialStore>,
    catalog: &Mutex<ModelCatalog<LiveSource>>,
    label: &str,
    key: &str,
) -> Result<(), String> {
    let key = checked_key(label, key)?;
    let id = find(store, label)?;
    KEYCHAIN.store(id.label(), &key)?;
    let credential = lock(store).admit_key(&id, key);
    lock(catalog).add_credential(id, credential);
    Ok(())
}

/// A typed-in key as it will be kept, or a question about what was pasted.
///
/// Every door a key is typed at asks this — one screen, one rule — and it is
/// exarch's own well-formedness rule ([`credential::well_formed_key`]), so a
/// key refused here is not one that would have been accepted from the
/// environment.
///
/// # Errors
/// Returns a plain sentence, phrased as a question, naming what is wrong with
/// what was pasted.
fn checked_key(label: &str, key: &str) -> Result<String, String> {
    if key.trim().is_empty() {
        return Err(format!("No key was typed for {label} — paste it first?"));
    }
    credential::well_formed_key(key).ok_or_else(|| {
        format!("That {label} key carries a line break — was more than the key copied?")
    })
}

/// Take back the key this window put away for `label`.
///
/// The account does not disappear: it is left known but keyless, or bound
/// again to the key the launching environment supplied all along, which the
/// store kept underneath rather than letting the admission destroy.
///
/// Total in every state, so no caller has to ask first: a label the vault
/// never held is not an error, and revealing the environment's key is the
/// same act as unbinding when there is none to reveal.
///
/// # Errors
/// Returns a plain sentence if no such service is known, or if the
/// credential manager would not give the key up.
pub fn forget_key(store: &Mutex<CredentialStore>, label: &str) -> Result<(), String> {
    let id = find(store, label)?;
    KEYCHAIN.forget(id.label())?;
    lock(store).forget(&id);
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
    label: &str,
    endpoint: &str,
    protocol: &str,
    key: Option<&str>,
) -> Result<(), String> {
    let label = label.trim();
    let endpoint = endpoint.trim();
    if label.is_empty() {
        return Err("What should this service be called?".to_string());
    }
    if find(store, label).is_ok() {
        return Err(format!("There is already a service called {label}."));
    }
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err(format!(
            "'{endpoint}' does not look like an address — should it begin with https://?"
        ));
    }
    let endpoint = if endpoint.ends_with('/') {
        endpoint.to_string()
    } else {
        // genai joins the service path onto the base, so the trailing
        // slash is not decoration; supplying it is kinder than refusing.
        format!("{endpoint}/")
    };
    let adapter = config::adapter_for_protocol(protocol, LABEL)?;

    // Checked before a line is written: a key refused after the declaration
    // was saved would leave the service in the file and unusable.
    let typed = match key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(key) => Some(checked_key(label, key)?),
        None => None,
    };

    let custom = CustomProvider {
        label: label.to_string(),
        // The key lives in the credential manager; a file synod wrote
        // never names an environment variable.
        key_env: None,
        endpoint,
        adapter,
    };
    let mut declared = declared_endpoints(store);
    declared.push(custom.clone());
    declared.sort_by(|a, b| a.label.cmp(&b.label));
    config::save_declared(&declarations_path(), &declared, LABEL)?;

    let id = ProviderId::Custom(std::sync::Arc::new(custom));
    // No key at all is a local server that wants none, admitted with the
    // inert bearer exarch already keeps for exactly that.
    let secret = match typed {
        Some(key) => {
            KEYCHAIN.store(label, &key)?;
            key
        }
        None => NO_AUTH_PLACEHOLDER.to_string(),
    };
    let credential = lock(store).admit_key(&id, secret);
    lock(catalog).add_credential(id, credential);
    Ok(())
}

/// Withdraw a declared endpoint: out of the file, out of the credential
/// manager, out of the picker.
///
/// # Errors
/// Returns a plain sentence if `label` names no declared endpoint (a famous
/// service cannot be withdrawn — only its key taken back), or if the file
/// or credential manager refused the write.
pub fn forget_endpoint(store: &Mutex<CredentialStore>, label: &str) -> Result<(), String> {
    let id = find(store, label)?;
    if !matches!(id, ProviderId::Custom(_)) {
        return Err(format!(
            "{label} is one of the services synod always knows — its key can be taken \
             back, but the service itself cannot be removed."
        ));
    }
    let declared: Vec<CustomProvider> = declared_endpoints(store)
        .into_iter()
        .filter(|custom| custom.label != label)
        .collect();
    config::save_declared(&declarations_path(), &declared, LABEL)?;
    KEYCHAIN.forget(label)?;
    lock(store).retire(&id);
    Ok(())
}

/// The endpoints currently declared, read back off the live store rather
/// than off the file — the store is what the running session believes, and
/// a file edited by hand behind synod's back should not be silently
/// re-adopted by an unrelated save.
fn declared_endpoints(store: &Mutex<CredentialStore>) -> Vec<CustomProvider> {
    lock(store)
        .known()
        .iter()
        .filter_map(|id| {
            if let ProviderId::Custom(custom) = id {
                Some(CustomProvider::clone(custom))
            } else {
                None
            }
        })
        .collect()
}

/// The provider called `label`, whether or not it has a credential.
fn find(store: &Mutex<CredentialStore>, label: &str) -> Result<ProviderId, String> {
    lock(store)
        .known()
        .iter()
        .find(|id| id.label() == label)
        .cloned()
        .ok_or_else(|| format!("There is no service called {label} on this computer."))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
