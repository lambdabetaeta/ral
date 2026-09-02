//! The provider/model picker: what [`crate::session::menu`] and
//! [`crate::session::refresh_menu`] shape for the window to render.

use exarch::provider::{
    self,
    credential::CredentialStore,
    identity::{self, Account},
    listing::Listing,
    models::{ModelCatalog, ModelSource},
    pricing,
};
use ral_core::sync::LockExt;
use std::sync::Mutex;

/// One model offered for a provider, and whether it takes a reasoning-effort
/// control.
///
/// `reasoning` reads `true` whenever the pricing catalog has not positively
/// said otherwise — before it is loaded, or for a model its fetch never
/// listed, [`pricing::ModelCaps::default`] is empty and `supports` treats
/// that as permission rather than refusal. This is the same
/// only-gray-on-positive-absence rule exarch's `/model` picker applies to
/// its highlighted row's parameters.
#[derive(serde::Serialize, Clone, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub struct ModelChoice {
    pub name: String,
    pub reasoning: bool,
}

/// One account the window can offer, and the models known for it.
#[derive(serde::Serialize, Clone, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub struct ProviderChoice {
    /// The [`AccountId`](exarch::provider::identity::AccountId) rendering —
    /// the identifier that round-trips as [`crate::session::Choice::account`]
    /// through [`crate::session::Conversation::begin`], which resolves it
    /// back to an [`Account`] via
    /// [`resolve_account`](exarch::provider::models::resolve_account). Never
    /// displayed; [`Self::label`] is.
    pub account: String,
    /// What the window shows for this entry — [`identity::label`],
    /// set-relative to every account [`menu`] or [`refresh_menu`] offers.
    pub label: String,
    pub default_model: Option<String>,
    /// Whatever the catalog honestly knows for this provider — at minimum
    /// the famous default, never blocking on a network fetch to build.
    pub models: Vec<ModelChoice>,
}

/// The provider picker: one entry per available account, plus the shared
/// effort ladder every entry's models offer a rung from.
#[derive(serde::Serialize, Clone, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub struct ModelMenu {
    pub providers: Vec<ProviderChoice>,
    /// [`provider::EFFORT_LADDER`]'s labels, ascending — the rungs
    /// [`crate::session::Choice::effort`] may name.
    pub efforts: Vec<String>,
    /// [`provider::default_effort_label`] — the rung a freshly-opened
    /// control should land on.
    pub default_effort: String,
}

/// The provider picker as it can be shown the instant the window opens.
///
/// No network touched: each provider's models come from whatever `catalog`
/// already has cached — a fresh disk entry carried over from an earlier
/// session, or nothing at all — merged with the famous default so a
/// provider with no cache still offers its one well-known model.
/// [`refresh_menu`] is the complete listing, fetched live; this is the
/// instant one the window shows while that runs.
pub fn menu<S>(store: &Mutex<CredentialStore>, catalog: &Mutex<ModelCatalog<S>>) -> ModelMenu
where
    S: ModelSource,
{
    let available = store.lock_ignore_poison().available();
    menu_from(&available, &mut catalog.lock_ignore_poison())
}

/// The complete provider picker: every available provider's live model
/// list, fetched from the network wherever the catalog has nothing cached.
///
/// Locks `catalog` only twice, and only briefly — once to open the
/// [`Listing`] (seeding from cache, spawning a background fetch per miss),
/// once to fold the fetches' results back in — never while
/// [`Listing::settle`] blocks on the network in between, so a concurrent
/// instant [`menu`] call is never held up behind this one's fetches.
/// `store` is read once, up front, for the account list alone: a sign-in
/// running alongside this fetch waits on nothing.
pub fn refresh_menu<S>(
    store: &Mutex<CredentialStore>,
    catalog: &Mutex<ModelCatalog<S>>,
) -> ModelMenu
where
    S: ModelSource + Clone + Send + 'static,
{
    let available = store.lock_ignore_poison().available();
    refresh_menu_for(&available, catalog)
}

