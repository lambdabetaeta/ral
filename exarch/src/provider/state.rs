//! The active model selection, persisted as JSON in the project's state
//! directory ([`crate::bootstrap::App::project_dir`]) beside that project's
//! session logs.
//!
//! That directory is keyed by where exarch was launched, and sits outside
//! cwd, so the sandboxed agent has no path to it and needs no deny-list
//! entry.

use crate::provider::{ProviderId, ReasoningEffort, Tuning};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const STATE_FILE: &str = "state.json";

/// The persisted selection. An absent knob means auto, and unknown keys are
/// ignored, so a file written by another version of exarch still loads.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct State {
    /// The provider's stable label ([`ProviderId::label`]), which
    /// [`State::provider_id`] matches on.
    pub provider: String,
    pub model: String,
    /// A [`ReasoningEffort::as_keyword`] spelling, so the file stays readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// The `OpenRouter` serving-provider slug pinned through `provider.order` —
    /// routing, not sampling, hence outside [`Tuning`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

impl State {
    /// Build state from a live selection. An effort with no keyword — genai's
    /// `Budget`, absent from the effort ladder — is stored as auto.
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

    /// The stored tuning as live values; a keyword that no longer parses
    /// ([`ReasoningEffort::from_keyword`]) reads as auto.
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

    /// The stored label matched against the live `available` providers. `None`
    /// — an unset key, a dropped `config.ral` entry, a signed-out account —
    /// leaves the caller to pick a default.
    pub fn provider_id(&self, available: &[ProviderId]) -> Option<ProviderId> {
        available
            .iter()
            .find(|id| id.label() == self.provider)
            .cloned()
    }
}

fn path_in(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

/// The selection saved in `dir`; `None` when the file is absent or malformed —
/// a corrupt file reads as no selection rather than bricking startup.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:state-read] reads the persisted picker selection; recoverable session state, not turn-time data I/O"
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

    fn fam(kind: crate::provider::ProviderKind) -> ProviderId {
        ProviderId::Famous(kind)
    }

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

    #[test]
    fn custom_provider_round_trips_by_label() {
        let dir = tmp_dir();
        let custom = ProviderId::Custom(std::sync::Arc::new(crate::provider::CustomProvider {
            label: "local-llama".into(),
            key_env: Some("LOCAL_LLAMA_KEY".into()),
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

    #[test]
    fn tuning_defaults_to_auto() {
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
