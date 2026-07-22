//! The LLM boundary: provider selection over credential-bound transports.
//!
//! [`Provider::complete`] sends an already-rendered transcript and streams one
//! assistant reply. The transcript owns history; this facade owns selection
//! normalisation and delegates transport, request, retry, stream, and usage
//! invariants to their focused modules.

pub mod credential;
mod error;
mod identity;
pub mod models;
pub mod oauth;
pub mod pricing;
mod request;
mod retry;
pub mod scripted;
pub mod state;
mod stream;
pub mod tls;
mod transport;
mod usage;

pub use error::ProviderError;
pub(crate) use error::error_object;
pub(crate) use identity::provider_label;
pub use identity::{ChatGptAccount, CustomProvider, ProviderId, ProviderKind, Subscription};
pub use request::Tuning;
#[allow(
    unused_imports,
    reason = "crate-private facade path retained for tls rustdoc and subsystem callers"
)]
pub(crate) use retry::MAX_ATTEMPTS;
pub use stream::{CutShort, StepOut, SummaryOut};
pub use transport::Engine;
pub use usage::{Usage, UsageParts, humanize_tokens};

pub use genai::chat::{ReasoningEffort, StopReason, ToolCall};

use crate::agent::cancel;
use credential::Credential;
use genai::chat::ChatMessage;
use std::sync::Arc;
use transport::Transport;

pub struct Provider {
    backend: Backend,
    id: ProviderId,
    model: String,
    max_tokens_override: Option<u32>,
    tuning: Tuning,
    route: Option<String>,
}

enum Backend {
    Live {
        engine: Arc<Engine>,
        transport: Arc<Transport>,
    },
    Scripted(scripted::Script),
}

impl Provider {
    /// Build a live provider selection on a shared engine.
    pub fn build(
        engine: Arc<Engine>,
        id: &ProviderId,
        model: String,
        credential: &Credential,
        max_tokens_override: Option<u32>,
        tuning: Tuning,
        route: Option<String>,
    ) -> Self {
        let transport = engine.transport_for(id, &model, credential);
        Self {
            backend: Backend::Live { engine, transport },
            id: id.clone(),
            model,
            max_tokens_override,
            tuning,
            route,
        }
    }

    /// Build a deterministic provider that replays scripted outcomes.
    pub fn scripted(model: &str, kind: ProviderKind, script: scripted::Script) -> Self {
        Self {
            backend: Backend::Scripted(script),
            id: ProviderId::Famous(kind),
            model: model.to_string(),
            max_tokens_override: None,
            tuning: Tuning::default(),
            route: None,
        }
    }

    /// The user-supplied output cap, or `None` for the adapter default.
    pub fn max_tokens_override(&self) -> Option<u32> {
        self.max_tokens_override
    }

    /// The request tuning bound to this selection.
    pub fn tuning(&self) -> &Tuning {
        &self.tuning
    }

    /// The selected `OpenRouter` serving-provider slug, if any.
    pub fn route(&self) -> Option<&str> {
        self.route.as_deref()
    }

    /// The resolved model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The provider identity.
    pub fn id(&self) -> &ProviderId {
        &self.id
    }

    /// This model's context window when the pricing catalog knows it.
    pub fn context_window(&self) -> Option<u64> {
        pricing::context_window(&self.model)
    }

    /// Whether this selection is metered or subscription-backed.
    pub fn subscription(&self) -> Subscription {
        match &self.backend {
            Backend::Live { transport, .. } => transport.subscription(),
            Backend::Scripted(_) => Subscription::Metered,
        }
    }

    pub(crate) fn complete<F: FnMut(&str), G: FnMut(&str)>(
        &self,
        system: &str,
        messages: Vec<ChatMessage>,
        tool_enabled: bool,
        on_text: &mut F,
        on_think: &mut G,
        cancel: &cancel::Token,
    ) -> Result<StepOut, ProviderError> {
        match &self.backend {
            Backend::Live { engine, transport } => engine.complete(
                transport,
                &self.model,
                self.max_tokens_override,
                &self.tuning,
                self.openrouter_route(),
                system,
                messages,
                tool_enabled,
                on_text,
                on_think,
                cancel,
            ),
            Backend::Scripted(script) => script.complete(&self.model, on_text, on_think),
        }
    }

    /// Admit provider routing only for an `OpenRouter` selection.
    fn openrouter_route(&self) -> Option<&str> {
        match self.id.famous() {
            Some(ProviderKind::Openrouter) => self.route.as_deref(),
            _ => None,
        }
    }

    /// Summarise an already-rendered transcript.
    ///
    /// # Errors
    /// Returns an error after the bounded provider retry policy is exhausted.
    pub fn summarize(
        &self,
        system: &str,
        messages: Vec<ChatMessage>,
        max_tokens: u32,
        cancel: &cancel::Token,
    ) -> Result<SummaryOut, ProviderError> {
        match &self.backend {
            Backend::Live { engine, transport } => {
                engine.summarize(transport, &self.model, system, messages, max_tokens, cancel)
            }
            Backend::Scripted(script) => script.summarize(&self.model),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_route_is_ignored_by_other_providers() {
        let mut provider =
            Provider::scripted("gpt-5.5", ProviderKind::Openai, scripted::Script::new());
        provider.route = Some("deepinfra".into());
        assert_eq!(provider.route(), Some("deepinfra"));
        assert_eq!(provider.openrouter_route(), None);

        provider.id = ProviderId::Famous(ProviderKind::Openrouter);
        assert_eq!(provider.openrouter_route(), Some("deepinfra"));
    }
}
