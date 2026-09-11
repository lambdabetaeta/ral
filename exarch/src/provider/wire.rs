//! The wire door: the one place owned `genai` request values are made.
//!
//! Settled against the locked genai 0.7.0-beta.19 source
//! (`adapter/adapters/anthropic/adapter_shared.rs:110-132,325-341`): the
//! Anthropic adapter consumes the request by value and reads
//! `msg.options.and_then(|o| o.cache_control)` purely as a value;
//! `ChatMessage` carries no id, and neither the adapter (a unit struct)
//! nor the client holds anything across calls. No behaviour is keyed on
//! message identity, so marking a fresh clone per attempt is safe. On
//! Anthropic the system prompt must stay an in-sequence `ChatMessage::system`
//! rather than `ChatRequest.system`: the first system slot is barred from
//! a cache-control mark (`:122-126`), while an in-sequence System-role
//! message keeps one, and the request-level auto-mark defers to any
//! message-level breakpoint (`:325-341`) — exactly what the last-two
//! marks below guarantee. Every other adapter keeps the dedicated
//! `system` field, untouched by any of this.

use crate::shell_eval::tools::Toolset;
use genai::adapter::AdapterKind;
use genai::chat::{CacheControl, ChatMessage, ChatRequest, MessageOptions, Tool, ToolConfig};

/// A wire request: one manufacture, one send.
pub(super) struct Sealed(ChatRequest); // deliberately not Clone

impl Sealed {
    pub(super) fn into_request(self) -> ChatRequest {
        self.0
    }
}

/// The one whole-history deep copy: the shared transcript → genai's owned
/// request. Call inside the attempt, so a retry re-manufactures and no
/// pre-made template is ever cloned.
///
/// Shapes the transcript for prompt caching. Breakpoints are
/// Anthropic-only: on `OpenAI` the same `CacheControl` selects the
/// metered explicit cache, which the `ChatGPT` and Codex endpoints reject
/// outright; elsewhere the prompt-cache key carries the intent alone.
/// Marking the system prompt and the last two messages leaves two anchors
/// of different depths: the shallower is free (it writes the same
/// tokens), and it saves the rewrite whenever the next tail diverges
/// rather than extends — a retry, a fork, an eviction.
pub(super) fn manufacture(
    adapter: AdapterKind,
    system: &str,
    transcript: &[ChatMessage],
    tools: &[Tool],
) -> Sealed {
    let mut owned: Vec<ChatMessage> = transcript.to_vec();
    let mut request = if adapter == AdapterKind::Anthropic {
        for message in owned.iter_mut().rev().take(2) {
            message.options = Some(MessageOptions::from(CacheControl::Ephemeral));
        }
        let mut all = Vec::with_capacity(owned.len() + 1);
        all.push(
            ChatMessage::system(system.to_string())
                .with_options(MessageOptions::from(CacheControl::Ephemeral)),
        );
        all.extend(owned);
        ChatRequest::new(all)
    } else {
        ChatRequest::new(owned).with_system(system)
    };
    request.tools = (!tools.is_empty()).then(|| tools.to_vec());
    Sealed(request)
}

