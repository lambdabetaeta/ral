//! Provider-independent request shaping.

use genai::adapter::AdapterKind;
use genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest, MessageOptions, ReasoningEffort, Tool,
};
use serde::{Deserialize, Serialize};

/// Per-selection reasoning and sampling controls.
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct Tuning {
    /// The reasoning effort, or `None` for the adapter default.
    pub effort: Option<ReasoningEffort>,
    /// The sampling temperature, or `None` for the adapter default.
    pub temperature: Option<f64>,
    /// The nucleus-sampling top-p, or `None` for the adapter default.
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

/// The effort ladder, ascending, shared by every front-end that offers a
/// discrete reasoning-effort control.
///
/// Exarch's `/model` overlay draws it as a rung ramp, synod's menu reads it
/// as a plain list. `auto` first (the default, no option sent); then
/// genai's keyword rungs from `none` up to `max`. `XHigh`'s keyword is
/// `xhigh`; `Budget`/`Minimal` are intentionally omitted (the former carries
/// a raw token count with no place on an ordered ladder, the latter is a
/// legacy pre-gpt-5 alias for `low`).
pub const EFFORT_LADDER: &[(&str, Option<ReasoningEffort>)] = &[
    ("auto", None),
    ("none", Some(ReasoningEffort::None)),
    ("low", Some(ReasoningEffort::Low)),
    ("med", Some(ReasoningEffort::Medium)),
    ("high", Some(ReasoningEffort::High)),
    ("xhigh", Some(ReasoningEffort::XHigh)),
    ("max", Some(ReasoningEffort::Max)),
];

/// Resolve a ladder label to its effort, strictly — an unrecognised label is
/// a caller error (a typo'd `--effort`, a stale menu choice), not a silent
/// fallback to `auto`.
///
/// # Errors
/// Returns `Err` naming `label` and listing the valid ladder labels when it
/// matches none of them.
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

/// The ladder label naming `effort` — the inverse of [`effort_by_label`],
/// for a front-end reporting back which rung is in force.
///
/// `None` for an effort the ladder does not name: the deliberately omitted
/// `Budget`/`Minimal` variants, which no ladder round trip can produce.
/// Matches by [`ReasoningEffort::variant_name`], the same coarse key
/// [`Tuning`]'s `PartialEq` uses.
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

/// The ladder label whose effort matches [`Tuning::initial`].
///
/// The rung a freshly-opened control should land on, read off the ladder
/// itself rather than restated as a literal that could drift out of step
/// with it.
///
/// # Panics
/// Panics if no ladder entry's effort matches [`Tuning::initial`]'s — a
/// broken invariant of this module, not a condition a caller can trigger.
pub fn default_effort_label() -> &'static str {
    effort_label(&Tuning::initial().effort)
        .expect("Tuning::initial()'s effort must appear on the effort ladder")
}

/// Manual impl: genai's `ReasoningEffort` has no derived `PartialEq`, so
/// this compares by [`ReasoningEffort::variant_name`] instead of
/// structurally — coarser only for `Budget(u32)`, a variant
/// [`EFFORT_LADDER`] never produces.
impl PartialEq for Tuning {
    fn eq(&self, other: &Self) -> bool {
        let key =
            |effort: &Option<ReasoningEffort>| effort.as_ref().map(ReasoningEffort::variant_name);
        key(&self.effort) == key(&other.effort)
            && self.temperature == other.temperature
            && self.top_p == other.top_p
    }
}

/// `OpenRouter`'s provider-routing extra body: pins the request to `slug`
/// with fallbacks disabled, so a route the user chose explicitly errors
/// out instead of silently landing on a different upstream.
fn openrouter_extra_body(slug: &str) -> serde_json::Value {
    serde_json::json!({
        "provider": {
            "order": [slug],
            "allow_fallbacks": false,
        }
    })
}

/// Assemble the per-request `ChatOptions`.  The four `capture_*` flags are
/// forced on regardless of `tuning`: `step_out_from_end` in `stream.rs`
/// reads usage, content, tool calls, and reasoning back off every
/// `StreamEnd`, so none of them can be left off.
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

/// Shape the transcript for prompt caching, routing the system prompt per
/// adapter: `OpenAIResp` (Responses API) takes it via `with_system`'s bare
/// `String` field, which can carry no cache breakpoint, so it bypasses the
/// message path entirely; every other adapter gets a cache-marked system
/// `ChatMessage` prepended instead.  The last message is marked too, so
/// consecutive turns share the largest possible cached prefix.
pub(super) fn build_cached_request(
    adapter: AdapterKind,
    system: &str,
    mut messages: Vec<ChatMessage>,
) -> ChatRequest {
    if let Some(last) = messages.last_mut() {
        last.options = Some(MessageOptions::from(CacheControl::Ephemeral));
    }
    if adapter == AdapterKind::OpenAIResp {
        return ChatRequest::new(messages).with_system(system);
    }
    let mut all = Vec::with_capacity(messages.len() + 1);
    all.push(
        ChatMessage::system(system.to_string())
            .with_options(MessageOptions::from(CacheControl::Ephemeral)),
    );
    all.extend(messages);
    ChatRequest::new(all)
}

pub(super) fn tool_defs(tool_enabled: bool) -> Vec<Tool> {
    tool_enabled
        .then(crate::shell_eval::tools::wire_tool)
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::ChatRole;

    #[test]
    fn responses_adapter_routes_system_to_instructions() {
        let request = build_cached_request(
            AdapterKind::OpenAIResp,
            "SYS",
            vec![ChatMessage::user("hi")],
        );
        assert_eq!(request.system.as_deref(), Some("SYS"));
        assert!(
            request
                .messages
                .iter()
                .all(|message| message.role != ChatRole::System)
        );
    }

    #[test]
    fn other_adapters_keep_a_cache_marked_system_message() {
        let request =
            build_cached_request(AdapterKind::Anthropic, "SYS", vec![ChatMessage::user("hi")]);
        assert!(request.system.is_none());
        assert_eq!(request.messages[0].role, ChatRole::System);
        assert_eq!(
            request.messages[0]
                .options
                .as_ref()
                .and_then(|options| options.cache_control.clone()),
            Some(CacheControl::Ephemeral)
        );
    }

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
