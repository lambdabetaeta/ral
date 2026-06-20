//! Runtime state: the active model selection, persisted per project under
//! the XDG state home.
//!
//! The selected provider+model is *runtime state*, not config: the picker
//! writes it, startup loads it, and it sticks until changed. It lives in
//! the per-project directory [`crate::bootstrap::project_dir`]
//! (`$XDG_STATE_HOME/exarch/<project>/state.json`), beside that project's
//! session logs — keyed by where exarch was launched, so the memory is
//! per-project without scattering a dotfile into the working tree. Because
//! the path is outside cwd, the sandboxed agent cannot reach it (no
//! deny-list entry needed); exarch's own process — the picker — writes it.
//!
//! The format is JSON: this is state, not config-as-code, so a simple
//! robust serialisation is right.

use crate::provider::ProviderId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The state file name within a project's [`crate::bootstrap::project_dir`].
const STATE_FILE: &str = "state.json";

/// The persisted runtime state. Only the model selection lives here in
/// this slice; tuning (slice 2) will extend this struct, so unknown future
/// fields must not break an older binary — hence `#[serde(default)]` on
/// what we add later, and a tolerant load.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct State {
    /// The active provider, by its stable label
    /// ([`ProviderId::label`]) so the file is human-readable. A famous
    /// provider stores its `ProviderKind` label; a custom provider stores its
    /// `config.ral` map key.
    pub provider: String,
    /// The active model name.
    pub model: String,
}

impl State {
    /// Build state from a resolved selection.
    pub fn new(provider: &ProviderId, model: &str) -> Self {
        Self {
            provider: provider.label().to_string(),
            model: model.to_string(),
        }
    }

    /// Resolve the stored provider label back to a live [`ProviderId`] among
    /// the `available` providers. `None` when the label names no available
    /// provider — a key no longer set, a custom provider no longer in
    /// `config.ral`, or a hand-edited / future-version file — so the caller
    /// falls back to a default.
    pub fn provider_id(&self, available: &[ProviderId]) -> Option<ProviderId> {
        available
            .iter()
            .find(|id| id.label() == self.provider)
            .cloned()
    }
}

/// The state-file path within a project directory `dir`.
fn path_in(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

/// Load the selection from `dir/state.json`, or `None` when the file is
/// absent or unreadable as state. A malformed file is treated as absent
/// (the default selection applies) rather than failing startup — the
/// selection is recoverable state, not load-bearing config.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:state-read] reads the persisted picker selection; recoverable session state, not turn-time data I/O"
)]
pub fn load(dir: &Path) -> Option<State> {
    let bytes = std::fs::read(path_in(dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist `state` to `dir/state.json`, creating `dir` if needed. Returns a
/// message on failure so the picker can note it without aborting the
/// switch — the switch already took effect in memory; only its persistence
/// failed.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:state-write] persists the picker selection; recoverable session state, not turn-time data I/O"
)]
pub fn save(dir: &Path, state: &State) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = path_in(dir);
    let json =
        serde_json::to_string_pretty(state).map_err(|e| format!("serialise {STATE_FILE}: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[io-door:test] test fs/process scaffolding")]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "exarch-state-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A famous provider's id, for the round-trip tests.
    fn fam(kind: crate::provider::ProviderKind) -> ProviderId {
        ProviderId::Famous(kind)
    }

    /// Save then load round-trips the exact selection.
    #[test]
    fn save_load_round_trip() {
        use crate::provider::ProviderKind;
        let dir = tmp_dir();
        let state = State::new(&fam(ProviderKind::Deepseek), "deepseek-reasoner");
        save(&dir, &state).unwrap();
        let loaded = load(&dir).expect("state should load");
        assert_eq!(loaded, state);
        let available = [fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)];
        assert_eq!(
            loaded.provider_id(&available),
            Some(fam(ProviderKind::Deepseek))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A custom provider's selection round-trips by label and resolves back
    /// to the matching live `ProviderId`.
    #[test]
    fn custom_provider_round_trips_by_label() {
        let dir = tmp_dir();
        let custom = ProviderId::Custom(std::sync::Arc::new(crate::provider::CustomProvider {
            label: "local-llama".into(),
            key_env: "LOCAL_LLAMA_KEY".into(),
            endpoint: "https://llama.example/v1/".into(),
            adapter: genai::adapter::AdapterKind::OpenAI,
        }));
        let state = State::new(&custom, "llama-3");
        save(&dir, &state).unwrap();
        let loaded = load(&dir).expect("state should load");
        assert_eq!(loaded.provider, "local-llama");
        let available = [custom.clone()];
        assert_eq!(loaded.provider_id(&available), Some(custom));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An absent file loads as `None`, not an error.
    #[test]
    fn absent_file_is_none() {
        let dir = tmp_dir();
        assert!(load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A malformed file is tolerated as absent so it cannot brick startup.
    #[test]
    fn malformed_file_is_none() {
        let dir = tmp_dir();
        std::fs::write(path_in(&dir), b"{ not json").unwrap();
        assert!(load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unknown provider label resolves to `None` so the caller defaults.
    #[test]
    fn unknown_provider_label_is_none() {
        use crate::provider::ProviderKind;
        let state = State {
            provider: "mistral".into(),
            model: "m".into(),
        };
        let available = [fam(ProviderKind::Anthropic)];
        assert!(state.provider_id(&available).is_none());
    }
}
