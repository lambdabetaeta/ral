//! Live model lists, cached with a TTL, behind a network seam.
//!
//! A provider's model list is the provider's to know, not exarch's: it is
//! fetched via genai's `Client::all_model_names` rather than restated in a
//! hardcoded table that would go stale. Fetching is **lazy** (the picker
//! pulls a provider's list the first time it is shown), **cached** to the
//! XDG cache dir with a TTL, and **manual entry** is always the fallback —
//! a list that fails to fetch, or that omits the wanted model, never
//! blocks a selection.
//!
//! All network I/O sits behind the [`ModelSource`] trait so `cargo test`
//! drives the cache, the TTL, and the resolution logic against an
//! in-memory fake and never touches the network.

use crate::credential::{Credential, CredentialStore};
use crate::provider::ProviderKind;
use genai::Client;
use genai::resolver::{AuthData, Endpoint, ProviderConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// How long a cached model list stays fresh. A provider's catalog changes
/// on the order of weeks; a day keeps the picker snappy on repeat opens
/// while still picking up new models within a session or two.
const TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The seam every fetch of a provider's model list goes through. The live
/// implementation talks to genai; tests substitute an in-memory fake so no
/// suite ever reaches the network.
pub trait ModelSource {
    /// Fetch the full model-name list for `kind`, or an error message
    /// describing why the fetch failed (the caller degrades to manual
    /// entry).
    fn list(&self, kind: ProviderKind) -> Result<Vec<String>, String>;
}

/// The live source: builds a genai client per provider from the in-memory
/// credentials and calls `all_model_names`. One short-lived tokio runtime
/// backs each fetch — model listing happens at most a handful of times per
/// session, so a dedicated runtime per call is cheaper than holding one
/// open for the picker's whole lifetime.
///
/// `Clone` is cheap (a small key map plus a clone of the shared genai
/// client) and lets the picker hand a copy to a background fetch thread
/// without sharing the catalog's caches.
#[derive(Clone)]
pub struct LiveSource {
    /// Each available API-key provider's key, cloned out of the store so the
    /// source can build a listing client without holding the store. A ChatGPT
    /// login carries no listable key — its Codex backend exposes no catalog —
    /// so that provider is absent here and the picker falls back to manual
    /// entry.
    keys: BTreeMap<ProviderKind, String>,
    /// One genai client, reused across every provider's listing call — the
    /// per-provider endpoint and key ride the per-call `ProviderConfig`.
    client: Client,
}

impl LiveSource {
    pub fn new(store: &CredentialStore) -> Self {
        let keys = store
            .available()
            .into_iter()
            .filter_map(|k| match store.get(k) {
                Some(Credential::ApiKey(key)) => Some((k, key.clone())),
                _ => None,
            })
            .collect();
        Self {
            keys,
            client: Client::builder().with_reqwest(crate::tls::client()).build(),
        }
    }
}

impl ModelSource for LiveSource {
    fn list(&self, kind: ProviderKind) -> Result<Vec<String>, String> {
        let key = self
            .keys
            .get(&kind)
            .ok_or_else(|| format!("{} has no resolved credential", kind.info().0))?;
        // Pass the provider's endpoint (when it has a custom one) and the
        // in-memory key explicitly, so listing does not depend on the
        // client's auth resolver being consulted for a catalog request. The
        // endpoint comes from `ProviderKind` — the single source — so this
        // does not restate provider knowledge.
        let provider_config = ProviderConfig {
            endpoint: kind.endpoint().map(Endpoint::from_static),
            auth: Some(AuthData::from_single(key.clone())),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("build listing runtime: {e}"))?;
        runtime
            .block_on(
                self.client
                    .all_model_names(kind.default_adapter(), provider_config),
            )
            .map_err(|e| format!("list models for {}: {e}", kind.info().0))
    }
}

/// One provider's cached list and when it was fetched.
#[derive(Serialize, Deserialize, Clone)]
struct CacheEntry {
    /// Unix seconds at fetch time. Compared against [`TTL`] for staleness.
    fetched_at: u64,
    models: Vec<String>,
}

/// The on-disk cache: a provider→entry map serialised as JSON under the
/// XDG cache dir.
#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    /// Keyed by the provider's stable label ([`ProviderKind::info`]`.0`),
    /// so the file stays readable and survives reordering the enum.
    providers: BTreeMap<String, CacheEntry>,
}

