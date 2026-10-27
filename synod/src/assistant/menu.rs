use crate::wire::{ModelChoice, ModelMenu, Models, ProviderChoice};
use exarch::provider::{
    self,
    credential::CredentialStore,
    identity::{self, Account, AccountId},
    listing::Listing,
    models::{ModelCatalog, ModelSource},
    pricing,
};
use ral_core::sync::LockExt;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Uses cached models only; a miss stays Loading until refresh.
pub(crate) fn menu<S>(store: &Mutex<CredentialStore>, catalog: &Mutex<ModelCatalog<S>>) -> ModelMenu
where
    S: ModelSource,
{
    let available = store.lock_ignore_poison().available();
    menu_from(
        &available,
        &mut catalog.lock_ignore_poison(),
        &BTreeMap::new(),
    )
}

/// Never hold store or catalog locks during network waits.
pub(crate) fn refresh_menu<S>(
    store: &Mutex<CredentialStore>,
    catalog: &Mutex<ModelCatalog<S>>,
) -> ModelMenu
where
    S: ModelSource + Clone + Send + 'static,
{
    let available = store.lock_ignore_poison().available();
    refresh_menu_for(&available, catalog)
}

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

    // Load pricing off the lock, before computing reasoning flags.
    pricing::ensure_loaded_blocking();

    let mut catalog = catalog.lock_ignore_poison();
    let mut failed = BTreeMap::new();
    for (id, result) in results {
        match result {
            Ok(models) => catalog.record(&id, models),
            Err(reason) => {
                failed.insert(id, reason.to_string());
            }
        }
    }
    menu_from(available, &mut catalog, &failed)
}

fn menu_from<S>(
    available: &[Account],
    catalog: &mut ModelCatalog<S>,
    failed: &BTreeMap<AccountId, String>,
) -> ModelMenu
where
    S: ModelSource,
{
    let providers = available
        .iter()
        .map(|account| provider_choice(account, available, catalog, failed))
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

fn offers(listed: Vec<String>, caps: impl Fn(&str) -> pricing::ModelCaps) -> Vec<ModelChoice> {
    listed
        .into_iter()
        .map(|name| {
            let reasoning = caps(&name).supports(pricing::Param::Reasoning);
            ModelChoice { name, reasoning }
        })
        .collect()
}

fn provider_choice<S>(
    account: &Account,
    available: &[Account],
    catalog: &mut ModelCatalog<S>,
    failed: &BTreeMap<AccountId, String>,
) -> ProviderChoice
where
    S: ModelSource,
{
    let models = match catalog.cached(&account.id) {
        Some(listed) => Models::Listed {
            choices: offers(listed, pricing::caps_or_default),
        },
        None => failed
            .get(&account.id)
            .map_or(Models::Loading, |reason| Models::Failed {
                reason: reason.clone(),
            }),
    };
    ProviderChoice {
        account: account.id.as_str().to_string(),
        label: identity::label(account, available),
        models,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exarch::provider::models::{ListFailure, Listed, ProviderEndpoint};
    use std::sync::Arc;

    fn fam(name: &str) -> Account {
        let service = identity::built_in_services()
            .into_iter()
            .find(|service| service.name.as_str() == name)
            .unwrap_or_else(|| panic!("no built-in service named {name}"));
        Account::of_service(service)
    }

    type Lists = BTreeMap<AccountId, Result<Vec<Listed>, ListFailure>>;

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
        fn list(&self, id: &AccountId) -> Result<Vec<Listed>, ListFailure> {
            self.lists
                .lock_ignore_poison()
                .get(id)
                .cloned()
                .unwrap_or_else(|| Err(ListFailure::Request("no fake list".into())))
        }

        fn endpoints(&self, _model: &str) -> Result<Vec<ProviderEndpoint>, String> {
            Err("not exercised by these tests".into())
        }
    }

    fn one(id: AccountId, models: &[&str]) -> Lists {
        let mut m = BTreeMap::new();
        m.insert(id, Ok(models.iter().map(|m| Listed::bare(*m)).collect()));
        m
    }

    fn model_names(choice: &ProviderChoice) -> Option<Vec<String>> {
        match &choice.models {
            Models::Listed { choices } => Some(choices.iter().map(|m| m.name.clone()).collect()),
            Models::Loading | Models::Failed { .. } => None,
        }
    }

    fn offered(models: &[ModelChoice]) -> Vec<&str> {
        models.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn offers_keep_the_listed_order() {
        let out = offers(vec!["b".to_string(), "a".to_string()], |_| {
            pricing::ModelCaps::default()
        });
        assert_eq!(offered(&out), vec!["b", "a"]);
    }

    #[test]
    fn reasoning_is_tagged_per_model_from_the_injected_caps() {
        let out = offers(vec!["reasoner".to_string(), "chat".to_string()], |m| {
            pricing::ModelCaps {
                supported_parameters: if m == "reasoner" {
                    vec!["reasoning".to_string()]
                } else {
                    vec!["temperature".to_string()]
                },
                ..Default::default()
            }
        });
        assert!(out[0].reasoning);
        assert!(!out[1].reasoning);
    }

    #[test]
    fn menu_with_nothing_cached_is_loading() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let available = [fam("anthropic")];

        let menu = menu_from(&available, &mut catalog, &BTreeMap::new());

        assert_eq!(menu.providers.len(), 1);
        assert!(matches!(menu.providers[0].models, Models::Loading));
        assert_eq!(menu.efforts.first().map(String::as_str), Some("auto"));
        assert_eq!(menu.default_effort, "med");
    }

    #[test]
    fn menu_with_a_cached_list_offers_it_in_order() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let anthropic = fam("anthropic");
        catalog.record(
            &anthropic.id,
            vec![Listed::bare("model-b"), Listed::bare("model-a")],
        );

        let menu = menu_from(
            std::slice::from_ref(&anthropic),
            &mut catalog,
            &BTreeMap::new(),
        );

        assert_eq!(
            model_names(&menu.providers[0]),
            Some(vec!["model-b".to_string(), "model-a".to_string()])
        );
    }

    #[test]
    fn refresh_menu_folds_fetched_lists_in_and_serves_them() {
        let account = Account::login(
            exarch::provider::identity::Login::Chatgpt,
            "work-account",
            "work-account",
        );
        let source = FakeSource::new(one(account.id.clone(), &["model-a"]));
        let catalog = Mutex::new(ModelCatalog::memo_only(source));

        let menu = refresh_menu_for(std::slice::from_ref(&account), &catalog);

        assert_eq!(
            model_names(&menu.providers[0]),
            Some(vec!["model-a".to_string()])
        );
        assert_eq!(
            catalog.lock_ignore_poison().cached(&account.id),
            Some(vec!["model-a".to_string()])
        );
    }

    #[test]
    fn refresh_menu_leaves_a_failed_fetch_uncached_and_says_why() {
        let deepseek = fam("deepseek");
        let mut lists = Lists::new();
        lists.insert(
            deepseek.id.clone(),
            Err(ListFailure::Request("network down".into())),
        );
        let catalog = Mutex::new(ModelCatalog::memo_only(FakeSource::new(lists)));

        let menu = refresh_menu_for(std::slice::from_ref(&deepseek), &catalog);

        assert!(matches!(
            &menu.providers[0].models,
            Models::Failed { reason } if reason == "network down"
        ));
        assert_eq!(catalog.lock_ignore_poison().cached(&deepseek.id), None);
    }
}