/// The tools a request carries: the agent's own `offered` set, and the
/// provider's built-in web search under `search`.
///
/// Search rides only the adapters genai maps `Tool::new_web_search()` to a
/// native tool for; the rest, plain `OpenAI` included, would serialise
/// `ToolName::WebSearch` into the function-name slot as junk on the wire.
/// `external_web_access` — codex's switch from its cached index to the live
/// internet — is `Custom` because genai's Responses arm drops the typed
/// `ToolConfig::WebSearch`, and `OpenAIResp`-only because genai merges a
/// `Custom` config onto the tool object, where Anthropic's
/// `web_search_20250305` would reject the unknown field.
pub(super) fn tool_defs(adapter: AdapterKind, offered: Toolset, search: bool) -> Vec<Tool> {
    let mut tools: Vec<Tool> = offered.wire().collect();
    if search
        && matches!(
            adapter,
            AdapterKind::OpenAIResp | AdapterKind::Anthropic | AdapterKind::Gemini
        )
    {
        let mut web_search = Tool::new_web_search();
        if adapter == AdapterKind::OpenAIResp {
            web_search = web_search.with_config(ToolConfig::Custom(serde_json::json!({
                "external_web_access": true
            })));
        }
        tools.push(web_search);
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::ChatRole;
    use genai::chat::ToolName;

    #[test]
    fn anthropic_marks_the_system_prompt_and_the_last_two_messages() {
        let transcript = vec![
            ChatMessage::user("one"),
            ChatMessage::user("two"),
            ChatMessage::user("three"),
        ];
        let request = manufacture(AdapterKind::Anthropic, "SYS", &transcript, &[]).into_request();
        assert!(request.system.is_none());
        assert_eq!(request.messages[0].role, ChatRole::System);
        let marked = |index: usize| {
            request.messages[index]
                .options
                .as_ref()
                .and_then(|options| options.cache_control.clone())
        };
        assert_eq!(marked(0), Some(CacheControl::Ephemeral));
        assert_eq!(marked(1), None);
        assert_eq!(marked(2), Some(CacheControl::Ephemeral));
        assert_eq!(marked(3), Some(CacheControl::Ephemeral));
    }

    #[test]
    fn openai_gets_a_bare_system_prompt_and_no_breakpoints() {
        let transcript = vec![ChatMessage::user("hi")];
        let request = manufacture(AdapterKind::OpenAIResp, "SYS", &transcript, &[]).into_request();
        assert_eq!(request.system.as_deref(), Some("SYS"));
        assert!(request.messages[0].options.is_none());
    }

    #[test]
    fn manufacture_leaves_the_source_messages_untouched() {
        let source = vec![ChatMessage::user("one"), ChatMessage::user("two")];
        let transcript = source.clone();
        let _ = manufacture(AdapterKind::Anthropic, "SYS", &transcript, &[]);
        assert!(source.iter().all(|message| message.options.is_none()));
        assert!(transcript.iter().all(|message| message.options.is_none()));
    }

    #[test]
    fn no_tools_leaves_the_request_tools_unset() {
        let transcript: Vec<ChatMessage> = Vec::new();
        let request = manufacture(AdapterKind::Anthropic, "SYS", &transcript, &[]).into_request();
        assert!(request.tools.is_none());
    }

    /// The degenerate last-two: a one-message transcript leaves the system
    /// prompt and that message both marked, and no third breakpoint invented.
    #[test]
    fn a_single_message_transcript_marks_the_system_prompt_and_itself() {
        let transcript = vec![ChatMessage::user("one")];
        let request = manufacture(AdapterKind::Anthropic, "SYS", &transcript, &[]).into_request();
        assert_eq!(request.messages.len(), 2);
        assert!(
            request
                .messages
                .iter()
                .all(|message| message.options.is_some())
        );
    }

    #[test]
    fn openai_resp_search_carries_live_internet_access() {
        let tools = tool_defs(AdapterKind::OpenAIResp, Toolset::default(), true);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, ToolName::WebSearch);
        assert_eq!(
            tools[0].config,
            Some(ToolConfig::Custom(
                serde_json::json!({"external_web_access": true})
            ))
        );
    }

    #[test]
    fn anthropic_search_carries_the_bare_tool() {
        let tools = tool_defs(AdapterKind::Anthropic, Toolset::default(), true);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, ToolName::WebSearch);
        assert_eq!(tools[0].config, None);
    }

    #[test]
    fn unsupported_adapter_gets_no_search_tool() {
        let tools = tool_defs(AdapterKind::OpenAI, Toolset::default(), true);
        assert!(tools.is_empty());
    }

    #[test]
    fn search_off_carries_no_tool_on_any_adapter() {
        for adapter in [
            AdapterKind::OpenAIResp,
            AdapterKind::Anthropic,
            AdapterKind::Gemini,
        ] {
            assert!(tool_defs(adapter, Toolset::default(), false).is_empty());
        }
    }
}
