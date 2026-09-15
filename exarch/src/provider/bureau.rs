//! One owner for provider construction.
//!
//! [`Provider::build`] needs an [`Engine`] and a [`Credential`], and a
//! [`Provider`] retains neither, so every front-end wanting a second
//! selection has had to hold the engine, the credential store, and the model
//! catalog as three unrelated locals. The bureau is that trio named once, and
//! the only place a live [`Provider`] is minted.
//!
//! Two arms, mirroring [`Provider`]'s own backends: [`Engine::new`] primes the
//! pricing catalog over the network, so a unit test holds
//! [`Bureau::Scripted`], which mints nothing and says so.
//!
//! The store and catalog are *shared* halves rather than owned ones, so two
//! hosts compose: exarch builds one bureau over its own pair, while synod
//! keeps the same pair as application-wide state and mints an engine per
//! conversation. They are two mutexes and not one because the spawn path
//! touches only the store and the picker's pump only the catalog; under a
//! single lock a spawn would queue behind a model-list fetch for no reason.
//!
//! Lock discipline, inherited from synod's own rule: both halves are locked
//! briefly, and never across a network call, a picker frame, or a machine
//! boot. [`Bureau::admit`] is the one place both are held at once, store
//! first. The converse obligation binds the other way too: a UI thread must
//! never hold either lock while waiting on an agent thread, which takes the
//! store whenever it mints a child's provider.

use std::sync::{Arc, Mutex};

use ral_core::sync::LockExt;

use super::allowance::{LiveMeters, Survey};
use super::credential::CredentialStore;
use super::models::{LiveSource, ModelCatalog};
use super::{Account, Engine, Provider, Tuning, identity, oauth};

/// Where a session's providers come from.
pub enum Bureau {
    /// A live session: the engine its requests run on, over the credentials
    /// and catalog the whole application shares.
    Live {
        engine: Arc<Engine>,
        store: Arc<Mutex<CredentialStore>>,
        catalog: Arc<Mutex<ModelCatalog<LiveSource>>>,
    },
    /// A scripted session mints nothing.
    Scripted,
}

/// What a provider is built from, decided before anything is minted — so the
/// decision is testable without an [`Engine`], and hence without the network.
struct Blueprint {
    account: Account,
    model: String,
    tuning: Tuning,
    route: Option<String>,
    max_tokens: Option<u32>,
}

/// The build order for `account` and `model`, everything else inherited from
/// `current`.
///
/// Tuning and the output cap are the operator's knobs rather than part of a
/// model's identity, so they carry across whatever the selection. The
/// `OpenRouter` route names a serving provider and means nothing on another
/// account, so it survives only where the account is unchanged.
fn blueprint(current: &Provider, account: &Account, model: String) -> Blueprint {
    Blueprint {
        route: current
            .route
            .clone()
            .filter(|_| current.account.id == account.id),
        account: account.clone(),
        model,
        tuning: current.tuning.clone(),
        max_tokens: current.max_tokens_override,
    }
}

/// What a scripted bureau answers every request for a provider with.
fn mints_nothing() -> String {
    "this session replays a scripted provider and mints no others".into()
}

impl Bureau {
    /// Every account this session can authenticate as.
    pub fn available(&self) -> Vec<Account> {
        match self {
            Self::Live { store, .. } => store.lock_ignore_poison().available(),
            Self::Scripted => Vec::new(),
        }
    }

    /// Mint a provider on this session's engine — the only caller of
    /// [`Provider::build`].
    ///
    /// # Errors
    /// If the bureau is scripted, or if `account` has no resolved credential.
    pub fn build(
        &self,
        account: &Account,
        model: String,
        tuning: &Tuning,
        route: Option<String>,
        max_tokens: Option<u32>,
    ) -> Result<Arc<Provider>, String> {
        let Self::Live { engine, store, .. } = self else {
            return Err(mints_nothing());
        };
        let credential = {
            let store = store.lock_ignore_poison();
            store.get(&account.id).cloned().ok_or_else(|| {
                format!(
                    "{} has no resolved credential",
                    identity::label(account, &store.available())
                )
            })?
        };
        Ok(Arc::new(Provider::build(
            engine.clone(),
            account,
            model,
            &credential,
            max_tokens,
            tuning.clone(),
            route,
        )))
    }

