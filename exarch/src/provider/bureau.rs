//! One owner for provider construction.
//!
//! [`Provider::build`] needs a backend over a [`Pool`] and a credential, and
//! a [`Provider`] retains neither, so every front-end wanting a second
//! selection has had to hold the engine and the [`Holdings`] as unrelated
//! locals. The bureau is those named once, and the only place a live
//! [`Provider`] is minted.
//!
//! Two arms, mirroring [`Provider`]'s own backends: [`Pool::new`] primes the
//! pricing catalog over the network, so a test holds [`Bureau::Scripted`],
//! which mints each provider from its next script.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::scripted::Script;

use ral_core::sync::LockExt;

use super::allowance::{LiveMeters, Survey};
use super::credential::CredentialStore;
use super::models::{Listed, LiveSource, ModelCatalog, ModelSource, ProviderEndpoint, listing_of};
use super::{Account, Backend, Pool, Provider, Rations, Selected, Tuning, Via, oauth, pricing};
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
/// a network call, a picker frame, or a machine boot. [`Holdings::change`] is
/// the one place both are held at once, store first. [`Rations`]' own lock is a
/// leaf, held for one map access and never with either. A UI thread must never
/// hold the store or catalog while waiting on an agent thread, which takes
/// both, one after the other, whenever it builds a provider.
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

    /// `model`'s serving endpoints: the catalog's, else fetched and recorded;
    /// the fetch's own error when it fails.
    fn endpoints_of(&self, model: &str) -> Result<Vec<ProviderEndpoint>, String> {
        let source = {
            let catalog = self.catalog.lock_ignore_poison();
            if let Some(cached) = catalog.cached_endpoints(model) {
                return Ok(cached);
            }
            catalog.source().clone()
        };
        let fetched = source.endpoints(model)?;
        self.catalog
            .lock_ignore_poison()
            .record_endpoints(model, fetched.clone());
        Ok(fetched)
    }

    /// Sign out of the login named `id`: delete its stored token and retire
    /// the account, returning the token for the caller to revoke. Blocking
    /// store work belongs on a frontend's background job boundary.
    ///
    /// # Errors
    /// If `id` names no known account or no signed-in login, or if the token
    /// store could not be rewritten.
    pub(crate) fn sign_out(&self, id: &str) -> Result<oauth::OAuthToken, String> {
        let (account, label) = {
            let store = self.store.lock_ignore_poison();
            let account = super::accounts::find(&store, id)?;
            let label = super::identity::label(&account, &store.available());
            drop(store);
            (account, label)
        };
        let token = oauth::sign_out(account.id.as_str())?
            .ok_or_else(|| format!("{label} is not signed in"))?;
        self.change(|store| store.retire_login(&account.id));
        Ok(token)
    }

    /// Change the credentials, and have the catalog list with them as they
    /// now stand.
    pub fn change<R>(&self, f: impl FnOnce(&mut CredentialStore) -> R) -> R {
        let mut store = self.store.lock_ignore_poison();
        let out = f(&mut store);
        self.catalog.lock_ignore_poison().set_roster(store.roster());
        out
    }
}

/// An account, one of its models, and the upstreams it is served via: what a
/// mint is asked for. `via` is `None` to leave the upstream to a routing
/// service, and must be for any other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub account: Account,
    pub model: String,
    pub via: Option<Via>,
}

/// A model and its tuning: what a human picks and an agent runs on.
#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    pub choice: Choice,
    pub tuning: Tuning,
}

impl Pick {
    /// What `provider` runs: its choice and its tuning.
    pub fn of(provider: &Provider) -> Self {
        Self {
            choice: provider.choice(),
            tuning: provider.tuning().clone(),
        }
    }
}

