//! One owner for provider construction.
//!
//! [`Provider::build`] needs a backend over an [`Engine`] and a credential, and
//! a [`Provider`] retains neither, so every front-end wanting a second
//! selection has had to hold the engine and the [`Holdings`] as unrelated
//! locals. The bureau is those named once, and the only place a live
//! [`Provider`] is minted.
//!
//! Two arms, mirroring [`Provider`]'s own backends: [`Engine::new`] primes the
//! pricing catalog over the network, so a unit test holds
//! [`Bureau::Scripted`], which mints nothing and says so.

use std::sync::{Arc, Mutex};

use ral_core::sync::LockExt;

use super::allowance::{LiveMeters, Survey};
use super::credential::CredentialStore;
use super::models::{Listed, LiveSource, ModelCatalog, ModelSource};
use super::{Account, Backend, Engine, Provider, Rations, Tuning, identity, oauth, pricing};
use crate::bootstrap::App;

/// The credentials, model catalog, and allowance record an application shares.
///
/// Exarch builds one bureau over its own; synod keeps them as
/// application-wide state and mints an engine per conversation.
///
/// The store and catalog are two mutexes and not one so that a credential
/// read never queues behind a catalog fold, which writes the disk cache under
/// its lock.
///
/// Lock discipline: the store and catalog are locked briefly, and never across
/// a network call, a picker frame, or a machine boot. [`Bureau::admit`] is the
/// one place both are held at once, store first. [`Rations`]' own lock is a
/// leaf, held for one map access and never with either. A UI thread must never
/// hold the store or catalog while waiting on an agent thread, which takes the
/// store whenever it mints a child's provider.
#[derive(Clone)]
pub struct Holdings {
    pub store: Arc<Mutex<CredentialStore>>,
    pub catalog: Arc<Mutex<ModelCatalog<LiveSource>>>,
    pub rations: Arc<Rations>,
}

impl Holdings {
    /// Holdings over `store`, with a catalog cached under `app`'s directories.
    pub fn new(store: CredentialStore, app: App) -> Self {
        let catalog = ModelCatalog::new(LiveSource::new(&store), app);
        Self {
            store: Arc::new(Mutex::new(store)),
            catalog: Arc::new(Mutex::new(catalog)),
            rations: Arc::new(Rations::default()),
        }
    }
}

/// Where a session's providers come from.
pub enum Bureau {
    /// A live session: the engine its requests run on, over the application's
    /// shared holdings.
    Live {
        engine: Arc<Engine>,
        holdings: Holdings,
    },
    /// A scripted session mints nothing.
    Scripted,
}

/// The `OpenRouter` route names a serving provider and means nothing on
/// another account, so it survives only where the account is unchanged.
fn route_across(current: &Provider, account: &Account) -> Option<String> {
    current
        .route
        .clone()
        .filter(|_| current.account.id == account.id)
}

/// `model`'s window: the serving account's own listing if it reports one, else
/// `fallback`, else unknown.
pub(super) fn resolve_window(
    listed: Option<&[Listed]>,
    model: &str,
    fallback: impl FnOnce(&str) -> Option<u64>,
) -> Option<u64> {
    listed
        .and_then(|models| models.iter().find(|m| m.id == model))
        .and_then(|m| m.context_window)
        .or_else(|| fallback(model))
}

/// Refuse `model` unless `listed`, the serving account's listing, holds it.
fn served(listed: Option<&[Listed]>, model: &str, label: &str) -> Result<(), String> {
    let listed = listed
        .ok_or_else(|| format!("could not list the models '{label}' serves to check '{model}'"))?;
    if listed.iter().any(|m| m.id == model) {
        Ok(())
    } else {
        Err(format!(
            "'{label}' does not list model '{model}' — name one it serves, or a provider that \
             serves this one"
        ))
    }
}

/// `account`'s listing: the catalog's cache, else one fetch made with the
/// catalog unlocked. A failed fetch is no listing.
fn listing_of(holdings: &Holdings, account: &Account) -> Option<Vec<Listed>> {
    let source = {
        let mut catalog = holdings.catalog.lock_ignore_poison();
        if let Some(listed) = catalog.cached_listing(&account.id) {
            return Some(listed);
        }
        catalog.source().clone()
    };
    let listed = source.list(&account.id).ok()?;
    holdings
        .catalog
        .lock_ignore_poison()
        .record(&account.id, listed.clone());
    Some(listed)
}

/// What a scripted bureau answers every request for a provider with.
fn mints_nothing() -> String {
    "this session replays a scripted provider and mints no others".into()
}