/// The model catalog: a live (or fake) [`ModelSource`] in front of the
/// XDG-cached, TTL'd lists. Holds a per-process in-memory copy too, so a
/// provider's list is fetched at most once per session even before the
/// disk cache is consulted again.
pub struct ModelCatalog<S: ModelSource> {
    source: S,
    /// `None` disables the disk cache (tests); `Some` is the JSON cache
    /// file path under `$XDG_CACHE_HOME/exarch/models.json`.
    cache_path: Option<PathBuf>,
    memo: BTreeMap<ProviderKind, Vec<String>>,
}

impl<S: ModelSource> ModelCatalog<S> {
    /// A catalog backed by `source`, persisting to the standard XDG cache
    /// path when one resolves.
    pub fn new(source: S) -> Self {
        Self {
            source,
            cache_path: cache_path(),
            memo: BTreeMap::new(),
        }
    }

    /// A catalog with no disk cache — the in-memory memo only. Used by
    /// tests so a suite never reads or writes the user's cache dir.
    #[cfg(test)]
    fn in_memory(source: S) -> Self {
        Self {
            source,
            cache_path: None,
            memo: BTreeMap::new(),
        }
    }

    /// `kind`'s model list. Served from the in-memory memo, then a fresh
    /// disk-cache entry, then a live fetch (which refreshes both caches).
    /// `None` when the fetch fails — callers degrade to manual entry.
    pub fn list(&mut self, kind: ProviderKind) -> Option<Vec<String>> {
        if let Some(models) = self.memo.get(&kind) {
            return Some(models.clone());
        }
        if let Some(models) = self.fresh_from_disk(kind) {
            self.memo.insert(kind, models.clone());
            return Some(models);
        }
        let models = self.source.list(kind).ok()?;
        self.write_disk(kind, &models);
        self.memo.insert(kind, models.clone());
        Some(models)
    }

    /// `kind`'s list if it is already cached (in-memory memo or a fresh
    /// disk entry), without ever fetching. The picker calls this on open to
    /// fill instantly from cache and spawns a background fetch only for the
    /// providers this returns `None` for.
    pub fn cached(&mut self, kind: ProviderKind) -> Option<Vec<String>> {
        if let Some(models) = self.memo.get(&kind) {
            return Some(models.clone());
        }
        let models = self.fresh_from_disk(kind)?;
        self.memo.insert(kind, models.clone());
        Some(models)
    }

    /// Fold a freshly-fetched list into both caches. The picker fetches on
    /// background threads (so the UI shows "loading…" rather than freezing)
    /// and hands the results back here, on the main thread, so the disk
    /// write stays single-threaded.
    pub fn record(&mut self, kind: ProviderKind, models: Vec<String>) {
        self.write_disk(kind, &models);
        self.memo.insert(kind, models);
    }

    /// The source, cloned, for a background fetch thread. The thread fetches
    /// through the [`ModelSource`] seam and reports back; the catalog's
    /// caches are touched only on the main thread via [`Self::record`].
    pub fn source(&self) -> &S {
        &self.source
    }

    /// A non-stale disk-cache entry for `kind`, or `None` when the cache is
    /// absent, unreadable, missing this provider, or stale.
    fn fresh_from_disk(&self, kind: ProviderKind) -> Option<Vec<String>> {
        let path = self.cache_path.as_ref()?;
        let file = read_cache(path)?;
        let entry = file.providers.get(kind.info().0)?;
        let age = crate::bootstrap::now_secs().saturating_sub(entry.fetched_at);
        (age < TTL.as_secs()).then(|| entry.models.clone())
    }

