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

use crate::provider::{ProviderId, ReasoningEffort, Tuning};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The state file name within a project's [`crate::bootstrap::project_dir`].
const STATE_FILE: &str = "state.json";

/// The persisted runtime state: the model selection plus its tuning. The
/// tuning fields are `#[serde(default)]` so a state file written by an older
/// binary (model only) still loads — its tuning reads as "auto" — and a
/// hand-edited or future file with unknown keys is tolerated by [`load`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct State {
    /// The active provider, by its stable label
    /// ([`ProviderId::label`]) so the file is human-readable. A famous
    /// provider stores its `ProviderKind` label; a custom provider stores its
    /// `config.ral` map key.
    pub provider: String,
    /// The active model name.
    pub model: String,
    /// The reasoning-effort keyword ([`ReasoningEffort::as_keyword`]), or
    /// absent for "auto". Stored as a keyword so the file stays
    /// human-readable; an unrecognised keyword resolves back to "auto".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The sampling temperature, or absent for "auto".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// The nucleus-sampling top-p, or absent for "auto".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// The chosen `OpenRouter` serving-provider slug (`provider.order` routing),
    /// or absent for "auto" (`OpenRouter` decides). Routing, not sampling, so it
    /// is its own field rather than part of the tuning — meaningful only for an
    /// `OpenRouter` selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

impl State {
    /// Build state from a resolved selection, its tuning, and its `OpenRouter`
    /// route (the chosen serving-provider slug, `None` for auto). An effort that
    /// has no keyword (genai's `Budget`, which the overlay never produces) is
    /// dropped to "auto" rather than persisted opaquely.
    pub fn new(provider: &ProviderId, model: &str, tuning: &Tuning, route: Option<&str>) -> Self {
        Self {
            provider: provider.label().to_string(),
            model: model.to_string(),
            effort: tuning
                .effort
                .as_ref()
                .and_then(|e| e.as_keyword().map(str::to_string)),
            temperature: tuning.temperature,
            top_p: tuning.top_p,
            route: route.map(str::to_string),
        }
    }

    /// The persisted tuning, resolved back to live values. An effort keyword
    /// that no longer parses ([`ReasoningEffort::from_keyword`]) reads as
    /// "auto".
    pub fn tuning(&self) -> Tuning {
        Tuning {
            effort: self
                .effort
                .as_deref()
                .and_then(ReasoningEffort::from_keyword),
            temperature: self.temperature,
            top_p: self.top_p,
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
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
        let state = State::new(
            &fam(ProviderKind::Deepseek),
            "deepseek-reasoner",
            &Tuning::default(),
            None,
        );
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
        let state = State::new(&custom, "llama-3", &Tuning::default(), None);
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
            effort: None,
            temperature: None,
            top_p: None,
            route: None,
        };
        let available = [fam(ProviderKind::Anthropic)];
        assert!(state.provider_id(&available).is_none());
    }

    /// Tuning round-trips through the keyword + temperature + top-p fields, and
    /// the resolved [`Tuning`] matches what was saved.
    #[test]
    fn tuning_round_trips() {
        use crate::provider::ProviderKind;
        let dir = tmp_dir();
        let tuning = Tuning {
            effort: Some(ReasoningEffort::High),
            temperature: Some(0.7),
            top_p: Some(0.95),
        };
        let state = State::new(
            &fam(ProviderKind::Anthropic),
            "claude-opus-4",
            &tuning,
            Some("deepinfra"),
        );
        assert_eq!(state.effort.as_deref(), Some("high"));
        assert_eq!(state.route.as_deref(), Some("deepinfra"));
        save(&dir, &state).unwrap();
        let loaded = load(&dir).expect("state should load");
        assert_eq!(loaded.tuning(), tuning);
        assert_eq!(loaded.route.as_deref(), Some("deepinfra"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A state file written before tuning existed (model only) still loads,
    /// and its tuning reads as "auto" (both knobs unset).
    #[test]
    fn pre_tuning_file_loads_as_auto() {
        let dir = tmp_dir();
        std::fs::write(
            path_in(&dir),
            br#"{"provider":"anthropic","model":"claude-opus-4"}"#,
        )
        .unwrap();
        let loaded = load(&dir).expect("state should load");
        assert_eq!(loaded.tuning(), Tuning::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
