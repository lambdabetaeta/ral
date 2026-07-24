//! Live model-list orchestration shared by every front-end that opens a
//! model picker over a [`ModelCatalog`] — exarch's `/model` TUI overlay and
//! synod's window alike.
//!
//! Fetching a provider's model list means blocking on the network for as
//! long as the provider takes to answer, which a UI thread cannot afford;
//! every front-end wants the same shape instead — seed from cache, spawn a
//! background fetch for the rest, poll without blocking, fold results back
//! into the catalog. This module states that shape once: the fetch-state
//! vocabulary ([`FetchState`] and its aliases), a keyed background-fetch
//! pump ([`Fetches`]), and the per-provider listing built on top of it
//! ([`Listing`]).

use super::ProviderId;
use super::models::{ModelCatalog, ModelSource, ProviderEndpoint};
use std::collections::BTreeMap;

/// One background fetch's state, generic over the loaded payload — a
/// provider's model list ([`ModelsState`]) or a model's serving-provider list
/// ([`EndpointsState`]).
#[derive(Clone)]
pub enum FetchState<T> {
    /// The background fetch is in flight — the row reads "loading…".
    Loading,
    /// A usable value (from cache or a completed fetch).
    Loaded(T),
    /// The fetch failed with this reason.
    Failed(String),
}

/// One provider's model-list fetch state. `Failed`: the provider still
/// accepts a manual model entry.
pub type ModelsState = FetchState<Vec<String>>;

/// One model's serving-provider (`OpenRouter` `/endpoints`) fetch state.
///
/// Keyed by model id, fetched intent-driven when the provider control is
/// focused on it. `Failed`: the route stays `auto` (`OpenRouter` decides).
pub type EndpointsState = FetchState<Vec<ProviderEndpoint>>;

/// A keyed background-fetch pump.
///
/// [`Self::spawn`] runs one fetch per key on its own thread and reports back
/// through a channel shared by every key, so a caller polls [`Self::landed`]
/// without ever blocking and folds results in the order they arrived,
/// whichever key that turns out to be.
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

    /// Run `fetch` on its own thread and send `(key, fetch())` back once it
    /// returns. A send error (the pump has already been dropped) is ignored
    /// — nothing is left to hand the result to.
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

    /// Block until every outstanding fetch has reported back, then return
    /// every result collected, whatever order they arrived in.
    ///
    /// This terminates even when a worker thread panics before sending: a
    /// panicking thread unwinds (or aborts) without running the send, so its
    /// clone of `tx` simply drops. Dropping this pump's own sender first is
    /// what makes the invariant hold — once every clone (this one, and each
    /// spawned thread's) has dropped, `recv` reports the channel closed and
    /// the loop below ends. There is deliberately no outstanding-fetch count
    /// and no timeout: the transport's TLS client already arms a per-read
    /// idle timeout, so a wedged fetch eventually errors and sends, rather
    /// than hanging forever with its sender still alive.
    pub fn settle(self) -> Vec<(K, Result<T, String>)> {
        drop(self.tx);
        std::iter::from_fn(|| self.rx.recv().ok()).collect()
    }
}

/// Every available provider's model list over a [`ModelCatalog`]: the
/// per-provider fetch state, seeded from the catalog's caches and completed
/// by a keyed [`Fetches`] pump.
pub struct Listing {
    states: BTreeMap<ProviderId, ModelsState>,
    fetches: Fetches<ProviderId, Vec<String>>,
}

impl Listing {
    /// Open a listing over every entry in `providers`. A provider already
    /// cached in `catalog` (its in-memory memo or a fresh disk entry) seeds
    /// `Loaded` with no network touched; a miss seeds `Loading` and spawns
    /// its background fetch through the catalog's [`ModelSource`], so the
    /// caller can show the list instantly and let the misses fill in as
    /// [`Self::pump`] drains them.
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

    /// Drain every fetch that has landed since the last call, folding each
    /// success into `catalog` (so the disk cache stays authoritative for the
    /// next open) and into this listing's own state, each failure into the
    /// state alone. Returns the providers whose state just changed, in
    /// landed order — the caller's cue to redraw those rows.
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

    /// Block until every outstanding fetch spawned by [`Self::open`] has
    /// reported back, returning each provider's raw result.
    ///
    /// Catalog-free by design: a caller holding the catalog behind a lock
    /// across this call (synod's session loop, say) folds the successes in
    /// afterward via [`ModelCatalog::record`], so this never needs to reach
    /// the catalog while some other thread might be holding it.
    pub fn settle(self) -> Vec<(ProviderId, Result<Vec<String>, String>)> {
        self.fetches.settle()
    }

    /// `id`'s current fetch state, if this listing opened it.
    pub fn state(&self, id: &ProviderId) -> Option<&ModelsState> {
        self.states.get(id)
    }

    /// Every provider this listing opened, paired with its current state.
    pub fn states(&self) -> impl Iterator<Item = (&ProviderId, &ModelsState)> {
        self.states.iter()
    }

    /// Whether any provider's fetch is still in flight.
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

    /// A famous provider's id — the common case in these tests.
    fn fam(kind: ProviderKind) -> ProviderId {
        ProviderId::Famous(kind)
    }

    type Lists = BTreeMap<ProviderId, Result<Vec<String>, String>>;

    /// A fake [`ModelSource`] whose fields are shared (not forked) across a
    /// clone, so a background-fetch thread's counter increment is visible on
    /// the calling thread — the whole point of these tests.
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

    /// Poll `listing` until nothing is loading, or give up after a generous
    /// bound — the fetches run on real threads, so a fixed number of
    /// zero-work polls would be flaky.
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
