//! Non-blocking model-list fetching for every front-end that opens a picker
//! over a [`ModelCatalog`] — exarch's `/model` overlay and synod's window.
//!
//! No UI thread can spend a network round trip on a provider's list, so both
//! seed from cache and fetch the misses in the background — [`FetchState`]
//! names the outcomes, [`Fetches`] pumps the threads, [`Listing`] joins them.

use super::ProviderId;
use super::models::{ModelCatalog, ModelSource, ProviderEndpoint};
use std::collections::BTreeMap;

/// One background fetch's state, over a provider's model list
/// ([`ModelsState`]) or a model's serving providers ([`EndpointsState`]).
#[derive(Clone)]
pub enum FetchState<T> {
    Loading,
    Loaded(T),
    Failed(String),
}

/// One provider's model-list fetch state; `Failed` still leaves manual entry.
pub type ModelsState = FetchState<Vec<String>>;

/// One model's serving-provider (`OpenRouter` `/endpoints`) fetch state, keyed
/// by model id; `Failed` leaves the route on `auto`, `OpenRouter`'s own choice.
pub type EndpointsState = FetchState<Vec<ProviderEndpoint>>;

/// A keyed background-fetch pump: one thread per key, all reporting through one
/// channel, so [`Self::landed`] never blocks and results fold in arrival order.
pub struct Fetches<K, T> {
    tx: std::sync::mpsc::Sender<(K, Result<T, String>)>,
    rx: std::sync::mpsc::Receiver<(K, Result<T, String>)>,
}

impl<K, T> Default for Fetches<K, T> {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self { tx, rx }
    }
}

impl<K, T> Fetches<K, T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `fetch` on its own thread and send `(key, fetch())` back. The send
    /// fails only when the pump is already dropped — nothing left to hand it to.
    pub fn spawn(&self, key: K, fetch: impl FnOnce() -> Result<T, String> + Send + 'static)
    where
        K: Send + 'static,
        T: Send + 'static,
    {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = fetch();
            let _ = tx.send((key, result));
        });
    }

    /// Every fetch that has landed since the last call, without blocking.
    pub fn landed(&mut self) -> Vec<(K, Result<T, String>)> {
        std::iter::from_fn(|| self.rx.try_recv().ok()).collect()
    }

    /// Block until every outstanding fetch has reported back, in arrival order.
    /// Dropping this pump's own sender first is what ends the loop: `recv`
    /// reports the channel closed once every clone has gone, including a
    /// panicking thread's, which unwound past its send. Hence no fetch count
    /// and no timeout — the transport's per-read idle timeout makes a wedged
    /// fetch error and send of its own accord.
    pub fn settle(self) -> Vec<(K, Result<T, String>)> {
        drop(self.tx);
        std::iter::from_fn(|| self.rx.recv().ok()).collect()
    }
}

/// Every available provider's model list over a [`ModelCatalog`]: fetch state
/// seeded from the catalog's caches, completed by a [`Fetches`] pump.
pub struct Listing {
    states: BTreeMap<ProviderId, ModelsState>,
    fetches: Fetches<ProviderId, Vec<String>>,
}

impl Listing {
    /// Open a listing over `providers`: a hit on `catalog`'s memo or fresh disk
    /// entry seeds `Loaded` untouched by the network, a miss seeds `Loading` and
    /// spawns a fetch through its [`ModelSource`] for [`Self::pump`] to drain.
    pub fn open<S>(providers: Vec<ProviderId>, catalog: &mut ModelCatalog<S>) -> Self
    where
        S: ModelSource + Clone + Send + 'static,
    {
        let mut states = BTreeMap::new();
        let fetches = Fetches::new();
        for id in providers {
            if let Some(models) = catalog.cached(&id) {
                states.insert(id, ModelsState::Loaded(models));
            } else {
                states.insert(id.clone(), ModelsState::Loading);
                let source = catalog.source().clone();
                fetches.spawn(id.clone(), move || source.list(&id));
            }
        }
        Self { states, fetches }
    }

    /// Fold every landed fetch into this listing, each success into `catalog`
    /// too, and return the providers whose state changed. Folding on the
    /// caller's thread is what keeps the catalog's disk write serial.
    pub fn pump<S: ModelSource>(&mut self, catalog: &mut ModelCatalog<S>) -> Vec<ProviderId> {
        let mut changed = Vec::new();
        for (id, result) in self.fetches.landed() {
            let state = match result {
                Ok(models) => {
                    catalog.record(&id, models.clone());
                    ModelsState::Loaded(models)
                }
                Err(reason) => ModelsState::Failed(reason),
            };
            self.states.insert(id.clone(), state);
            changed.push(id);
        }
        changed
    }

    /// Block until every fetch [`Self::open`] spawned has reported back.
    /// Catalog-free by design: a caller holding the catalog behind a lock
    /// (synod's `refresh_menu`) folds the successes in afterward through
    /// [`ModelCatalog::record`], never holding it across the network.
    pub fn settle(self) -> Vec<(ProviderId, Result<Vec<String>, String>)> {
        self.fetches.settle()
    }

    /// `id`'s current fetch state, if this listing opened it.
    pub fn state(&self, id: &ProviderId) -> Option<&ModelsState> {
        self.states.get(id)
    }

    pub fn states(&self) -> impl Iterator<Item = (&ProviderId, &ModelsState)> {
        self.states.iter()
    }

