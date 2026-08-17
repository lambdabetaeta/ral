//! The computer's own credential manager, behind one door.
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
//! implying a protection that is not there.  The fallback is a `0600` file
//! written through [`secret_file::write_private`].
//!
//! Nothing here reads or writes the environment: a key from the
//! environment is [`super::credential`]'s business.  Exarch does not use
//! this store; synod does.

use crate::bootstrap::App;
use crate::provider::credential::SecretVault;
use crate::provider::identity::Account;
use crate::provider::secret_file;
use std::collections::BTreeMap;
use std::path::PathBuf;

const FALLBACK_FILE: &str = "keys.json";

/// Where this computer's secrets actually land.  The window shows this
/// verbatim, so both arms read as the end of the sentence "kept in …".
pub enum Vault {
    Os(&'static str),
    /// No credential manager could be reached; this owner-only file instead.
    File(PathBuf),
}

impl std::fmt::Display for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Os(name) => f.write_str(name),
            Self::File(path) => write!(f, "the file {}", path.display()),
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
            None => self.read_fallback().remove(id)?,
        };
        super::credential::well_formed_key(&raw)
    }

    /// Keep `key` under `id`, replacing whatever was there.
    ///
    /// # Errors
    /// Returns a plain sentence naming the vault that refused it — a locked
    /// keychain, a read-only config directory — never a bare error code.
    pub fn store(self, id: &str, key: &str) -> Result<(), String> {
        let Some(entry) = self.entry(id)? else {
            let mut keys = self.read_fallback();
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
    /// give it up.
    pub fn forget(self, id: &str) -> Result<(), String> {
        let Some(entry) = self.entry(id)? else {
            let mut keys = self.read_fallback();
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

    fn fallback_path(self) -> PathBuf {
        self.app
            .xdg_dir(ral_core::path::basedir::XdgKind::Config)
            .join(FALLBACK_FILE)
    }

    /// The fallback file's contents, or an empty map — an unreadable or
    /// malformed file reads as no keys, since the window's next `store`
    /// rewrites it wholesale and a hard error here would leave the user
    /// unable to enter one at all.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:key-read] reads the owner-only fallback key file; credential store infra, not turn-time data I/O"
    )]
    fn read_fallback(self) -> BTreeMap<String, String> {
        std::fs::read(self.fallback_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:key-write] creates the app's config directory for the owner-only fallback key file; credential store infra, not turn-time data I/O"
    )]
    fn write_fallback(self, keys: &BTreeMap<String, String>) -> Result<(), String> {
        let path = self.fallback_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not make {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(keys)
            .map_err(|e| format!("could not write out the keys: {e}"))?;
        secret_file::write_private(&path, json.as_bytes())
            .map_err(|e| format!("could not write {}: {e}", path.display()))
    }
}

impl SecretVault for Keychain {
    /// Read `account`'s key by its id — a key-bearing service's id *is* its
    /// service name, so this is the same entry [`Keychain::store`] filed a
    /// typed key under. A `ChatGPT` account holds no key and is never asked:
    /// [`super::credential::CredentialStore::admit_from`] offers this vault
    /// only the accounts that authenticate with one.
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