impl Bureau {
    /// Every account this session can authenticate as.
    pub fn available(&self) -> Vec<Account> {
        match self {
            Self::Live { holdings, .. } => holdings.store.lock_ignore_poison().available(),
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
        let Self::Live { engine, holdings } = self else {
            return Err(mints_nothing());
        };
        // Locked only long enough to clone the roster out; the reads the meter
        // later runs have the lock long released.
        let roster = holdings.store.lock_ignore_poison().roster();
        let credential = roster
            .credential(&account.id)
            .ok_or_else(|| format!("{} has no resolved credential", roster.label(account)))?;
        let context_window = resolve_window(
            listing_of(holdings, account).as_deref(),
            &model,
            pricing::context_window,
        );
        let transport = engine.transport_for(account, &model, credential);
        let backend = Backend::Live {
            engine: Arc::clone(engine),
            transport,
            meters: LiveMeters::new(roster),
            rations: Arc::clone(&holdings.rations),
        };
        Ok(Arc::new(Provider::build(
            backend,
            account,
            model,
            max_tokens,
            tuning.clone(),
            route,
            context_window,
        )))
    }

    /// Mint a provider that differs from `current` only in its account and
    /// model — the spawn's door, where a child's selection is checked against
    /// the account's own listing.
    ///
    /// Tuning and the output cap are the operator's knobs rather than part of
    /// a model's identity, so they carry across whatever the selection.
    ///
    /// # Errors
    /// As [`Bureau::build`], and if `account` does not list `model`.
    pub fn reselect(
        &self,
        current: &Provider,
        account: &Account,
        model: String,
    ) -> Result<Arc<Provider>, String> {
        let Self::Live { holdings, .. } = self else {
            return Err(mints_nothing());
        };
        served(
            listing_of(holdings, account).as_deref(),
            &model,
            &identity::label(account, &self.available()),
        )?;
        self.build(
            account,
            model,
            &current.tuning,
            route_across(current, account),
            current.max_tokens_override,
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
        let Self::Live { holdings, .. } = self else {
            return Err(mints_nothing());
        };
        let mut store = holdings.store.lock_ignore_poison();
        let mut catalog = holdings.catalog.lock_ignore_poison();
        Ok(super::admit_login(&mut store, &mut catalog, token))
    }

    /// Run `f` against the model catalog, locked for exactly that call —
    /// `None` when the bureau is scripted and has no catalog.
    pub fn with_catalog<R>(&self, f: impl FnOnce(&mut ModelCatalog<LiveSource>) -> R) -> Option<R> {
        match self {
            Self::Live { holdings, .. } => Some(f(&mut holdings.catalog.lock_ignore_poison())),
            Self::Scripted => None,
        }
    }

    /// Open a survey over every available account. `None` when the bureau
    /// is scripted and has no store — the command then says so rather than
    /// drawing an empty card.
    pub fn survey_allowances(&self) -> Option<Survey> {
        let Self::Live { holdings, .. } = self else {
            return None;
        };
        let roster = holdings.store.lock_ignore_poison().roster();
        Some(Survey::open(
            &roster,
            &LiveMeters::new(roster.clone()),
            Arc::clone(&holdings.rations),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::scripted::Script;

    fn parent() -> Provider {
        let mut provider = Provider::scripted("gpt-5.5", Script::new());
        provider.account = Account::built_in("openrouter");
        provider.route = Some("deepinfra".into());
        provider.tuning.temperature = Some(0.4);
        provider.max_tokens_override = Some(4096);
        provider
    }

    #[test]
    fn the_route_survives_on_the_parents_own_account() {
        let parent = parent();
        assert_eq!(
            route_across(&parent, parent.account()).as_deref(),
            Some("deepinfra")
        );
    }

    #[test]
    fn the_route_is_dropped_on_another_account() {
        let parent = parent();
        assert_eq!(route_across(&parent, &Account::built_in("anthropic")), None);
    }

    fn listed(id: &str, window: Option<u64>) -> Listed {
        Listed {
            id: id.into(),
            context_window: window,
        }
    }

    #[test]
    fn a_listed_window_beats_the_fallback() {
        let models = [listed("m", Some(10))];
        assert_eq!(resolve_window(Some(&models), "m", |_| Some(99)), Some(10));
    }

    #[test]
    fn a_listed_model_without_a_window_falls_back() {
        let models = [listed("m", None)];
        assert_eq!(resolve_window(Some(&models), "m", |_| Some(99)), Some(99));
    }

    #[test]
    fn an_unlisted_model_falls_back() {
        let models = [listed("other", Some(10))];
        assert_eq!(resolve_window(Some(&models), "m", |_| Some(99)), Some(99));
        assert_eq!(resolve_window(None, "m", |_| Some(99)), Some(99));
    }

    #[test]
    fn no_listing_and_no_fallback_is_unknown() {
        assert_eq!(resolve_window(None, "m", |_| None), None);
    }

    #[test]
    fn a_listed_model_is_served() {
        assert_eq!(served(Some(&[listed("m", None)]), "m", "acct"), Ok(()));
    }

    #[test]
    fn an_unlisted_model_is_refused_naming_both_halves() {
        let err = served(Some(&[listed("other", None)]), "m", "acct").unwrap_err();
        assert!(err.contains("'acct'") && err.contains("'m'"), "got: {err}");
    }

    #[test]
    fn no_listing_refuses_rather_than_guessing() {
        assert!(served(None, "m", "acct").is_err());
    }
}