    pub fn is_loading(&self) -> bool {
        self.states
            .values()
            .any(|s| matches!(s, ModelsState::Loading))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;
    use crate::sync::LockExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn fam(kind: ProviderKind) -> ProviderId {
        ProviderId::Famous(kind)
    }

    type Lists = BTreeMap<ProviderId, Result<Vec<String>, String>>;

    /// A fake [`ModelSource`] whose state is shared, not forked, across a clone
    /// — so a background thread's fetch is counted where the test can see it.
    #[derive(Clone)]
    struct FakeSource {
        lists: Arc<Mutex<Lists>>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeSource {
        fn new(lists: Lists) -> Self {
            Self {
                lists: Arc::new(Mutex::new(lists)),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ModelSource for FakeSource {
        fn list(&self, id: &ProviderId) -> Result<Vec<String>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
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

    fn one(id: ProviderId, models: &[&str]) -> Lists {
        let mut m = BTreeMap::new();
        m.insert(id, Ok(models.iter().map(ToString::to_string).collect()));
        m
    }

    /// Poll until nothing is loading. The fetches run on real threads, so a
    /// fixed number of zero-work polls would be flaky.
    fn wait_for_settled<S: ModelSource>(listing: &mut Listing, catalog: &mut ModelCatalog<S>) {
        for _ in 0..1000 {
            listing.pump(catalog);
            if !listing.is_loading() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("listing did not settle in time");
    }

    #[test]
    fn everything_cached_loads_with_no_fetch() {
        let source = FakeSource::new(one(fam(ProviderKind::Anthropic), &["claude-opus-4"]));
        let mut catalog = ModelCatalog::memo_only(source.clone());
        catalog.record(&fam(ProviderKind::Anthropic), vec!["claude-opus-4".into()]);

        let listing = Listing::open(vec![fam(ProviderKind::Anthropic)], &mut catalog);

        match listing.state(&fam(ProviderKind::Anthropic)) {
            Some(ModelsState::Loaded(models)) => {
                assert_eq!(models, &vec!["claude-opus-4".to_string()]);
            }
            Some(ModelsState::Loading) => panic!("expected Loaded, got Loading"),
            Some(ModelsState::Failed(reason)) => panic!("expected Loaded, got Failed({reason})"),
            None => panic!("expected Loaded, got no state at all"),
        }
        assert!(!listing.is_loading());
        assert_eq!(source.calls(), 0, "a cached provider must spawn no fetch");
    }

    #[test]
    fn a_miss_loads_in_the_background_and_records_into_the_catalog() {
        let source = FakeSource::new(one(fam(ProviderKind::Deepseek), &["deepseek-chat"]));
        let mut catalog = ModelCatalog::memo_only(source);

        let mut listing = Listing::open(vec![fam(ProviderKind::Deepseek)], &mut catalog);
        assert!(matches!(
            listing.state(&fam(ProviderKind::Deepseek)),
            Some(ModelsState::Loading)
        ));

        wait_for_settled(&mut listing, &mut catalog);

        match listing.state(&fam(ProviderKind::Deepseek)) {
            Some(ModelsState::Loaded(models)) => {
                assert_eq!(models, &vec!["deepseek-chat".to_string()]);
            }
            Some(ModelsState::Loading) => panic!("expected Loaded, got Loading"),
            Some(ModelsState::Failed(reason)) => panic!("expected Loaded, got Failed({reason})"),
            None => panic!("expected Loaded, got no state at all"),
        }
        assert_eq!(
            catalog.cached(&fam(ProviderKind::Deepseek)),
            Some(vec!["deepseek-chat".to_string()])
        );
    }

    #[test]
    fn a_failed_fetch_lands_failed_and_is_not_recorded() {
        let mut lists = BTreeMap::new();
        lists.insert(fam(ProviderKind::Openai), Err("network down".to_string()));
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(lists));

        let mut listing = Listing::open(vec![fam(ProviderKind::Openai)], &mut catalog);
        wait_for_settled(&mut listing, &mut catalog);

        match listing.state(&fam(ProviderKind::Openai)) {
            Some(ModelsState::Failed(reason)) => assert!(reason.contains("network down")),
            Some(ModelsState::Loaded(_)) => panic!("expected Failed, got Loaded"),
            Some(ModelsState::Loading) => panic!("expected Failed, got Loading"),
            None => panic!("expected Failed, got no state at all"),
        }
        assert_eq!(catalog.cached(&fam(ProviderKind::Openai)), None);
    }

    #[test]
    fn settle_returns_every_outstanding_result() {
        let mut lists = one(fam(ProviderKind::Anthropic), &["claude-opus-4"]);
        lists.insert(fam(ProviderKind::Openai), Err("no key".to_string()));
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(lists));

        let listing = Listing::open(
            vec![fam(ProviderKind::Anthropic), fam(ProviderKind::Openai)],
            &mut catalog,
        );
        let mut results = listing.settle();
        results.sort_by_key(|(id, _)| id.label().to_string());

        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0],
            (
                fam(ProviderKind::Anthropic),
                Ok(vec!["claude-opus-4".to_string()])
            )
        );
        assert_eq!(
            results[1],
            (fam(ProviderKind::Openai), Err("no key".to_string()))
        );
    }

    #[test]
    fn settle_drops_a_panicked_fetch_rather_than_hanging() {
        let fetches: Fetches<&'static str, i32> = Fetches::new();
        fetches.spawn("ok", || Ok(1));
        fetches.spawn("boom", || panic!("a fetch that never sends"));

        let results = fetches.settle();

        assert_eq!(results, vec![("ok", Ok(1))]);
    }
}