/// [`refresh_menu`]'s body, over a provider list rather than a store — so
/// the fetch/fold/shape logic is exercised directly, with a fake
/// [`ModelSource`] and no [`CredentialStore`] to stand up.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the guard is deliberately held from the fold-in loop through menu_from's read of the same catalog — one lock for both, not one per use"
)]
fn refresh_menu_for<S>(available: &[Account], catalog: &Mutex<ModelCatalog<S>>) -> ModelMenu
where
    S: ModelSource + Clone + Send + 'static,
{
    let listing = {
        let mut catalog = catalog.lock_ignore_poison();
        let ids = available.iter().map(|account| account.id.clone()).collect();
        Listing::open(ids, &mut catalog)
    };
    let results = listing.settle();

    // Best effort, and off the lock: the [`ModelChoice::reasoning`] flags
    // [`menu_from`] computes below read this catalog, so it should be
    // loaded before that runs wherever loading it is possible at all.
    pricing::ensure_loaded_blocking();

    let mut catalog = catalog.lock_ignore_poison();
    for (id, result) in results {
        if let Ok(models) = result {
            catalog.record(&id, models);
        }
    }
    menu_from(available, &mut catalog)
}

/// Shape `available` into a [`ModelMenu`], reading each provider's model
/// list from `catalog` without ever fetching — the part [`menu`] and
/// [`refresh_menu_for`] share once each has decided what belongs in the
/// catalog.
fn menu_from<S>(available: &[Account], catalog: &mut ModelCatalog<S>) -> ModelMenu
where
    S: ModelSource,
{
    let providers = available
        .iter()
        .map(|account| provider_choice(account, available, catalog))
        .collect();
    ModelMenu {
        providers,
        efforts: provider::EFFORT_LADDER
            .iter()
            .map(|(label, _)| label.to_string())
            .collect(),
        default_effort: provider::default_effort_label().to_string(),
    }
}

/// `default` first, then `listed`, deduped and each tagged by `caps`.
///
/// `caps` is injected rather than reached for globally, so this stays pure
/// and a test can stub it with no live pricing catalog behind it.
fn offers(
    default: Option<&str>,
    listed: Vec<String>,
    caps: impl Fn(&str) -> pricing::ModelCaps,
) -> Vec<ModelChoice> {
    default
        .map(str::to_string)
        .into_iter()
        .chain(listed.into_iter().filter(|m| Some(m.as_str()) != default))
        .map(|name| {
            let reasoning = caps(&name).supports("reasoning");
            ModelChoice { name, reasoning }
        })
        .collect()
}

