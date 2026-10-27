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

pub use bureau::{Bureau, Choice, Holdings};
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
pub use transport::Engine;
pub use usage::{Usage, UsageParts, humanize_tokens};
pub use via::Via;

pub use genai::chat::{ReasoningEffort, StopReason, ToolCall};

use crate::cancel;
use allowance::{Allowance, LiveMeters};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// The model and the knobs a selection fixes, apart from the account.
pub(crate) struct Selection {
    pub(crate) model: String,
    pub(crate) max_tokens_override: Option<u32>,
    pub(crate) tuning: Tuning,
    /// `Some` only on a service that routes — [`Bureau::build`] refuses it
    /// elsewhere.
    pub(crate) via: Option<Via>,
}

/// A session's chosen model, tuning, and upstreams, plus the backend its
/// requests run on.
pub struct Provider {
    backend: Backend,
    account: Account,
    selection: Selection,
    /// Resolved when the bureau mints this selection, then only ever narrowed
    /// by an overflow; 0 for unknown.
    context_window: AtomicU64,
}

/// A live transport over the shared [`Engine`], or a scripted replay so
/// agent-loop tests never touch the network.
pub(in crate::provider) enum Backend {
    Live {
        engine: Arc<Engine>,
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
        selection: Selection,
        context_window: Option<u64>,
    ) -> Self {
        Self {
            backend,
            account: account.clone(),
            selection,
            context_window: AtomicU64::new(context_window.unwrap_or(0)),
        }
    }

    /// Build a provider that replays scripted outcomes instead of dialling out.
    pub fn scripted(model: &str, script: scripted::Script) -> Self {
        Self::build(
            Backend::Scripted(script),
            &Account::of_service(scripted_service()),
            Selection {
                model: model.to_string(),
                max_tokens_override: None,
                tuning: Tuning::default(),
                via: None,
            },
            None,
        )
    }

    /// The user-supplied output cap, or `None` for the adapter default.
    pub fn max_tokens_override(&self) -> Option<u32> {
        self.selection.max_tokens_override
    }

    /// The request tuning bound to this selection.
    pub fn tuning(&self) -> &Tuning {
        &self.selection.tuning
    }

    /// The resolved model name.
    pub fn model(&self) -> &str {
        &self.selection.model
    }

    /// The account this selection authenticates as.
    pub fn account(&self) -> &Account {
        &self.account
    }

    /// The upstreams this selection is served via; `None` leaves the choice
    /// to the service.
    pub fn via(&self) -> Option<&Via> {
        self.selection.via.as_ref()
    }

    /// The account, model and via this selection was minted for.
    pub fn choice(&self) -> Choice {
        Choice {
            account: self.account.clone(),
            model: self.selection.model.clone(),
            via: self.selection.via.clone(),
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

    /// This model's context window: the catalog's figure, or a tighter one an
    /// overflow taught.
    pub fn context_window(&self) -> Option<u64> {
        Some(self.context_window.load(Ordering::Relaxed)).filter(|&w| w > 0)
    }

    /// Learn that the endpoint holds only `limit`; whether that narrowed the window.
    pub(crate) fn narrow_window(&self, limit: u64) -> bool {
        self.context_window
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |w| {
                (w == 0 || w > limit).then_some(limit)
            })
            .is_ok()
    }

    /// Stream one assistant turn; `on_delta` fires per chunk, prose and
    /// reasoning in arrival order. Raced against `cancel`, so an interrupt
    /// need not wait on the next network chunk.
    pub(crate) fn complete<F: FnMut(Delta<'_>)>(
        &self,
        request: Request<'_>,
        on_delta: &mut F,
        cancel: &cancel::Token,
    ) -> Result<StepOut, ProviderError> {
        let outcome = match &self.backend {
            Backend::Live {
                engine,
                transport,
                meters,
                rations,
            } => {
                rations.admit(&self.account, self.model(), meters)?;
                let outcome =
                    engine.complete(transport, &self.selection, request, on_delta, cancel);
                rations.settle(&self.account.id, self.model(), &outcome);
                outcome
            }
            Backend::Scripted(script) => script.complete(self.model(), on_delta),
        };
        outcome.map_err(|mut error| {
            if let ProviderError::Overflow { limit, listed, .. } = &mut error {
                *listed = self.context_window();
                if let Some(l) = *limit {
                    self.narrow_window(l);
                }
            }
            error
        })
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