/// Where a session's providers come from.
pub enum Bureau {
    /// A live session: the engine its requests run on, over the application's
    /// shared holdings.
    Live {
        engine: Arc<Pool>,
        holdings: Holdings,
    },
    /// A scripted session: each mint takes the next script, and none is left
    /// unless given.
    Scripted(Mutex<VecDeque<Script>>),
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

/// The window `model` runs under via `via` on `account`: refused unless its
/// service routes and the model's endpoints, fetched by `endpoints`, list every
/// upstream `via` names; else the least its upstreams hold.
fn window_via(
    account: &Account,
    label: &str,
    model: &str,
    via: &Via,
    listed: Option<u64>,
    endpoints: impl FnOnce() -> Result<Vec<ProviderEndpoint>, String>,
) -> Result<Option<u64>, String> {
    if !account.service.routes {
        return Err(format!(
            "'{label}' does not route, so it cannot serve '{model}' via {via}"
        ));
    }
    let endpoints =
        endpoints().map_err(|why| format!("could not list the upstreams of '{model}': {why}"))?;
    via.window(&endpoints, listed)
        .map_err(|why| format!("'{model}' via {via}: {why}"))
}

/// The selection `account` can carry: a plan refuses sampling and an output
/// cap, so neither rides one; effort does.
fn selection(
    account: &Account,
    model: String,
    label: String,
    context_window: Option<u64>,
    tuning: &Tuning,
    via: Option<Via>,
    max_tokens: Option<u32>,
) -> Selected {
    let (tuning, max_tokens) = if account.service.shares_a_plan() {
        (
            Tuning {
                effort: tuning.effort.clone(),
                ..Tuning::default()
            },
            None,
        )
    } else {
        (tuning.clone(), max_tokens)
    };
    Selected {
        model,
        context_window,
        label,
        service: account.service.name.as_str().to_string(),
        account: account.id.as_str().to_string(),
        via,
        tuning,
        max_tokens,
    }
}

impl Bureau {
    /// A scripted bureau minting `scripts`, in order.
    pub fn scripted(scripts: impl IntoIterator<Item = Script>) -> Self {
        Self::Scripted(Mutex::new(scripts.into_iter().collect()))
    }

    /// Every account this session can authenticate as.
    pub fn available(&self) -> Vec<Account> {
        match self {
            Self::Live { holdings, .. } => holdings.store.lock_ignore_poison().available(),
            Self::Scripted(_) => Vec::new(),
        }
    }

    /// Mint `choice` on this session's engine — the door every
    /// selection passes (bar [`Provider::scripted`]): its account must list
    /// its model, and serve it via its upstreams.
    ///
    /// # Errors
    /// If a scripted bureau has no script left, if the account has no
    /// resolved credential, if its listing cannot be had or does not name the
    /// model, or if the via is refused: on a service that does not route, or
    /// naming an upstream the model's endpoints do not list.
    pub fn build(
        &self,
        choice: Choice,
        tuning: &Tuning,
        max_tokens: Option<u32>,
    ) -> Result<Arc<Provider>, String> {
        let Choice {
            account,
            model,
            via,
        } = choice;
        let (engine, holdings) = match self {
            Self::Live { engine, holdings } => (engine, holdings),
            Self::Scripted(scripts) => {
                let script = scripts.lock_ignore_poison().pop_front().ok_or_else(|| {
                    format!("this scripted session has no script left to mint {model}")
                })?;
                return Ok(Arc::new(Provider::build(
                    Backend::Scripted(script),
                    &account,
                    selection(
                        &account,
                        model,
                        super::identity::label(&account, &[]),
                        None,
                        tuning,
                        via,
                        max_tokens,
                    ),
                )));
            }
        };
        // Locked only long enough to clone the roster out; the reads the meter
        // later runs have the lock long released.
        let roster = holdings.store.lock_ignore_poison().roster();
        let label = roster.label(&account);
        let credential = roster
            .credential(&account.id)
            .ok_or_else(|| format!("{label} has no resolved credential"))?;
        let listing = listing_of(&holdings.catalog, &account.id)
            .map_err(|why| format!("{label} could not list its models: {why}"))?;
        let listed = window_of(&listing, &model, &label, pricing::context_window)?;
        let context_window = match &via {
            None => listed,
            Some(via) => window_via(&account, &label, &model, via, listed, || {
                holdings.endpoints_of(&model)
            })?,
        };
        let transport = engine.transport_for(&account, &model, credential);
        let backend = Backend::Live {
            engine: Arc::clone(engine),
            transport,
            meters: LiveMeters::new(roster),
            rations: Arc::clone(&holdings.rations),
        };
        Ok(Arc::new(Provider::build(
            backend,
            &account,
            selection(
                &account,
                model,
                label,
                context_window,
                tuning,
                via,
                max_tokens,
            ),
        )))
    }

    /// Admit a freshly committed login, returning who it now is and what it
    /// is now called.
    ///
    /// # Errors
    /// If the bureau is scripted, which has no store to admit into, or the
    /// login has since been signed out.
    pub fn admit(
        &self,
        login: &oauth::CredentialHandle,
    ) -> Result<(super::AccountId, String), String> {
        let Self::Live { holdings, .. } = self else {
            return Err("a scripted session keeps no credentials to admit a login into".into());
        };
        super::admit_login(holdings, login)
    }