    /// Merge `models` into the disk cache under `kind`, stamping the fetch
    /// time. Best-effort: a cache the process cannot write is not fatal —
    /// the in-memory memo still serves the session.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:models-cache-write] persists the model catalog cache; registry infra, not turn-time data I/O"
    )]
    fn write_disk(&self, kind: ProviderKind, models: &[String]) {
        let Some(path) = self.cache_path.as_ref() else {
            return;
        };
        let mut file = read_cache(path).unwrap_or_default();
        file.providers.insert(
            kind.info().0.to_string(),
            CacheEntry {
                fetched_at: crate::bootstrap::now_secs(),
                models: models.to_vec(),
            },
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&file) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// `$XDG_CACHE_HOME/exarch/models.json`, or `None` when no cache base
/// resolves (`$HOME` unset and no absolute override) — the catalog then
/// runs memo-only.
fn cache_path() -> Option<PathBuf> {
    let dir = crate::bootstrap::xdg_app_dir(ral_core::path::basedir::XdgKind::Cache);
    dir.is_absolute().then(|| dir.join("models.json"))
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:models-cache-read] reads the model catalog cache; registry infra, not turn-time data I/O"
)]
fn read_cache(path: &PathBuf) -> Option<CacheFile> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Resolve a user-supplied `--model` name to the provider that should
/// serve it. Prefers the available provider whose live list contains the
/// name; failing that, falls back to a name-shape match (an `openrouter`
/// slug like `vendor/model` resolves to OpenRouter when available). Errors
/// clearly when no available provider can plausibly serve the name.
///
/// `catalog.list` may fetch, so this is a network-touching call when the
/// name is not yet cached — it runs only on an explicit `--model` override,
/// never on the common path.
pub fn resolve_model_provider<S: ModelSource>(
    name: &str,
    available: &[ProviderKind],
    catalog: &mut ModelCatalog<S>,
) -> Result<ProviderKind, String> {
    if available.is_empty() {
        return Err(
            "no provider available — set a provider API key (e.g. ANTHROPIC_API_KEY)".into(),
        );
    }
    for &kind in available {
        if let Some(models) = catalog.list(kind)
            && models.iter().any(|m| m == name)
        {
            return Ok(kind);
        }
    }
    // No listed match — fall back to the name's shape. A `vendor/model`
    // slug is an OpenRouter convention; bare names go to the single
    // available provider when there is exactly one, so a scripted run with
    // one key set need not name the provider.
    if name.contains('/')
        && let Some(&kind) = available.iter().find(|&&k| k == ProviderKind::Openrouter)
    {
        return Ok(kind);
    }
    if let [only] = available {
        return Ok(*only);
    }
    Err(format!(
        "model '{name}' is not listed by any available provider ({}); \
         pass a model that one of them serves",
        available
            .iter()
            .map(|k| k.info().0)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A fake source returning canned lists and counting fetches, so tests
    /// can assert the memo and TTL prevent redundant network calls.
    struct FakeSource {
        lists: BTreeMap<ProviderKind, Result<Vec<String>, String>>,
        calls: Cell<usize>,
    }

    impl FakeSource {
        fn new(lists: BTreeMap<ProviderKind, Result<Vec<String>, String>>) -> Self {
            Self {
                lists,
                calls: Cell::new(0),
            }
        }
    }

    impl ModelSource for FakeSource {
        fn list(&self, kind: ProviderKind) -> Result<Vec<String>, String> {
            self.calls.set(self.calls.get() + 1);
            self.lists
                .get(&kind)
                .cloned()
                .unwrap_or_else(|| Err("no fake list".into()))
        }
    }

    fn one(
        kind: ProviderKind,
        models: &[&str],
    ) -> BTreeMap<ProviderKind, Result<Vec<String>, String>> {
        let mut m = BTreeMap::new();
        m.insert(kind, Ok(models.iter().map(|s| s.to_string()).collect()));
        m
    }

    /// A listed provider returns its models, and a second `list` is served
    /// from the in-memory memo without a second fetch.
    #[test]
    fn lists_then_memoises() {
        let source = FakeSource::new(one(
            ProviderKind::Anthropic,
            &["claude-opus-4", "claude-haiku-4"],
        ));
        let mut cat = ModelCatalog::in_memory(source);
        match cat.list(ProviderKind::Anthropic) {
            Some(m) => assert_eq!(m, vec!["claude-opus-4", "claude-haiku-4"]),
            None => panic!("expected a list"),
        }
        let _ = cat.list(ProviderKind::Anthropic);
        assert_eq!(
            cat.source.calls.get(),
            1,
            "memo must prevent a second fetch"
        );
    }

    /// A failed fetch degrades to `None` (the picker then offers manual
    /// entry); the reason is surfaced by the source seam, which the picker
    /// reads directly for its note.
    #[test]
    fn failed_fetch_is_none_with_reason_at_the_source() {
        let mut lists = BTreeMap::new();
        lists.insert(ProviderKind::Deepseek, Err("network down".to_string()));
        let mut cat = ModelCatalog::in_memory(FakeSource::new(lists));
        assert!(cat.list(ProviderKind::Deepseek).is_none());
        assert!(
            cat.source()
                .list(ProviderKind::Deepseek)
                .unwrap_err()
                .contains("network down")
        );
    }

    /// `--model` resolves to the available provider whose list contains it.
    #[test]
    fn resolve_prefers_listing_match() {
        let mut lists = BTreeMap::new();
        lists.insert(ProviderKind::Anthropic, Ok(vec!["claude-opus-4".into()]));
        lists.insert(ProviderKind::Deepseek, Ok(vec!["deepseek-chat".into()]));
        let mut cat = ModelCatalog::in_memory(FakeSource::new(lists));
        let available = [ProviderKind::Anthropic, ProviderKind::Deepseek];
        assert_eq!(
            resolve_model_provider("deepseek-chat", &available, &mut cat).unwrap(),
            ProviderKind::Deepseek
        );
    }

    /// A `vendor/model` slug falls back to OpenRouter when no list matches.
    #[test]
    fn resolve_slug_falls_back_to_openrouter() {
        let mut cat = ModelCatalog::in_memory(FakeSource::new(BTreeMap::new()));
        let available = [ProviderKind::Anthropic, ProviderKind::Openrouter];
        assert_eq!(
            resolve_model_provider("x-ai/grok-9", &available, &mut cat).unwrap(),
            ProviderKind::Openrouter
        );
    }

    /// A single available provider serves a bare name even when its list
    /// does not contain it — a scripted run need not name the provider.
    #[test]
    fn resolve_bare_name_to_sole_provider() {
        let mut cat = ModelCatalog::in_memory(FakeSource::new(BTreeMap::new()));
        let available = [ProviderKind::Anthropic];
        assert_eq!(
            resolve_model_provider("claude-future", &available, &mut cat).unwrap(),
            ProviderKind::Anthropic
        );
    }

    /// An unknown name with several providers and no slug shape errors
    /// clearly rather than guessing.
    #[test]
    fn resolve_unknown_with_many_providers_errors() {
        let mut cat = ModelCatalog::in_memory(FakeSource::new(BTreeMap::new()));
        let available = [ProviderKind::Anthropic, ProviderKind::Deepseek];
        let err = resolve_model_provider("mystery", &available, &mut cat).unwrap_err();
        assert!(err.contains("not listed"), "got: {err}");
    }

    /// No available provider is a clear error, not a panic.
    #[test]
    fn resolve_with_no_providers_errors() {
        let mut cat = ModelCatalog::in_memory(FakeSource::new(BTreeMap::new()));
        let err = resolve_model_provider("anything", &[], &mut cat).unwrap_err();
        assert!(err.contains("no provider available"), "got: {err}");
    }
}
