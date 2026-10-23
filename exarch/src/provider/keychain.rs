//! The computer's own credential manager, behind one type.
//!
//! The macOS Keychain, the Windows Credential Manager, or the freedesktop
//! Secret Service, reached through a [`keyring`] entry named
//! `(app, account-id)` — so synod's Anthropic key and exarch's are two
//! entries, as their config directories are two directories ([`App`]).  A
//! key-bearing service's account id *is* its service name, so `anthropic`,
//! `deepseek` and every declared endpoint keep the entry name they have
//! always had; a `ChatGPT` login holds no key and never reaches this store.
//!
//! A headless box has no credential manager, and asking it is an error,
//! not a lie to paper over: [`Keychain::vault`] answers where secrets on
//! *this* computer actually land, so the window can say so rather than
//! implying a protection that is not there.  The fallback is an owner-only
//! file in the app's state directory, replaced whole as a
//! [`Visibility::Secret`] [`OwnDir::store`]: it is made again owner-only
//! whatever was there, and a crash leaves the old file or the new.
//!
//! Nothing here reads or writes the environment: a key from the
//! environment is [`super::credential`]'s business, and outranks this one.

use crate::app::App;
use crate::provider::credential::SecretVault;
use crate::provider::identity::Account;
use ral_core::own::{OwnDir, OwnError};
use ral_core::path::{Visibility, XdgKind};
use std::collections::BTreeMap;
use std::path::PathBuf;

const FALLBACK_FILE: &str = "keys.json";

/// Where this computer's secrets actually land.  The window shows this
/// verbatim, so both arms read as the end of the sentence "kept in …".
pub enum Vault {
    Os(&'static str),
    /// No credential manager could be reached; this owner-only file instead,
    /// which has no spelling where the process has no state directory.
    File(Option<PathBuf>),
}

impl std::fmt::Display for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Os(name) => f.write_str(name),
            Self::File(Some(path)) => write!(f, "the file {}", ral_core::path::shown(path)),
            Self::File(None) => f.write_str(
                "an owner-only file, but this process has no state directory to put it in",
            ),
        }
    }
}

/// One app's provider keys, in this computer's credential manager.
#[derive(Clone, Copy)]
pub struct Keychain {
    app: App,
}

impl Keychain {
    #[must_use]
    pub const fn for_app(app: App) -> Self {
        Self { app }
    }

    /// Where this computer keeps the keys, for the window to tell the user.
    pub fn vault(self) -> Vault {
        if Self::has_credential_manager() {
            Vault::Os(OS_VAULT_NAME)
        } else {
            Vault::File(self.fallback_path())
        }
    }

    /// The key stored for `id`, or `None` when none is.
    ///
    /// A blank or control-bearing entry reads as `None`: it authenticates
    /// nothing, and a provider bound to it would fail in the middle of a
    /// conversation rather than here.
    pub fn read(self, id: &str) -> Option<String> {
        let raw = match self.entry(id).ok()? {
            Some(entry) => entry.get_password().ok()?,
            // A query: a fallback file that cannot be read holds no key.
            None => self.read_fallback().ok()?.remove(id)?,
        };
        super::credential::well_formed_key(&raw)
    }

    /// Keep `key` under `id`, replacing whatever was there.
    ///
    /// # Errors
    /// Returns a plain sentence naming the vault that refused it — a locked
    /// keychain, a read-only state directory, a fallback file that cannot be
    /// read and so is not written over — never a bare error code.
    pub fn store(self, id: &str, key: &str) -> Result<(), String> {
        let Some(entry) = self.entry(id)? else {
            let mut keys = self.read_fallback()?;
            keys.insert(id.to_string(), key.to_string());
            return self.write_fallback(&keys);
        };
        entry
            .set_password(key)
            .map_err(|e| format!("could not keep the {id} key in {}: {e}", self.vault()))
    }

    /// Forget `id`'s key.  Forgetting one that was never kept is not an
    /// error: the user asked for it to be gone, and it is.
    ///
    /// # Errors
    /// Returns a plain sentence if the vault held the key and would not
    /// give it up, or if the fallback file cannot be read and so is not
    /// written over.
    pub fn forget(self, id: &str) -> Result<(), String> {
        let Some(entry) = self.entry(id)? else {
            let mut keys = self.read_fallback()?;
            if keys.remove(id).is_none() {
                return Ok(());
            }
            return self.write_fallback(&keys);
        };
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!(
                "could not forget the {id} key in {}: {e}",
                self.vault()
            )),
        }
    }

    /// This app's entry for `id`, or `None` on a computer with no
    /// credential manager — the one place the fallback branch is decided,
    /// and decided by the same question [`Self::vault`] answers.
    ///
    /// # Errors
    /// A computer that *has* a credential manager but will not open this
    /// entry is a failure to report, not a reason to demote the secret to a
    /// file while the window goes on claiming the keychain has it.
    fn entry(self, id: &str) -> Result<Option<keyring::Entry>, String> {
        if !Self::has_credential_manager() {
            return Ok(None);
        }
        keyring::Entry::new(self.app.name(), id)
            .map(Some)
            .map_err(|e| format!("could not reach {} for the {id} key: {e}", self.vault()))
    }

    fn has_credential_manager() -> bool {
        keyring::Entry::store_status().is_ok()
    }

    /// Where this app's keys land with no credential manager to hold them,
    /// as a spelling: `None` where the host names no state base.
    /// `pub(crate)` so [`super::credential_files`] can deny it: the file is a
    /// plain owner-only JSON map, and an owner-only file is no protection from
    /// an agent running as the user who owns it.
    pub(crate) fn fallback_path(self) -> Option<PathBuf> {
        self.app
            .xdg_dir(XdgKind::State)
            .map(|dir| dir.join(FALLBACK_FILE))
    }

    /// The fallback file's keys, an empty map where there is no file.
    ///
    /// # Errors
    /// [`OwnError`] where the state directory, or the file in it, cannot be
    /// read as a key map.  A caller that writes the map back stops here: a
    /// file that could not be read is never written over.
    fn read_fallback(self) -> Result<BTreeMap<String, String>, OwnError> {
        Ok(self.state()?.load(FALLBACK_FILE)?.unwrap_or_default())
    }

    fn write_fallback(self, keys: &BTreeMap<String, String>) -> Result<(), String> {
        Ok(self
            .state()?
            .store(FALLBACK_FILE, keys, Visibility::Secret)?)
    }

    /// Where the fallback file lives: this app's state directory.
    fn state(self) -> Result<OwnDir, OwnError> {
        self.app.own(XdgKind::State)
    }
}

impl SecretVault for Keychain {
    /// Read `account`'s key by its id — a key-bearing service's id *is* its
    /// service name, so this is the same entry [`Keychain::store`] filed a
    /// typed key under. [`super::credential::CredentialStore::read_vault`]
    /// asks only for the accounts that take a key.
    fn read(&self, account: &Account) -> Option<String> {
        Self::read(*self, account.id.as_str())
    }
}

#[cfg(target_os = "macos")]
const OS_VAULT_NAME: &str = "the macOS Keychain";
#[cfg(target_os = "windows")]
const OS_VAULT_NAME: &str = "the Windows Credential Manager";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const OS_VAULT_NAME: &str = "this computer's password manager";
