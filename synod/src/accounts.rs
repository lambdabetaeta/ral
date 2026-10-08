//! Synod's own provider accounts: which services it can talk to, and what
//! it authenticates to each of them with.
//!
//! Synod is a desktop application, so keys are typed into a window and
//! kept in this computer's credential manager
//! ([`exarch::provider::keychain`]).  Per account, a key in the launch
//! environment outranks the saved one, exactly as in exarch
//! ([`exarch::provider::credential`]); the window says so.  A key from the
//! environment is never silently written into the vault.
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
use exarch::provider::accounts::{self, Entry};
use exarch::provider::credential::CredentialStore;
use exarch::provider::identity;
use exarch::provider::keychain::Keychain;
use exarch::wallet::Wallet;
use ral_core::sync::LockExt;
use std::sync::Mutex;

use crate::session::SYNOD;

const DECLARATIONS_FILE: &str = "providers.ral";

/// How the declarations file names itself when it has a complaint to make.
const LABEL: &str = "provider settings";

/// Synod's own wallet: its vault, its declarations file.
pub fn wallet() -> Wallet {
    Wallet {
        keychain: Keychain::for_app(SYNOD),
        declarations: SYNOD
            .xdg_dir(ral_core::host::XdgKind::Config)
            .join(DECLARATIONS_FILE),
        label: LABEL,
    }
}

/// Resolve every account this computer offers, once, at startup.
///
/// The environment sweep first, then the credential manager beneath it
/// ([`CredentialStore::read_vault`]).
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
    let wallet = wallet();
    let declared = config::load_declared(&wallet.declarations, wallet.label)?;
    let mut store = CredentialStore::resolve_and_scrub(declared);
    store.read_vault(&wallet.keychain);
    Ok(store)
}

/// Where a row's credential in force actually came from.
#[derive(serde::Serialize, Clone, Copy, PartialEq, Eq, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// No credential at all — the row is known but keyless.
    None,
    /// A `ChatGPT` login: the one case a key is never typed, and the sign-in
    /// affordance replaces the key form.
    SignedIn,
    /// A server that checks no key.
    Keyless,
    Vault,
    /// The launch environment's key, which outranks a saved one.
    Environment,
}

impl From<accounts::Source> for Source {
    fn from(source: accounts::Source) -> Self {
        match source {
            accounts::Source::None => Self::None,
            accounts::Source::SignedIn => Self::SignedIn,
            accounts::Source::Keyless => Self::Keyless,
            accounts::Source::Vault => Self::Vault,
            accounts::Source::Environment => Self::Environment,
        }
    }
}

/// One row of the accounts screen: an [`Entry`] on the wire.
#[derive(serde::Serialize, Clone, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub struct Account {
    /// The identifier command payloads name this row by — an `AccountId`
    /// rendering, resolved back through [`exarch::provider::accounts::find`].
    /// Never shown; [`Self::label`] is what the screen draws.
    pub id: String,
    /// This account's name among every account currently known.
    pub label: String,
    pub source: Source,
    /// The last four characters of the key in force, never more.
    pub hint: Option<String>,
    /// The last four of a saved key the environment's outranks.
    pub shadowed: Option<String>,
    /// The environment variable a key-bearing account reads.
    pub env_var: Option<String>,
    /// A declared endpoint's address and protocol; `None` for a built-in
    /// service, whose address is not the user's business.
    pub endpoint: Option<String>,
    pub protocol: Option<String>,
    /// Whether this row's service can be withdrawn outright, rather than
    /// merely having its key taken back.
    pub withdrawable: bool,
}

impl From<Entry> for Account {
    fn from(entry: Entry) -> Self {
        Self {
            id: entry.id,
            label: entry.label,
            source: entry.source.into(),
            hint: entry.hint,
            shadowed: entry.shadowed,
            env_var: entry.env_var,
            endpoint: entry.endpoint,
            protocol: entry.protocol,
            withdrawable: entry.withdrawable,
        }
    }
}

/// The accounts screen: every service, whether or not it has a key, and a
/// plain sentence naming where a key typed here would be kept.
#[derive(serde::Serialize, Clone, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub struct AccountList {
    pub accounts: Vec<Account>,
    /// "the macOS Keychain", "the Windows Credential Manager", or the
    /// owner-only file a computer with no credential manager falls back to.
    pub vault: String,
    /// The protocols an added endpoint may speak.
    pub protocols: Vec<String>,
}

/// Every account, in the store's own order.
pub fn list(store: &Mutex<CredentialStore>) -> AccountList {
    AccountList {
        accounts: accounts::entries(&store.lock_ignore_poison())
            .into_iter()
            .map(Account::from)
            .collect(),
        vault: wallet().keychain.vault().to_string(),
        protocols: identity::protocols()
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}
