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
use super::models::{Listed, LiveSource, ModelCatalog, listing_of};
use super::{Account, Backend, Engine, Provider, Rations, Selection, Tuning, oauth, pricing};
use crate::app::App;

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
/// hold the store or catalog while waiting on an agent thread, which takes
/// both, one after the other, whenever it mints a child's provider.
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

/// `model`'s context window, refusing it unless `listed`, the serving
/// account's listing, names it: the listing's own figure, else `fallback`'s.
fn window_of(
    listed: &[Listed],
    model: &str,
    label: &str,
    fallback: impl FnOnce(&str) -> Option<u64>,
) -> Result<Option<u64>, String> {
    let entry = listed
        .iter()
        .find(|m| m.id == model)
        .ok_or_else(|| format!("'{label}' does not list model '{model}'"))?;
    Ok(entry.context_window.or_else(|| fallback(model)))
}

/// The selection `account` can carry: a plan refuses sampling and an output
/// cap, so neither rides one; effort does.
fn selection(
    account: &Account,
    model: String,
    tuning: &Tuning,
    route: Option<String>,
    max_tokens: Option<u32>,
) -> Selection {
    if account.service.shares_a_plan() {
        Selection {
            model,
            max_tokens_override: None,
            tuning: Tuning {
                effort: tuning.effort.clone(),
                ..Tuning::default()
            },
            route,
        }
    } else {
        Selection {
            model,
            max_tokens_override: max_tokens,
            tuning: tuning.clone(),
            route,
        }
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
            Self::Live { holdings, .. } => holdings.store.lock_ignore_poison().available(),
            Self::Scripted => Vec::new(),
        }
    }

    /// Mint a provider on this session's engine — the only caller of
    /// [`Provider::build`], and so the one door every selection passes:
    /// `account` must list `model`.
    ///
    /// # Errors
    /// If the bureau is scripted, if `account` has no resolved credential, if
    /// its listing cannot be had, or if the listing does not name `model`.
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
        let label = roster.label(account);
        let credential = roster
            .credential(&account.id)
            .ok_or_else(|| format!("{label} has no resolved credential"))?;
        let listed = listing_of(&holdings.catalog, &account.id)
            .map_err(|why| format!("could not list the models '{label}' serves: {why}"))?;
        let context_window = window_of(&listed, &model, &label, pricing::context_window)?;
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
            selection(account, model, tuning, route, max_tokens),
            context_window,
        )))
    }

    /// Mint a provider that differs from `current` only in its account and
    /// model — the spawn's door.
    ///
    /// Tuning and the output cap are the operator's knobs rather than part of
    /// a model's identity, so they carry across; the route was chosen for
    /// `current`'s model, so it does not.
    ///
    /// # Errors
    /// As [`Bureau::build`].
    pub fn reselect(
        &self,
        current: &Provider,
        account: &Account,
        model: String,
    ) -> Result<Arc<Provider>, String> {
        self.build(
            account,
            model,
            current.tuning(),
            None,
            current.max_tokens_override(),
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
    use genai::chat::ReasoningEffort;

    fn listed(id: &str, window: Option<u64>) -> Listed {
        Listed {
            id: id.into(),
            context_window: window,
        }
    }

    #[test]
    fn a_listed_window_beats_the_fallback() {
        let models = [listed("m", Some(10))];
        assert_eq!(window_of(&models, "m", "acct", |_| Some(99)), Ok(Some(10)));
    }

    #[test]
    fn a_listed_model_without_a_window_falls_back() {
        let models = [listed("m", None)];
        assert_eq!(window_of(&models, "m", "acct", |_| Some(99)), Ok(Some(99)));
        assert_eq!(window_of(&models, "m", "acct", |_| None), Ok(None));
    }

    #[test]
    fn a_plan_selection_carries_effort_alone() {
        let tuning = Tuning {
            effort: Some(ReasoningEffort::Medium),
            temperature: Some(0.5),
            top_p: Some(0.9),
        };
        let pick = |account: &Account| selection(account, "m".into(), &tuning, None, Some(10));

        let plan = pick(&Account::chatgpt("a", "a@x"));
        assert!(matches!(plan.tuning.effort, Some(ReasoningEffort::Medium)));
        assert_eq!((plan.tuning.temperature, plan.tuning.top_p), (None, None));
        assert_eq!(plan.max_tokens_override, None);

        let keyed = pick(&Account::built_in("openai"));
        assert_eq!(keyed.tuning.temperature, Some(0.5));
        assert_eq!(keyed.tuning.top_p, Some(0.9));
        assert_eq!(keyed.max_tokens_override, Some(10));
    }

    #[test]
    fn an_unlisted_model_is_refused_naming_both_halves() {
        let err = window_of(&[listed("other", None)], "m", "acct", |_| Some(99)).unwrap_err();
        assert!(err.contains("'acct'") && err.contains("'m'"), "got: {err}");
    }
}
