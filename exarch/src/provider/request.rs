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

impl PartialEq for Tuning {
    fn eq(&self, other: &Self) -> bool {
        let key =
            |effort: &Option<ReasoningEffort>| effort.as_ref().map(ReasoningEffort::variant_name);
        key(&self.effort) == key(&other.effort)
            && self.temperature == other.temperature
            && self.top_p == other.top_p
    }
}

fn openrouter_extra_body(slug: &str) -> serde_json::Value {
    serde_json::json!({
        "provider": {
            "order": [slug],
            "allow_fallbacks": false,
        }
    })
}

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
}
