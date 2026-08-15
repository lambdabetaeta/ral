//! The LLM boundary: one resolved model selection over a credential-bound
//! transport.
//!
//! History lives in the transcript the caller renders, not here; transport,
//! request shaping, retry, streaming, and usage each keep their own sibling
//! module.

pub mod credential;
mod error;
mod identity;
pub mod keychain;
pub mod listing;
pub mod models;
pub mod oauth;
pub mod pricing;
mod request;
mod retry;
pub mod scripted;
mod secret_file;
pub mod state;
mod stream;
pub mod tls;
mod transport;
mod usage;

pub use error::ProviderError;
pub(crate) use error::{error_object, extract_url};
pub(crate) use identity::provider_label;
pub use identity::{ChatGptAccount, CustomProvider, ProviderId, ProviderKind, Subscription};
pub use request::{EFFORT_LADDER, Tuning, default_effort_label, effort_by_label, effort_label};
#[allow(
    unused_imports,
    reason = "crate-private facade path retained for tls rustdoc and subsystem callers"
)]
pub(crate) use retry::MAX_ATTEMPTS;
pub use stream::{CutShort, Delta, StepOut, SummaryOut};
pub use transport::Engine;
pub use usage::{Usage, UsageParts, humanize_tokens};

pub use genai::chat::{ReasoningEffort, StopReason, ToolCall};

use crate::agent::cancel;
use credential::Credential;
use genai::chat::ChatMessage;
use std::sync::Arc;
use transport::Transport;

/// A session's chosen model, tuning, and routing, plus the backend its
/// requests run on.
pub struct Provider {
    backend: Backend,
    id: ProviderId,
    model: String,
    max_tokens_override: Option<u32>,
    tuning: Tuning,
    route: Option<String>,
}

/// A live transport over the shared [`Engine`], or a scripted replay so
/// agent-loop tests never touch the network.
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

    /// Build a provider that replays scripted outcomes instead of dialling out.
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

    /// Stream one assistant turn; `on_delta` fires per chunk, prose and
    /// reasoning in arrival order. Raced against `cancel`, so an interrupt
    /// need not wait on the next network chunk. `tool_enabled` gates our tool
    /// definitions, `search` the provider's own built-in web search.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete<F: FnMut(Delta<'_>)>(
        &self,
        system: &str,
        messages: Vec<ChatMessage>,
        tool_enabled: bool,
        search: bool,
        on_delta: &mut F,
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
                search,
                on_delta,
                cancel,
            ),
            Backend::Scripted(script) => script.complete(&self.model, on_delta),
        }
    }

    fn openrouter_route(&self) -> Option<&str> {
        match self.id.famous() {
            Some(ProviderKind::Openrouter) => self.route.as_deref(),
            _ => None,
        }
    }

    /// Summarise an already-rendered transcript.
    ///
    /// # Errors
    /// Returns an error once the bounded retry policy is exhausted.
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
        assert_eq!(provider.openrouter_route(), None);

        provider.id = ProviderId::Famous(ProviderKind::Openrouter);
        assert_eq!(provider.openrouter_route(), Some("deepinfra"));
    }
}