    /// Mint a provider that differs from `current` only in its account and
    /// model — the spawn's door, where a child's selection is decided.
    ///
    /// # Errors
    /// As [`Bureau::build`].
    pub fn reselect(
        &self,
        current: &Provider,
        account: &Account,
        model: String,
    ) -> Result<Arc<Provider>, String> {
        let plan = blueprint(current, account, model);
        self.build(
            &plan.account,
            plan.model,
            &plan.tuning,
            plan.route,
            plan.max_tokens,
        )
    }

    /// Admit a freshly signed-in `ChatGPT` token, returning who it now is and
    /// what it is now called.
    ///
    /// The one door that holds both halves at once — store, then catalog —
    /// so no caller has to nest them itself.
    ///
    /// # Errors
    /// If the bureau is scripted, which has no store to admit into.
    pub fn admit(&self, token: &oauth::OAuthToken) -> Result<(super::AccountId, String), String> {
        let Self::Live { store, catalog, .. } = self else {
            return Err(mints_nothing());
        };
        let mut store = store.lock_ignore_poison();
        let mut catalog = catalog.lock_ignore_poison();
        Ok(super::admit_login(&mut store, &mut catalog, token))
    }

    /// Run `f` against the model catalog, locked for exactly that call —
    /// `None` when the bureau is scripted and has no catalog.
    pub fn with_catalog<R>(&self, f: impl FnOnce(&mut ModelCatalog<LiveSource>) -> R) -> Option<R> {
        match self {
            Self::Live { catalog, .. } => Some(f(&mut catalog.lock_ignore_poison())),
            Self::Scripted => None,
        }
    }

    /// Open a survey over every available account. `None` when the bureau is
    /// scripted and has no store — the command then says so rather than
    /// drawing an empty card.
    pub fn survey_allowances(&self) -> Option<Survey> {
        let Self::Live { store, .. } = self else {
            return None;
        };
        // Locked only long enough to clone the roster out; the fetches
        // `Survey::open` spawns run with the lock long released.
        let roster = store.lock_ignore_poison().roster();
        Some(Survey::open(&roster, &LiveMeters::new(roster.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::scripted::Script;
    use crate::provider::{ServiceName, built_in};

    fn parent() -> Provider {
        let mut provider = Provider::scripted("gpt-5.5", Script::new());
        provider.account =
            Account::of_service(built_in(&ServiceName::declared("openrouter").unwrap()).unwrap());
        provider.route = Some("deepinfra".into());
        provider.tuning.temperature = Some(0.4);
        provider.max_tokens_override = Some(4096);
        provider
    }

    #[test]
    fn a_blueprint_names_the_account_and_model_it_was_given() {
        let parent = parent();
        let plan = blueprint(&parent, parent.account(), "gpt-5.1-mini".into());
        assert_eq!(plan.account.id, parent.account().id);
        assert_eq!(plan.model, "gpt-5.1-mini");
    }

    #[test]
    fn a_blueprint_inherits_the_tuning_and_the_output_cap() {
        let parent = parent();
        let other =
            Account::of_service(built_in(&ServiceName::declared("anthropic").unwrap()).unwrap());
        let plan = blueprint(&parent, &other, "claude-haiku-4-5".into());
        assert_eq!(plan.tuning.temperature, Some(0.4));
        assert_eq!(plan.max_tokens, Some(4096));
    }

    #[test]
    fn a_blueprint_keeps_the_route_on_the_parents_own_account() {
        let parent = parent();
        let plan = blueprint(&parent, parent.account(), "gpt-5.1".into());
        assert_eq!(plan.route.as_deref(), Some("deepinfra"));
    }

    #[test]
    fn a_blueprint_drops_the_route_on_another_account() {
        let parent = parent();
        let other =
            Account::of_service(built_in(&ServiceName::declared("anthropic").unwrap()).unwrap());
        let plan = blueprint(&parent, &other, "claude-haiku-4-5".into());
        assert_eq!(plan.route, None);
    }
}
