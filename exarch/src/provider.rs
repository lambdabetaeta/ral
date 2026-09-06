//! The LLM boundary: one resolved model selection over a credential-bound
//! transport.
//!
//! History lives in the transcript the caller renders, not here; transport,
//! request shaping, retry, streaming, and usage each keep their own sibling
//! module.

pub mod accounts;
pub mod bureau;
pub mod credential;
mod error;
pub mod identity;
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
mod wire;

pub use bureau::Bureau;
pub use error::ProviderError;
pub(crate) use error::{error_object, extract_url, transient_label};
pub use identity::{Account, AccountId, Auth, Billing, Service, ServiceName};
pub use identity::{built_in, built_in_services, chatgpt_service, scripted_service};
pub use request::{EFFORT_LADDER, Tuning, default_effort_label, effort_by_label, effort_label};
pub use stream::{CutShort, Delta, StepOut, SummaryOut};
pub use transport::Engine;
pub use usage::{Usage, UsageParts, humanize_tokens};

pub use genai::chat::{ReasoningEffort, StopReason, ToolCall};

use crate::agent::cancel;
use crate::record::model::Transcript;
use credential::{Credential, CredentialStore};
use models::{LiveSource, ModelCatalog};
use std::sync::Arc;
use transport::Transport;

/// Every file on this computer holding one of our own credentials: each
/// product's keychain fallback ([`keychain`]) and the `ChatGPT` tokens
/// ([`oauth`], exarch's state dir wherever the login was made).
///
/// [`crate::policy::for_invocation`] denies these to every session whose grant
/// attenuates the filesystem at all.  They need saying because the profiles
/// read `xdg:config` and `xdg:state` wholesale so tools find their configs,
/// and these sit in exactly that reach — the key that pays for the turn is not
/// a thing the agent may read back.  Environment-borne keys need no entry:
/// [`credential`] sweeps them out of the process before any session runs.
pub(crate) fn credential_files() -> Vec<std::path::PathBuf> {
    crate::bootstrap::APPS
        .iter()
        .map(|app| keychain::Keychain::for_app(*app).fallback_path())
        .chain(std::iter::once(oauth::token_path()))
        .collect()
}

/// A session's chosen model, tuning, and routing, plus the backend its
/// requests run on.
pub struct Provider {
    backend: Backend,
    account: Account,
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
    ///
    /// [`bureau::Bureau::build`] is the only caller: minting a provider needs
    /// a credential, and the bureau is what holds one.
    pub(crate) fn build(
        engine: Arc<Engine>,
        account: &Account,
        model: String,
        credential: &Credential,
        max_tokens_override: Option<u32>,
        tuning: Tuning,
        route: Option<String>,
    ) -> Self {
        let transport = engine.transport_for(account, &model, credential);
        Self {
            backend: Backend::Live { engine, transport },
            account: account.clone(),
            model,
            max_tokens_override,
            tuning,
            route,
        }
    }

    /// Build a provider that replays scripted outcomes instead of dialling out.
    pub fn scripted(model: &str, script: scripted::Script) -> Self {
        Self {
            backend: Backend::Scripted(script),
            account: Account::of_service(scripted_service()),
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

    /// The account this selection authenticates as.
    pub fn account(&self) -> &Account {
        &self.account
    }

    /// This model's context window when the pricing catalog knows it.
    pub fn context_window(&self) -> Option<u64> {
        pricing::context_window(&self.model)
    }

    /// Stream one assistant turn; `on_delta` fires per chunk, prose and
    /// reasoning in arrival order. Raced against `cancel`, so an interrupt
    /// need not wait on the next network chunk. `tool_enabled` gates our tool
    /// definitions, `search` the provider's own built-in web search.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete<F: FnMut(Delta<'_>)>(
        &self,
        system: &str,
        transcript: &Transcript,
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
                transcript,
                tool_enabled,
                search,
                on_delta,
                cancel,
            ),
            Backend::Scripted(script) => script.complete(&self.model, on_delta),
        }
    }

    /// The `OpenRouter` serving-provider pin, when this selection's service
    /// actually routes — never sent to a service for which the string would
    /// mean nothing.
    fn openrouter_route(&self) -> Option<&str> {
        if self.account.service.routes {
            self.route.as_deref()
        } else {
            None
        }
    }

    /// Summarise an already-rendered transcript.
    ///
    /// # Errors
    /// Returns an error once the bounded retry policy is exhausted.
    pub fn summarize(
        &self,
        system: &str,
        transcript: &Transcript,
        max_tokens: u32,
        cancel: &cancel::Token,
    ) -> Result<SummaryOut, ProviderError> {
        match &self.backend {
            Backend::Live { engine, transport } => engine.summarize(
                transport,
                &self.model,
                system,
                transcript,
                max_tokens,
                cancel,
            ),
            Backend::Scripted(script) => script.summarize(&self.model),
        }
    }
}

/// Admit a freshly signed-in `ChatGPT` token to the live store and catalog.
///
/// Returns who it now is and what it is now called — the same order for every
/// front-end, since which store an account lands in and its label are provider
/// facts. The id comes back as well as the label because a label answers only
/// *which of the accounts on offer*, and a caller asking whether this is the
/// account it already holds is asking about identity, not about display.
pub fn admit_login(
    store: &mut CredentialStore,
    catalog: &mut ModelCatalog<LiveSource>,
    token: &oauth::OAuthToken,
) -> (AccountId, String) {
    let (account, credential) = store.add_oauth(token);
    // The store's name for it, which says which account when two share an email.
    let label = identity::label(&account, &store.available());
    let id = account.id.clone();
    catalog.add_credential(account, credential);
    (id, label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_route_is_ignored_by_other_providers() {
        let mut provider = Provider::scripted("gpt-5.5", scripted::Script::new());
        provider.route = Some("deepinfra".into());
        assert_eq!(provider.openrouter_route(), None);

        provider.account =
            Account::of_service(built_in(&ServiceName::declared("openrouter").unwrap()).unwrap());
        assert_eq!(provider.openrouter_route(), Some("deepinfra"));
    }
}
