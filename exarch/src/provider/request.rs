//! Provider-independent request shaping.

use genai::chat::{ChatOptions, ReasoningEffort};
use serde::{Deserialize, Serialize};

/// Per-selection reasoning and sampling controls. A `None` field sends no
/// option at all, unlike `ReasoningEffort::Zero` — an explicit "no reasoning".
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct Tuning {
    pub effort: Option<ReasoningEffort>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
}

impl Tuning {
    /// The thinking-on tuning a fresh selection starts with.
    pub fn initial() -> Self {
        Self {
            effort: Some(ReasoningEffort::Medium),
            temperature: None,
            top_p: None,
        }
    }
}

/// The effort ladder, ascending: `auto` sends no option, then genai's keyword
/// rungs from `zero` (explicit opt-out) up to `max`.
///
/// Exarch's `/model` picker indexes into it and draws a glyph per rung, so
/// the order is load-bearing; synod serves the same labels flat. `Budget` (a
/// raw token count, unorderable) and `Minimal` (legacy pre-gpt-5) stay off
/// it.
pub const EFFORT_LADDER: &[(&str, Option<ReasoningEffort>)] = &[
    ("auto", None),
    ("zero", Some(ReasoningEffort::Zero)),
    ("low", Some(ReasoningEffort::Low)),
    ("med", Some(ReasoningEffort::Medium)),
    ("high", Some(ReasoningEffort::High)),
    ("xhigh", Some(ReasoningEffort::XHigh)),
    ("max", Some(ReasoningEffort::Max)),
];

/// Resolve a ladder label to its effort, strictly — an unrecognised label is a
/// caller error (a stale menu choice reaching synod's `resolve_tuning`), never
/// a silent fallback to `auto`.
///
/// # Errors
/// Returns `Err` naming `label` and the valid rungs.
pub fn effort_by_label(label: &str) -> Result<Option<ReasoningEffort>, String> {
    EFFORT_LADDER
        .iter()
        .find(|(candidate, _)| *candidate == label)
        .map(|(_, effort)| effort.clone())
        .ok_or_else(|| {
            let valid = EFFORT_LADDER
                .iter()
                .map(|(candidate, _)| *candidate)
                .collect::<Vec<_>>()
                .join(", ");
            format!("invalid effort '{label}' — expected one of: {valid}")
        })
}

/// The ladder label naming `effort`, the inverse of [`effort_by_label`], for a
/// front-end reporting which rung is in force. `None` off the ladder.
pub fn effort_label(effort: &Option<ReasoningEffort>) -> Option<&'static str> {
    EFFORT_LADDER
        .iter()
        .find(|(_, rung)| match (rung, effort) {
            (None, None) => true,
            (Some(a), Some(b)) => a.variant_name() == b.variant_name(),
            _ => false,
        })
        .map(|(label, _)| *label)
}

/// The rung a freshly-opened control lands on, read off the ladder rather than
/// restated as a literal that could drift out of step with it.
///
/// # Panics
/// Panics if [`Tuning::initial`]'s effort names no rung — this module's own
/// invariant, not a condition a caller can trigger.
pub fn default_effort_label() -> &'static str {
    effort_label(&Tuning::initial().effort)
        .expect("Tuning::initial()'s effort must appear on the effort ladder")
}

/// genai's `ReasoningEffort` has no `PartialEq`, so efforts compare by
/// [`ReasoningEffort::variant_name`] — coarser than structural equality only
/// for `Budget(u32)`, which [`EFFORT_LADDER`] never produces.
impl PartialEq for Tuning {
    fn eq(&self, other: &Self) -> bool {
        let key =
            |effort: &Option<ReasoningEffort>| effort.as_ref().map(ReasoningEffort::variant_name);
        key(&self.effort) == key(&other.effort)
            && self.temperature == other.temperature
            && self.top_p == other.top_p
    }
}

/// `OpenRouter`'s provider-routing extra body: pin to `slug` with fallbacks
/// off, so an explicitly chosen route errors rather than silently landing on a
/// different upstream.
fn openrouter_extra_body(slug: &str) -> serde_json::Value {
    serde_json::json!({
        "provider": {
            "order": [slug],
            "allow_fallbacks": false,
        }
    })
}

/// Assemble the per-request `ChatOptions`. The four `capture_*` flags are on
/// regardless of `tuning`: `step_out_from_end` in `stream.rs` reads usage,
/// content, tool calls, and reasoning back off every `StreamEnd`.
pub(super) fn complete_options(
    cache_key: &str,
    max_tokens_override: Option<u32>,
    tuning: &Tuning,
    route: Option<&str>,
) -> ChatOptions {
    let mut options = ChatOptions::default()
        .with_capture_usage(true)
        .with_capture_content(true)
        .with_capture_tool_calls(true)
        .with_capture_reasoning_content(true)
        .with_prompt_cache_key(cache_key);
    if let Some(max_tokens) = max_tokens_override {
        options = options.with_max_tokens(max_tokens);
    }
    if let Some(effort) = &tuning.effort {
        options = options.with_reasoning_effort(effort.clone());
    }
    if let Some(temperature) = tuning.temperature {
        options = options.with_temperature(temperature);
    }
    if let Some(top_p) = tuning.top_p {
        options = options.with_top_p(top_p);
    }
    if let Some(slug) = route {
        options = options.with_extra_body(openrouter_extra_body(slug));
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_options_leave_auto_controls_unset() {
        let options = complete_options("cache", None, &Tuning::default(), None);
        assert_eq!(options.max_tokens, None);
        assert_eq!(options.temperature, None);
        assert_eq!(options.top_p, None);
        assert!(options.reasoning_effort.is_none());
        assert!(options.extra_body.is_none());
    }

    #[test]
    fn openrouter_route_pins_slug_without_fallback() {
        let options = complete_options("cache", None, &Tuning::default(), Some("deepinfra"));
        assert_eq!(
            options.extra_body,
            Some(serde_json::json!({
                "provider": { "order": ["deepinfra"], "allow_fallbacks": false }
            }))
        );
    }

    #[test]
    fn default_effort_label_is_medium() {
        assert_eq!(default_effort_label(), "med");
    }

    #[test]
    fn effort_by_label_resolves_every_ladder_rung() {
        for (label, effort) in EFFORT_LADDER {
            let resolved = effort_by_label(label).unwrap();
            let key = |e: &Option<ReasoningEffort>| e.as_ref().map(ReasoningEffort::variant_name);
            assert_eq!(key(&resolved), key(effort));
        }
    }

    #[test]
    fn effort_by_label_rejects_an_unknown_label() {
        let err = effort_by_label("extreme").unwrap_err();
        assert!(err.contains("extreme"), "got: {err}");
        assert!(err.contains("xhigh"), "got: {err}");
    }
}