/// One account's entry: its cached models (if any) merged with its
/// service's default, each carrying whether the pricing catalog knows it
/// reasons.
fn provider_choice<S>(
    account: &Account,
    available: &[Account],
    catalog: &mut ModelCatalog<S>,
) -> ProviderChoice
where
    S: ModelSource,
{
    let default_model = account.service.default_model.clone();
    let cached = catalog.cached(&account.id).unwrap_or_default();
    let models = offers(default_model.as_deref(), cached, pricing::caps_or_default);
    ProviderChoice {
        account: account.id.as_str().to_string(),
        label: identity::label(account, available),
        default_model,
        models,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exarch::provider::identity::AccountId;
    use exarch::provider::models::ProviderEndpoint;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// A built-in service's sole account — the common case in these tests.
    fn fam(name: &str) -> Account {
        let service = identity::built_in_services()
            .into_iter()
            .find(|service| service.name.as_str() == name)
            .unwrap_or_else(|| panic!("no built-in service named {name}"));
        Account::of_service(service)
    }

    /// A signed-in `ChatGPT` account, whose service names no default model
    /// and so stands in for every service [`menu_from`] cannot fall back on.
    fn chatgpt(handle: &str) -> Account {
        let service = identity::chatgpt_service();
        Account {
            id: AccountId::of_login(&service.name, handle),
            service,
            handle: handle.to_string(),
        }
    }

    type Lists = BTreeMap<AccountId, Result<Vec<String>, String>>;

    /// A fake [`ModelSource`] whose list is shared (not forked) across a
    /// clone, so a background-fetch thread run by [`Listing::open`] serves
    /// the same lists the test set up.
    #[derive(Clone)]
    struct FakeSource {
        lists: Arc<Mutex<Lists>>,
    }

    impl FakeSource {
        fn new(lists: Lists) -> Self {
            Self {
                lists: Arc::new(Mutex::new(lists)),
            }
        }
    }

    impl ModelSource for FakeSource {
        fn list(&self, id: &AccountId) -> Result<Vec<String>, String> {
            self.lists
                .lock_ignore_poison()
                .get(id)
                .cloned()
                .unwrap_or_else(|| Err("no fake list".into()))
        }

        fn endpoints(&self, _model: &str) -> Result<Vec<ProviderEndpoint>, String> {
            Err("not exercised by these tests".into())
        }
    }

    fn one(id: AccountId, models: &[&str]) -> Lists {
        let mut m = BTreeMap::new();
        m.insert(id, Ok(models.iter().map(ToString::to_string).collect()));
        m
    }

    fn model_names(choice: &ProviderChoice) -> Vec<String> {
        choice.models.iter().map(|m| m.name.clone()).collect()
    }

    fn offered(models: &[ModelChoice]) -> Vec<&str> {
        models.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn the_default_comes_first_and_is_not_repeated() {
        let out = offers(
            Some("claude-3-5"),
            vec!["claude-haiku-4".to_string(), "claude-3-5".to_string()],
            |_| pricing::ModelCaps::default(),
        );
        assert_eq!(offered(&out), vec!["claude-3-5", "claude-haiku-4"]);
    }

    #[test]
    fn no_default_leaves_the_listed_order_alone() {
        let out = offers(None, vec!["a".to_string(), "b".to_string()], |_| {
            pricing::ModelCaps::default()
        });
        assert_eq!(offered(&out), vec!["a", "b"]);
    }

    #[test]
    fn reasoning_is_tagged_per_model_from_the_injected_caps() {
        let out = offers(
            None,
            vec!["reasoner".to_string(), "chat".to_string()],
            |m| pricing::ModelCaps {
                supported_parameters: if m == "reasoner" {
                    vec!["reasoning".to_string()]
                } else {
                    vec!["temperature".to_string()]
                },
                ..Default::default()
            },
        );
        assert!(out[0].reasoning);
        assert!(!out[1].reasoning);
    }

    #[test]
    fn menu_with_nothing_cached_offers_the_service_default_alone() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let available = [fam("anthropic")];

        let menu = menu_from(&available, &mut catalog);

        assert_eq!(menu.providers.len(), 1);
        assert_eq!(
            model_names(&menu.providers[0]),
            vec![fam("anthropic").service.default_model.unwrap()]
        );
        assert_eq!(menu.efforts.first().map(String::as_str), Some("auto"));
        assert_eq!(menu.default_effort, "med");
    }

    #[test]
    fn menu_with_a_cached_list_puts_the_default_first_and_dedupes_it() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let anthropic = fam("anthropic");
        let default = anthropic.service.default_model.clone().unwrap();
        catalog.record(
            &anthropic.id,
            vec!["claude-haiku-4".to_string(), default.clone()],
        );

        let menu = menu_from(std::slice::from_ref(&anthropic), &mut catalog);

        assert_eq!(
            model_names(&menu.providers[0]),
            vec![default, "claude-haiku-4".to_string()]
        );
    }

    #[test]
    fn a_chatgpt_style_account_with_no_service_default_starts_empty() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let account = chatgpt("work-account");

        let menu = menu_from(std::slice::from_ref(&account), &mut catalog);

        assert!(menu.providers[0].default_model.is_none());
        assert!(model_names(&menu.providers[0]).is_empty());
    }

    #[test]
    fn refresh_menu_folds_fetched_lists_in_and_serves_them() {
        let account = chatgpt("work-account");
        let source = FakeSource::new(one(account.id.clone(), &["gpt-5.5-codex"]));
        let catalog = Mutex::new(ModelCatalog::memo_only(source));

        let menu = refresh_menu_for(std::slice::from_ref(&account), &catalog);

        assert_eq!(
            model_names(&menu.providers[0]),
            vec!["gpt-5.5-codex".to_string()]
        );
        assert_eq!(
            catalog.lock_ignore_poison().cached(&account.id),
            Some(vec!["gpt-5.5-codex".to_string()])
        );
    }

    #[test]
    fn refresh_menu_leaves_a_failed_fetch_uncached_but_still_shows_the_default() {
        let deepseek = fam("deepseek");
        let mut lists = Lists::new();
        lists.insert(deepseek.id.clone(), Err("network down".to_string()));
        let catalog = Mutex::new(ModelCatalog::memo_only(FakeSource::new(lists)));

        let menu = refresh_menu_for(std::slice::from_ref(&deepseek), &catalog);

        assert_eq!(
            model_names(&menu.providers[0]),
            vec![deepseek.service.default_model.clone().unwrap()]
        );
        assert_eq!(catalog.lock_ignore_poison().cached(&deepseek.id), None);
    }
}
