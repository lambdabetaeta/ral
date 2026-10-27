//! Environment keys outrank saved keys; endpoint declarations contain no keys.

use crate::wire::{Account, AccountList, Choice};
use exarch::provider::accounts;
use exarch::provider::credential::CredentialStore;
use exarch::provider::identity::{self, Account as ProviderAccount};
use exarch::provider::keychain::Keychain;
use exarch::provider::models::resolve_account;
use exarch::wallet::Wallet;
use ral_core::own::OwnError;
use ral_core::sync::LockExt;
use std::sync::Mutex;

use exarch::app::SYNOD;

const DECLARATIONS_FILE: &str = "providers.ral";

const LABEL: &str = "provider settings";

pub(crate) fn wallet() -> Result<Wallet, OwnError> {
    Wallet::of(SYNOD, DECLARATIONS_FILE, LABEL)
}

/// Called before threads: preparing credentials scrubs the launch environment.
pub(crate) fn prepare() -> Result<CredentialStore, String> {
    wallet()?.prepare()
}

pub(crate) fn list(store: &Mutex<CredentialStore>) -> AccountList {
    AccountList {
        accounts: accounts::entries(&store.lock_ignore_poison())
            .into_iter()
            .map(Account::from)
            .collect(),
        vault: Keychain::for_app(SYNOD).vault().to_string(),
        protocols: identity::protocols()
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

pub(crate) struct Selected {
    pub(crate) account: ProviderAccount,
    pub(crate) label: String,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
}

/// Resolve by account id, never by the non-unique display label; release before slow work.
pub(crate) fn select_account(
    store: &Mutex<CredentialStore>,
    choice: Choice,
) -> Result<Selected, String> {
    let store = store.lock_ignore_poison();
    let available = store.available();
    if available.is_empty() {
        return Err(
            "no assistant account is set up on this computer — sign in with ChatGPT on the \
             opening screen, or ask whoever administers this computer to set a \
             provider API key (ANTHROPIC_API_KEY, OPENAI_API_KEY, …)"
                .into(),
        );
    }
    let Choice {
        account,
        model,
        effort,
    } = choice;
    let account = resolve_account(&account, &available)
        .ok_or("the chosen account is no longer available on this computer")?;
    let label = identity::label(&account, &available);
    drop(store);
    Ok(Selected {
        account,
        label,
        model,
        effort,
    })
}
