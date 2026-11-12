//! The wire door: the one place owned `genai` request values are made.
//!
//! In genai 0.7.3, Anthropic's `into_anthropic_request_parts` consumes
//! message cache controls by value. Its dedicated system field loses the
//! mark; an in-sequence system message preserves it. Explicit message marks
//! suppress request-level auto-placement.

use super::identity::HostedSearch;
use super::request::Request;
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
/// metered explicit cache, which the `ChatGPT` plan route rejects
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

/// The tools a request carries: the agent's own set, and the provider's
/// built-in web search under `search`.
///
/// Service policy grants search; the adapter whitelist only constrains its
/// encoding. Plain `OpenAI` would encode it as a function name.
pub(super) fn tool_defs(
    adapter: AdapterKind,
    hosted_search: HostedSearch,
    request: Request<'_>,
) -> Vec<Tool> {
    let Request {
        tools: offered,
        search,
        ..
    } = request;
    let mut tools: Vec<Tool> = offered.wire().collect();
    if search
        && matches!(
            adapter,
            AdapterKind::OpenAIResp | AdapterKind::Anthropic | AdapterKind::Gemini
        )
    {
        let search = match hosted_search {
            HostedSearch::None => return tools,
            HostedSearch::ExternalAccess if adapter == AdapterKind::OpenAIResp => {
                Tool::new_web_search().with_config(ToolConfig::Custom(
                    serde_json::json!({"external_web_access": true}),
                ))
            }
            HostedSearch::Native | HostedSearch::ExternalAccess => Tool::new_web_search(),
        };
        tools.push(search);
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Toolset;
    use genai::chat::ChatRole;
    use genai::chat::ToolName;

    fn searching(search: bool) -> Request<'static> {
        Request {
            system: "",
            transcript: &[],
            tools: Toolset::default(),
            search,
        }
    }

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
    fn codex_enables_external_access_only_on_responses() {
        let policy = crate::provider::Login::Codex.service().hosted_search;
        assert_eq!(policy, HostedSearch::ExternalAccess);
        let tools = tool_defs(AdapterKind::OpenAIResp, policy, searching(true));
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, ToolName::WebSearch);
        assert_eq!(
            tools[0].config,
            Some(ToolConfig::Custom(
                serde_json::json!({"external_web_access": true})
            )),
        );
        for adapter in [AdapterKind::Anthropic, AdapterKind::Gemini] {
            let tools = tool_defs(adapter, policy, searching(true));
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, ToolName::WebSearch);
            assert_eq!(tools[0].config, None);
        }
    }

    #[test]
    fn declared_protocols_and_scripted_service_grant_no_hosted_search() {
        use crate::provider::{Auth, Service, ServiceName, scripted_service};

        for adapter in [AdapterKind::OpenAIResp, AdapterKind::Anthropic] {
            let service = Service::declared(
                ServiceName::declared("local").unwrap(),
                "https://local.example/v1/".into(),
                adapter,
                Auth::Keyless,
            );
            assert_eq!(service.hosted_search, HostedSearch::None);
            assert!(tool_defs(adapter, service.hosted_search, searching(true)).is_empty());
        }
        let service = scripted_service();
        assert!(
            tool_defs(
                service.model_protocol.default_adapter(),
                service.hosted_search,
                searching(true),
            )
            .is_empty()
        );
    }

    #[test]
    fn first_party_hosted_search_is_preserved() {
        use crate::provider::Account;

        for name in ["openai", "anthropic", "gemini", "openai-chatgpt"] {
            let service = Account::built_in(name).service;
            assert_eq!(service.hosted_search, HostedSearch::Native);
            let adapter = service.model_protocol.default_adapter();
            let tools = tool_defs(adapter, service.hosted_search, searching(true));
            assert_eq!(tools.len(), 1, "{name}");
            assert_eq!(tools[0].name, ToolName::WebSearch);
            assert_eq!(tools[0].config, None);
            assert!(tool_defs(adapter, service.hosted_search, searching(false)).is_empty());
        }
    }

    #[test]
    fn unavailable_services_never_send_hosted_search() {
        for name in [
            "openrouter",
            "deepseek",
            "opencode-zen",
            "opencode-go",
            "xai",
            "qwen",
            "xai-grok",
        ] {
            let service = crate::provider::Account::built_in(name).service;
            assert_eq!(service.hosted_search, HostedSearch::None);
            for adapter in [
                AdapterKind::OpenAIResp,
                AdapterKind::Anthropic,
                AdapterKind::Gemini,
            ] {
                assert!(tool_defs(adapter, service.hosted_search, searching(true)).is_empty());
            }
        }
    }

    #[test]
    fn unsupported_adapters_get_no_search_tool() {
        for adapter in [AdapterKind::OpenAI, AdapterKind::Xai] {
            for policy in [
                HostedSearch::None,
                HostedSearch::Native,
                HostedSearch::ExternalAccess,
            ] {
                assert!(tool_defs(adapter, policy, searching(true)).is_empty());
            }
        }
    }
}
