//! The active model selection, persisted as JSON in the project's state
//! directory ([`crate::bootstrap::App::project_dir`]) beside that project's
//! session logs.
//!
//! That directory is keyed by where exarch was launched, and sits outside
//! cwd, so the sandboxed agent has no path to it and needs no deny-list
//! entry.

use crate::provider::identity::{self, Account};
use crate::provider::models::resolve_account;
use crate::provider::Provider;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const STATE_FILE: &str = "state.json";

/// The persisted selection: provider and model only. Unknown keys are
/// ignored, so a file written by another version of exarch still loads.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct State {
    /// The selected account's [`identity::AccountId::as_str`] rendering,
    /// which [`State::account`] matches against the live accounts. For a
    /// `ChatGPT` login that is the account id, not the login email two
    /// accounts can share.
    pub provider: String,
    /// The account's name at save time — a snapshot, defaulted empty for a
    /// file written before this field existed. The one thing a stale
    /// selection can still say about itself once its account is gone.
    #[serde(default)]
    pub provider_name: String,
    pub model: String,
}

impl State {
    /// Build state from a live selection. `available` is the set `account`
    /// was chosen from — needed only to compute [`Self::provider_name`]'s
    /// snapshot, per [`identity::label`]'s own rule that a label is always
    /// computed against the set in hand, never cached on the account itself.
    pub fn new(account: &Account, available: &[Account], model: &str) -> Self {
        Self {
            provider: account.id.as_str().to_string(),
            provider_name: identity::label(account, available),
            model: model.to_string(),
        }
    }

    /// The persisted form of `provider`, the selection in force.
    pub fn of(provider: &Provider, available: &[Account]) -> Self {
        Self::new(&provider.account, available, &provider.model)
    }

    /// The stored account-id rendering matched against the live `available`
    /// accounts. `None` — an unset key, a dropped `config.ral` entry, a
    /// signed-out account — leaves the caller to ask for a choice and say why;
    /// [`Self::provider_name`] is what that message names.
    pub fn account(&self, available: &[Account]) -> Option<Account> {
        resolve_account(&self.provider, available)
    }
}

fn path_in(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

/// The selection saved in `dir`; `None` when the file is absent or malformed —
/// a corrupt file reads as no selection rather than bricking startup.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:state-read] reads the persisted picker selection; recoverable session state, not turn-time data I/O"
)]
pub fn load(dir: &Path) -> Option<State> {
    let bytes = std::fs::read(path_in(dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist `state` to `dir/state.json`, creating `dir` if needed. Failure is
/// returned, not fatal: the switch already took effect in memory, so
/// `tui::model_picker` merely notes it.
///
/// # Errors
/// Returns `Err` if creating `dir`, serialising, or writing fails.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:state-write] persists the picker selection; recoverable session state, not turn-time data I/O"
)]
pub fn save(dir: &Path, state: &State) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = path_in(dir);
    let json =
        serde_json::to_string_pretty(state).map_err(|e| format!("serialise {STATE_FILE}: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::provider::identity::{Auth, Service, ServiceName};

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "exarch-state-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tmp_dir();
        let deepseek = Account::built_in("deepseek");
        let available = [Account::built_in("anthropic"), deepseek.clone()];
        let state = State::new(&deepseek, &available, "deepseek-reasoner");
        save(&dir, &state).unwrap();
        let loaded = load(&dir).expect("state should load");
        assert_eq!(loaded, state);
        assert_eq!(loaded.account(&available), Some(deepseek));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_provider_round_trips_by_service_name() {
        let dir = tmp_dir();
        let llama = Account::of_service(Service::declared(
            ServiceName::declared("local-llama").unwrap(),
            "https://llama.example/v1/".into(),
            genai::adapter::AdapterKind::OpenAI,
            Auth::Env("LOCAL_LLAMA_KEY".into()),
        ));
        let available = [llama.clone()];
        let state = State::new(&llama, &available, "model-a");
        save(&dir, &state).unwrap();
        let loaded = load(&dir).expect("state should load");
        assert_eq!(loaded.provider, "local-llama");
        assert_eq!(loaded.account(&available), Some(llama));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_file_is_none() {
        let dir = tmp_dir();
        assert!(load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_file_is_none() {
        let dir = tmp_dir();
        std::fs::write(path_in(&dir), b"{ not json").unwrap();
        assert!(load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_account_is_none() {
        let state = State {
            provider: "mistral".into(),
            provider_name: "mistral".into(),
            model: "m".into(),
        };
        let available = [Account::built_in("anthropic")];
        assert!(state.account(&available).is_none());
    }

    /// A `state.json` written before this change carries no `provider_name`
    /// and its `provider` is already a key-bearing service's own name — the
    /// one case the pre-change file and the new rendering coincide, so
    /// resolution must still succeed unchanged.
    #[test]
    fn a_pre_change_state_file_still_resolves() {
        let dir = tmp_dir();
        std::fs::write(
            path_in(&dir),
            br#"{"provider":"anthropic","model":"model-a","effort":"high"}"#,
        )
        .unwrap();
        let loaded = load(&dir).expect("a pre-change file must still load");
        assert_eq!(loaded.provider_name, "", "defaulted, not fabricated");
        let available = [Account::built_in("anthropic")];
        assert_eq!(
            loaded.account(&available),
            Some(Account::built_in("anthropic"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
