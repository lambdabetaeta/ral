//! The LLM boundary: one resolved model selection over a credential-bound
//! transport.
//!
//! History lives in the transcript the caller renders, not here; transport,
//! request shaping, retry, streaming, usage, and the per-account record of
//! what is left of an allowance ([`rations`]) each keep their own sibling
//! module.

pub mod accounts;
pub mod allowance;
pub mod bureau;
pub mod credential;
mod error;
pub mod identity;
pub mod keychain;
pub mod listing;
pub mod models;
pub mod oauth;
pub mod pricing;
mod rations;
mod request;
pub(crate) mod reset;
mod retry;
pub mod roles;
pub mod scripted;
pub mod state;
mod stream;
pub mod tls;
pub mod tools;
mod transport;
mod usage;
pub mod via;
mod wire;

pub use bureau::{Bureau, Choice, Holdings, Pick};
pub use error::{CancelSite, CutShort, Limit, ProviderError, Refusal};
pub(crate) use error::{error_object, extract_url};
pub use identity::{Account, AccountId, Auth, Billing, Login, Meter, Service, ServiceName};
pub use identity::{built_in, built_in_services, scripted_service};
pub use rations::Rations;
pub(crate) use request::Request;
pub use request::{EFFORT_LADDER, Tuning, default_effort_label, effort_by_label, effort_label};
pub use retry::Recovery;
pub use stream::{Delta, Served, StepOut};
pub use tools::Toolset;
pub use transport::Pool;
pub use usage::{Usage, UsageParts, humanize_tokens};
pub use via::Via;

pub use genai::chat::{ReasoningEffort, StopReason, ToolCall};

use allowance::{Allowance, LiveMeters};
use ral_core::process::Scope;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use transport::Transport;

/// Every file on this computer holding one of our own credentials, as
/// spellings: each product's keychain fallback ([`keychain`]) and the
/// sign-in tokens ([`oauth`]), all in a state directory; none where the
/// host names no state base.
///
/// [`crate::policy::for_invocation`] denies these to every session whose grant
/// attenuates the filesystem at all.  They need saying because the profiles
/// read `xdg:config` and `xdg:state` wholesale so tools find their configs,
/// and these sit in exactly that reach — the key that pays for the turn is not
/// a thing the agent may read back.  Environment-borne keys need no entry:
/// [`credential`] sweeps them out of the process before any session runs.
pub(crate) fn credential_files() -> Vec<std::path::PathBuf> {
    crate::app::APPS
        .iter()
        .filter_map(|app| keychain::Keychain::for_app(*app).fallback_path())
        .chain(oauth::token_path())
        .collect()
}

/// A model as chosen, recorded: what requests go out under.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selected {
    pub model: String,
    /// The catalog's window at the choice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// The account's label at the choice.
    pub label: String,
    pub service: String,
    pub account: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    pub tuning: Tuning,
    /// Shared plans carry no output cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl Selected {
    #[doc(hidden)]
    pub fn for_test(model: &str) -> Self {
        let account = Account::of_service(scripted_service());
        Self {
            model: model.to_string(),
            context_window: None,
            label: identity::label(&account, &[]),
            service: account.service.name.as_str().to_string(),
            account: account.id.as_str().to_string(),
            via: None,
            tuning: Tuning::default(),
            max_tokens: None,
        }
    }
}

/// An immutable client minted from a choice.
pub struct Provider {
    backend: Backend,
    account: Account,
    selected: Selected,
}

/// A live transport over the shared [`Pool`], or a scripted replay so
/// agent-loop tests never touch the network.
pub(in crate::provider) enum Backend {
    Live {
        engine: Arc<Pool>,
        transport: Arc<Transport>,
        /// This selection's meter, over the roster it was minted from: as
        /// fresh as its transport and no fresher — a rotated key reaches both
        /// at the next mint.
        meters: LiveMeters,
        rations: Arc<Rations>,
    },
    Scripted(scripted::Script),
}