    /// The shared credentials and catalog; `None` when the bureau is scripted
    /// and keeps no accounts.
    pub fn holdings(&self) -> Option<&Holdings> {
        match self {
            Self::Live { holdings, .. } => Some(holdings),
            Self::Scripted(_) => None,
        }
    }

    /// Run `f` against the model catalog, locked for exactly that call —
    /// `None` when the bureau is scripted and has no catalog.
    pub fn with_catalog<R>(&self, f: impl FnOnce(&mut ModelCatalog<LiveSource>) -> R) -> Option<R> {
        match self {
            Self::Live { holdings, .. } => Some(f(&mut holdings.catalog.lock_ignore_poison())),
            Self::Scripted(_) => None,
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
    fn window_of_prefers_the_listing_to_the_fallback() {
        let models = [listed("m", Some(10))];
        assert_eq!(window_of(&models, "m", "acct", |_| Some(99)), Ok(Some(10)));
    }

    #[test]
    fn a_listed_model_without_a_window_falls_back() {
        let models = [listed("m", None)];
        assert_eq!(window_of(&models, "m", "acct", |_| Some(99)), Ok(Some(99)));
        assert_eq!(window_of(&models, "m", "acct", |_| None), Ok(None));
    }

    fn endpoint(tag: &str, window: Option<u64>) -> ProviderEndpoint {
        ProviderEndpoint {
            provider_name: tag.into(),
            tag: tag.into(),
            context_length: window,
            quantization: None,
        }
    }

    #[test]
    fn a_via_on_a_service_that_does_not_route_is_refused() {
        let via = Via::parse("deepinfra").unwrap();
        let err = window_via(
            &Account::built_in("anthropic"),
            "anthropic",
            "m",
            &via,
            None,
            || panic!("a service that does not route lists no upstreams"),
        )
        .unwrap_err();
        assert_eq!(
            err,
            "'anthropic' does not route, so it cannot serve 'm' via deepinfra"
        );
    }

    #[test]
    fn a_via_whose_upstreams_cannot_be_listed_is_refused() {
        let via = Via::parse("deepinfra").unwrap();
        let err = window_via(
            &Account::built_in("openrouter"),
            "openrouter",
            "m",
            &via,
            None,
            || Err("network down".into()),
        )
        .unwrap_err();
        assert_eq!(err, "could not list the upstreams of 'm': network down");
    }

    #[test]
    fn a_routed_via_gets_the_least_window_its_upstreams_hold() {
        let via = Via::parse("ionet,deepinfra/fp8").unwrap();
        let window = window_via(
            &Account::built_in("openrouter"),
            "openrouter",
            "m",
            &via,
            Some(64_000),
            || {
                Ok(vec![
                    endpoint("ionet", Some(262_000)),
                    endpoint("deepinfra/fp8", Some(131_072)),
                ])
            },
        );
        assert_eq!(window, Ok(Some(131_072)));
    }

    #[test]
    fn only_chatgpt_selection_carries_effort_alone() {
        let tuning = Tuning {
            effort: Some(ReasoningEffort::Medium),
            temperature: Some(0.5),
            top_p: Some(0.9),
        };
        let pick = |account: &Account| {
            selection(
                account,
                "m".into(),
                super::super::identity::label(account, &[]),
                None,
                &tuning,
                None,
                Some(10),
            )
        };

        let plan = pick(&Account::login(super::super::Login::Chatgpt, "a", "a@x"));
        assert!(matches!(plan.tuning.effort, Some(ReasoningEffort::Medium)));
        assert_eq!((plan.tuning.temperature, plan.tuning.top_p), (None, None));
        assert_eq!(plan.max_tokens, None);

        for account in [
            Account::built_in("openai"),
            Account::login(super::super::Login::Codex, "a", "a@x"),
            Account::login(super::super::Login::Grok, "a", "a@x"),
        ] {
            let selection = pick(&account);
            assert!(matches!(
                selection.tuning.effort,
                Some(ReasoningEffort::Medium)
            ));
            assert_eq!(selection.tuning.temperature, Some(0.5));
            assert_eq!(selection.tuning.top_p, Some(0.9));
            assert_eq!(selection.max_tokens, Some(10));
        }
    }

    #[test]
    fn an_unlisted_model_is_refused_naming_both_halves() {
        let err = window_of(&[listed("other", None)], "m", "acct", |_| Some(99)).unwrap_err();
        assert!(err.contains("'acct'") && err.contains("'m'"), "got: {err}");
    }
}
