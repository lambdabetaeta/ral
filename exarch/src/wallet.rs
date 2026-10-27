//! One product's persisted account holdings: keys in its vault, declared
//! endpoints in its declarations file, each change applied live as well.
//! It sits above both [`crate::config`] and [`crate::provider`].
//!
//! Each method checks everything before it writes anything, and touches the
//! vault with the store unlocked: a keychain may stop to ask the user.

use crate::app::App;
use crate::config;
use crate::provider::Holdings;
use crate::provider::accounts::{
    checked_key, declare_endpoint, declared_endpoints, find, withdrawable,
};
use crate::provider::credential::CredentialStore;
use crate::provider::identity::{Account, Auth};
use crate::provider::keychain::Keychain;
use ral_core::own::{OwnError, UserFile};
use ral_core::path::XdgKind;
use ral_core::sync::LockExt;

/// Where one product keeps what its accounts screen changes: keys in its
/// vault, declared endpoints in its declarations file.
pub struct Wallet {
    pub(crate) keychain: Keychain,
    declarations: UserFile,
    /// How the declarations file names itself when it has a complaint.
    label: &'static str,
}

impl Wallet {
    /// One app's vault and declarations file in its config home.
    ///
    /// # Errors
    /// [`OwnError::NoBase`] where the host names no config directory.
    pub fn of(app: App, declarations: &'static str, label: &'static str) -> Result<Self, OwnError> {
        Ok(Self {
            keychain: Keychain::for_app(app),
            declarations: UserFile::xdg(app.name(), XdgKind::Config, declarations)?,
            label,
        })
    }

    /// Load declarations, resolve and scrub environment keys, then read the vault.
    ///
    /// # Errors
    /// Returns a sentence if declarations cannot be read or decoded.
    /// An unreachable vault only reduces the available accounts.
    ///
    /// # Panics
    /// Must be called while the process is still single-threaded:
    /// the credential scrub mutates the environment.
    pub fn prepare(&self) -> Result<CredentialStore, String> {
        let declared = config::load_declared(&self.declarations, self.label)?;
        let mut store = CredentialStore::resolve_and_scrub(declared);
        store.read_vault(&self.keychain);
        Ok(store)
    }

    /// Save `key` for the account named `id`, in force at once unless the
    /// launch environment's key outranks it.
    ///
    /// # Errors
    /// Returns a plain sentence if no such account is known, if it takes no
    /// key, if the key is malformed, or if the vault would not keep it.
    pub fn set_key(&self, holdings: &Holdings, id: &str, key: &str) -> Result<(), String> {
        let account = find(&holdings.store.lock_ignore_poison(), id)?;
        // A key-bearing account's label is its service's name.
        let name = account.service.name.as_str();
        if !matches!(account.service.auth, Auth::Key(_)) {
            return Err(format!("{name} takes no key: is it the service you meant?"));
        }
        let key = checked_key(name, key)?;
        self.keychain.store(account.id.as_str(), &key)?;
        holdings.change(|store| store.save_key(&account.id, key));
        Ok(())
    }

    /// Forget the key saved for the account named `id`. The account stays
    /// known, on the environment's key if it has one. Total: an account with
    /// no saved key is not an error.
    ///
    /// # Errors
    /// Returns a plain sentence if no such account is known, or if the vault
    /// would not give the key up.
    pub fn forget_key(&self, holdings: &Holdings, id: &str) -> Result<(), String> {
        let account = find(&holdings.store.lock_ignore_poison(), id)?;
        self.keychain.forget(account.id.as_str())?;
        holdings.change(|store| store.forget_key(&account.id));
        Ok(())
    }

    /// Declare another service: a name, an address, the protocol it speaks,
    /// and `key` — `None` for a server that checks none, else what was typed,
    /// blank when the key is to come from the environment or a later save.
    ///
    /// # Errors
    /// Returns a plain sentence if the name is taken, the address is not one,
    /// the protocol is not one of [`crate::provider::identity::protocols`], the key is
    /// malformed, or the declarations file or vault refused the write.
    pub fn add_endpoint(
        &self,
        holdings: &Holdings,
        name: &str,
        endpoint: &str,
        protocol: &str,
        key: Option<&str>,
    ) -> Result<(), String> {
        let service = declare_endpoint(
            &holdings.store.lock_ignore_poison(),
            name,
            endpoint,
            protocol,
            key.is_none(),
            self.label,
        )?;
        let typed = key
            .filter(|k| !k.trim().is_empty())
            .map(|k| checked_key(service.name.as_str(), k))
            .transpose()?;

        let mut declared = declared_endpoints(&holdings.store.lock_ignore_poison());
        declared.push(service.clone());
        declared.sort_by(|a, b| a.name.cmp(&b.name));
        config::save_declared(&self.declarations, &declared, self.label)?;
        let account = Account::of_service(service);
        if let Some(key) = &typed {
            self.keychain.store(account.id.as_str(), key)?;
        }
        holdings.change(|store| {
            let id = account.id.clone();
            store.declare(account);
            if let Some(key) = typed {
                store.save_key(&id, key);
            }
        });
        Ok(())
    }

    /// Withdraw a declared endpoint: out of the file, the vault, and the live
    /// store.
    ///
    /// # Errors
    /// Returns a plain sentence if `id` names no known account, if it names a
    /// built-in service rather than a declared endpoint, or if the file or
    /// vault refused the write.
    pub fn forget_endpoint(&self, holdings: &Holdings, id: &str) -> Result<(), String> {
        let account = withdrawable(&holdings.store.lock_ignore_poison(), id)?;
        let remaining: Vec<_> = declared_endpoints(&holdings.store.lock_ignore_poison())
            .into_iter()
            .filter(|service| service.name != account.service.name)
            .collect();
        config::save_declared(&self.declarations, &remaining, self.label)?;
        self.keychain.forget(account.id.as_str())?;
        holdings.change(|store| store.retire(&account.id));
        Ok(())
    }
}