impl Provider {
    /// Build a provider selection on `backend`.
    ///
    /// [`bureau::Bureau::build`] is the only caller that minds a live backend:
    /// minting one needs a credential, and the bureau is what holds one.
    pub(in crate::provider) fn build(
        backend: Backend,
        account: &Account,
        selected: Selected,
    ) -> Self {
        Self {
            backend,
            account: account.clone(),
            selected,
        }
    }

    /// Build a provider that replays scripted outcomes instead of dialling out.
    pub fn scripted(model: &str, script: scripted::Script) -> Self {
        Self::build(
            Backend::Scripted(script),
            &Account::of_service(scripted_service()),
            Selected::for_test(model),
        )
    }

    pub fn selected(&self) -> &Selected {
        &self.selected
    }

    /// The request tuning bound to this selection.
    pub fn tuning(&self) -> &Tuning {
        &self.selected.tuning
    }

    /// The resolved model name.
    pub fn model(&self) -> &str {
        &self.selected.model
    }

    /// The account this selection authenticates as.
    pub fn account(&self) -> &Account {
        &self.account
    }

    /// The upstreams this selection is served via; `None` leaves the choice
    /// to the service.
    pub fn via(&self) -> Option<&Via> {
        self.selected.via.as_ref()
    }

    /// The account, model and via this selection was minted for.
    pub fn choice(&self) -> Choice {
        Choice {
            account: self.account.clone(),
            model: self.selected.model.clone(),
            via: self.selected.via.clone(),
        }
    }

    /// The record's last reading of this selection's account; empty when
    /// scripted, or before one has landed.
    pub fn allowances(&self) -> Vec<Allowance> {
        match &self.backend {
            Backend::Live { rations, .. } => rations.reading(&self.account.id),
            Backend::Scripted(_) => Vec::new(),
        }
    }

    /// The windows of [`Self::allowances`] that newly reach a rung of
    /// `ladder`, told once per account; empty when scripted.
    pub(crate) fn climb_allowances(&self, ladder: &[u32]) -> Vec<Allowance> {
        match &self.backend {
            Backend::Live { rations, .. } => rations.climb(&self.account.id, ladder),
            Backend::Scripted(_) => Vec::new(),
        }
    }

    /// Stream one assistant turn; `on_delta` fires per chunk, prose and
    /// reasoning in arrival order. Raced against `cancel`, so an interrupt
    /// need not wait on the next network chunk.
    pub(crate) fn complete<F: FnMut(Delta<'_>)>(
        &self,
        request: Request<'_>,
        on_delta: &mut F,
        cancel: &Scope,
    ) -> Result<StepOut, ProviderError> {
        match &self.backend {
            Backend::Live {
                engine,
                transport,
                meters,
                rations,
            } => {
                rations.admit(&self.account, self.model(), meters)?;
                let outcome = engine.complete(transport, &self.selected, request, on_delta, cancel);
                rations.settle(&self.account.id, self.model(), &outcome);
                outcome
            }
            Backend::Scripted(script) => script.complete(self.model(), on_delta),
        }
    }
}

/// Admit a freshly committed login to the live store and catalog.
///
/// Returns who it now is and what it is now called — the same order for every
/// front-end, since which store an account lands in and its label are provider
/// facts. The id comes back as well as the label because a label answers only
/// *which of the accounts on offer*, and a caller asking whether this is the
/// account it already holds is asking about identity, not about display.
///
/// # Errors
/// The committed account has since been signed out.
pub fn admit_login(
    holdings: &Holdings,
    login: &oauth::CredentialHandle,
) -> Result<(AccountId, String), String> {
    holdings.change(|store| {
        let account = store.add_login(login)?;
        // The store's name for it, which says which account when two share an email.
        let label = identity::label(&account, &store.available());
        Ok((account.id, label))
    })
}
