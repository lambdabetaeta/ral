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
use crate::provider::{ProviderId, ProviderKind};
use genai::Client;
use genai::resolver::{AuthData, Endpoint, ProviderConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// How long a cached model list stays fresh. A provider's catalog changes
/// on the order of weeks; a day keeps the picker snappy on repeat opens
/// while still picking up new models within a session or two.
const TTL: Duration = Duration::from_hours(24);

/// One upstream provider OpenRouter can route a given model to — a row of the
/// `/model` overlay's provider control. OpenRouter fronts several serving
/// providers per model (DeepInfra, Novita, …) that differ in context window and
/// quantization; this is the picker's view of one such endpoint, distilled from
/// OpenRouter's per-model `/endpoints` listing.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderEndpoint {
    /// The human display name, e.g. `"DeepInfra"`.
    pub provider_name: String,
    /// The routing slug — what `provider.order` wants in the request body. It is
    /// the part of the endpoint `tag` before the `/` (`"deepinfra/fp4"` →
    /// `"deepinfra"`), which equals the `/api/v1/providers` slug.
    pub slug: String,
    /// This endpoint's context window in tokens, when reported — often the
    /// deciding factor between providers serving the same model.
    pub context_length: Option<u64>,
    /// The weight quantization (`"fp8"`, `"fp4"`, …), when reported — it bears
    /// on output quality.
    pub quantization: Option<String>,
}

/// The seam every fetch of a provider's model list goes through. The live
/// implementation talks to genai (model lists) and OpenRouter's REST API
/// (per-model endpoints); tests substitute an in-memory fake so no suite ever
/// reaches the network.
pub trait ModelSource {
    /// Fetch the full model-name list for `id`, or an error message
    /// describing why the fetch failed (the caller degrades to manual
    /// entry).
    fn list(&self, id: &ProviderId) -> Result<Vec<String>, String>;

    /// Fetch the upstream serving providers OpenRouter lists for `model`, or an
    /// error message. Only meaningful for OpenRouter `vendor/model` ids — the
    /// only provider that routes — so the picker calls this solely for an
    /// OpenRouter selection.
    fn endpoints(&self, model: &str) -> Result<Vec<ProviderEndpoint>, String>;
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
    keys: BTreeMap<ProviderId, String>,
    /// One genai client, reused across every provider's listing call — the
    /// per-provider endpoint and key ride the per-call `ProviderConfig`.
    client: Client,
}

impl LiveSource {
    pub fn new(store: &CredentialStore) -> Self {
        let keys = store
            .available()
            .into_iter()
            .filter_map(|id| match store.get(&id) {
                Some(Credential::ApiKey(key)) => Some((id, key.clone())),
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
    fn list(&self, id: &ProviderId) -> Result<Vec<String>, String> {
        let key = self
            .keys
            .get(id)
            .ok_or_else(|| format!("{} has no resolved credential", id.label()))?;
        // Pass the provider's endpoint (when it has a custom one) and the
        // in-memory key explicitly, so listing does not depend on the
        // client's auth resolver being consulted for a catalog request. The
        // endpoint comes from the `ProviderId` — the single source — so this
        // does not restate provider knowledge. A custom provider lists through
        // its declared endpoint and adapter exactly as a famous one does.
        let provider_config = ProviderConfig {
            endpoint: id.endpoint().map(Endpoint::from_owned),
            auth: Some(AuthData::from_single(key.clone())),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("build listing runtime: {e}"))?;
        runtime
            .block_on(
                self.client
                    .all_model_names(id.default_adapter(), provider_config),
            )
            .map_err(|e| format!("list models for {}: {e}", id.label()))
    }

    /// `GET https://openrouter.ai/api/v1/models/{model}/endpoints`. The listing
    /// is public, but the OpenRouter key is sent when available so the call
    /// shares the account's rate budget rather than the anonymous one. A
    /// short-lived current-thread runtime backs the request, as for [`Self::list`].
    fn endpoints(&self, model: &str) -> Result<Vec<ProviderEndpoint>, String> {
        let url = format!("https://openrouter.ai/api/v1/models/{model}/endpoints");
        let key = self
            .keys
            .iter()
            .find_map(|(id, k)| (id.famous() == Some(ProviderKind::Openrouter)).then(|| k.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("build endpoints runtime: {e}"))?;
        runtime.block_on(async {
            let mut request = crate::tls::client().get(&url);
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let response = request
                .send()
                .await
                .map_err(|e| format!("list providers for {model}: {e}"))?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("list providers for {model}: HTTP {status}"));
            }
            let body: EndpointsResponse = response
                .json()
                .await
                .map_err(|e| format!("parse providers for {model}: {e}"))?;
            Ok(body
                .data
                .endpoints
                .into_iter()
                .map(ProviderEndpoint::from_wire)
                .collect())
        })
    }
}

/// OpenRouter's `/endpoints` envelope: the model object carries the serving
/// `endpoints` array. Only the fields the picker shows (and the routing slug)
/// are read; the rest of each entry (pricing, uptime, latency) is ignored.
#[derive(Deserialize)]
struct EndpointsResponse {
    data: EndpointsData,
}

#[derive(Deserialize)]
struct EndpointsData {
    #[serde(default)]
    endpoints: Vec<EndpointWire>,
}

#[derive(Deserialize)]
struct EndpointWire {
    #[serde(default)]
    provider_name: String,
    /// `provider-slug/variant`, e.g. `"deepinfra/fp4"`; the slug is its prefix.
    #[serde(default)]
    tag: String,
    context_length: Option<u64>,
    quantization: Option<String>,
}

impl ProviderEndpoint {
    /// Distil a wire endpoint into the picker's view, deriving the routing slug
    /// from the `tag` prefix (the whole tag when it carries no `/`).
    fn from_wire(wire: EndpointWire) -> Self {
        let slug = wire
            .tag
            .split_once('/')
            .map(|(prefix, _)| prefix.to_string())
            .unwrap_or(wire.tag);
        Self {
            provider_name: wire.provider_name,
            slug,
            context_length: wire.context_length,
            quantization: wire.quantization,
        }
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
    memo: BTreeMap<ProviderId, Vec<String>>,
    /// In-session per-model serving-provider lists, keyed by OpenRouter model
    /// id. Memo-only (no disk cache): provider availability is volatile and the
    /// fetch is cheap, so it is refetched once per session rather than persisted.
    endpoints_memo: BTreeMap<String, Vec<ProviderEndpoint>>,
}

impl<S: ModelSource> ModelCatalog<S> {
    /// A catalog backed by `source`, persisting to the standard XDG cache
    /// path when one resolves.
    pub fn new(source: S) -> Self {
        Self {
            source,
            cache_path: cache_path(),
            memo: BTreeMap::new(),
            endpoints_memo: BTreeMap::new(),
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
            endpoints_memo: BTreeMap::new(),
        }
    }

    /// `model`'s serving providers if already fetched this session, without
    /// touching the network — the picker seeds instantly from this and spawns a
    /// background fetch (through [`Self::source`]) only on a memo miss.
    pub fn cached_endpoints(&self, model: &str) -> Option<Vec<ProviderEndpoint>> {
        self.endpoints_memo.get(model).cloned()
    }

    /// Fold a freshly-fetched serving-provider list into the in-session memo.
    /// The picker fetches on a background thread and hands the result back here,
    /// on the main thread, mirroring [`Self::record`] for model lists.
    pub fn record_endpoints(&mut self, model: &str, endpoints: Vec<ProviderEndpoint>) {
        self.endpoints_memo.insert(model.to_string(), endpoints);
    }

    /// `id`'s model list. Served from the in-memory memo, then a fresh
    /// disk-cache entry, then a live fetch (which refreshes both caches).
    /// `None` when the fetch fails — callers degrade to manual entry.
    pub fn list(&mut self, id: &ProviderId) -> Option<Vec<String>> {
        if let Some(models) = self.memo.get(id) {
            return Some(models.clone());
        }
        if let Some(models) = self.fresh_from_disk(id) {
            self.memo.insert(id.clone(), models.clone());
            return Some(models);
        }
        let models = self.source.list(id).ok()?;
        self.write_disk(id, &models);
        self.memo.insert(id.clone(), models.clone());
        Some(models)
    }

    /// `id`'s list if it is already cached (in-memory memo or a fresh
    /// disk entry), without ever fetching. The picker calls this on open to
    /// fill instantly from cache and spawns a background fetch only for the
    /// providers this returns `None` for.
    pub fn cached(&mut self, id: &ProviderId) -> Option<Vec<String>> {
        if let Some(models) = self.memo.get(id) {
            return Some(models.clone());
        }
        let models = self.fresh_from_disk(id)?;
        self.memo.insert(id.clone(), models.clone());
        Some(models)
    }

    /// Fold a freshly-fetched list into both caches. The picker fetches on
    /// background threads (so the UI shows "loading…" rather than freezing)
    /// and hands the results back here, on the main thread, so the disk
    /// write stays single-threaded.
    pub fn record(&mut self, id: &ProviderId, models: Vec<String>) {
        self.write_disk(id, &models);
        self.memo.insert(id.clone(), models);
    }

    /// The source, cloned, for a background fetch thread. The thread fetches
    /// through the [`ModelSource`] seam and reports back; the catalog's
    /// caches are touched only on the main thread via [`Self::record`].
    pub fn source(&self) -> &S {
        &self.source
    }

    /// A non-stale disk-cache entry for `id`, or `None` when the cache is
    /// absent, unreadable, missing this provider, or stale.
    fn fresh_from_disk(&self, id: &ProviderId) -> Option<Vec<String>> {
        let path = self.cache_path.as_ref()?;
        let file = read_cache(path)?;
        let entry = file.providers.get(id.label())?;
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
    fn write_disk(&self, id: &ProviderId, models: &[String]) {
        let Some(path) = self.cache_path.as_ref() else {
            return;
        };
        let mut file = read_cache(path).unwrap_or_default();
        file.providers.insert(
            id.label().to_string(),
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
    available: &[ProviderId],
    catalog: &mut ModelCatalog<S>,
) -> Result<ProviderId, String> {
    if available.is_empty() {
        return Err(
            "no provider available — set a provider API key (e.g. ANTHROPIC_API_KEY)".into(),
        );
    }
    for id in available {
        if let Some(models) = catalog.list(id)
            && models.iter().any(|m| m == name)
        {
            return Ok(id.clone());
        }
    }
    // No listed match — fall back to the name's shape. A `vendor/model`
    // slug is an OpenRouter convention; bare names go to the single
    // available provider when there is exactly one, so a scripted run with
    // one key set need not name the provider.
    if name.contains('/')
        && let Some(id) = available
            .iter()
            .find(|id| id.famous() == Some(ProviderKind::Openrouter))
    {
        return Ok(id.clone());
    }
    if let [only] = available {
        return Ok(only.clone());
    }
    Err(format!(
        "model '{name}' is not listed by any available provider ({}); \
         pass a model that one of them serves",
        available
            .iter()
            .map(ProviderId::label)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Resolve an explicit `--provider` label to the matching available provider,
/// pinning it verbatim. No model-listing lookup happens here — that is the
/// whole point of pinning — so the caller may then name a model the provider
/// does not advertise. Errors clearly when the label matches no available
/// provider.
pub fn resolve_pinned_provider(
    name: &str,
    available: &[ProviderId],
) -> Result<ProviderId, String> {
    if let Some(id) = available.iter().find(|id| id.label() == name) {
        return Ok(id.clone());
    }
    if available.is_empty() {
        return Err(
            "no provider available — set a provider API key (e.g. ANTHROPIC_API_KEY)".into(),
        );
    }
    Err(format!(
        "provider '{name}' is not available ({}); set its API key or name one that is",
        available
            .iter()
            .map(ProviderId::label)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::adapter::AdapterKind;
    use std::cell::Cell;
    use std::sync::Arc;

    /// A famous provider's id — the common case in these tests.
    fn fam(kind: ProviderKind) -> ProviderId {
        ProviderId::Famous(kind)
    }

    /// A custom provider's id with `label`; the other facts are immaterial to
    /// the catalog/resolver (they key on the label).
    fn custom(label: &str) -> ProviderId {
        ProviderId::Custom(Arc::new(crate::provider::CustomProvider {
            label: label.into(),
            key_env: format!("{}_KEY", label.to_uppercase()),
            endpoint: format!("https://{label}.example/v1/"),
            adapter: AdapterKind::OpenAI,
        }))
    }

    /// A fake source returning canned lists and counting fetches, so tests
    /// can assert the memo and TTL prevent redundant network calls.
    struct FakeSource {
        lists: BTreeMap<ProviderId, Result<Vec<String>, String>>,
        endpoints: BTreeMap<String, Result<Vec<ProviderEndpoint>, String>>,
        calls: Cell<usize>,
    }

    impl FakeSource {
        fn new(lists: BTreeMap<ProviderId, Result<Vec<String>, String>>) -> Self {
            Self {
                lists,
                endpoints: BTreeMap::new(),
                calls: Cell::new(0),
            }
        }

        fn with_endpoints(
            mut self,
            endpoints: BTreeMap<String, Result<Vec<ProviderEndpoint>, String>>,
        ) -> Self {
            self.endpoints = endpoints;
            self
        }
    }

    impl ModelSource for FakeSource {
        fn list(&self, id: &ProviderId) -> Result<Vec<String>, String> {
            self.calls.set(self.calls.get() + 1);
            self.lists
                .get(id)
                .cloned()
                .unwrap_or_else(|| Err("no fake list".into()))
        }

        fn endpoints(&self, model: &str) -> Result<Vec<ProviderEndpoint>, String> {
            self.endpoints
                .get(model)
                .cloned()
                .unwrap_or_else(|| Err("no fake endpoints".into()))
        }
    }

    fn one(id: ProviderId, models: &[&str]) -> BTreeMap<ProviderId, Result<Vec<String>, String>> {
        let mut m = BTreeMap::new();
        m.insert(id, Ok(models.iter().map(ToString::to_string).collect()));
        m
    }

    /// A listed provider returns its models, and a second `list` is served
    /// from the in-memory memo without a second fetch.
    #[test]
    fn lists_then_memoises() {
        let source = FakeSource::new(one(
            fam(ProviderKind::Anthropic),
            &["claude-opus-4", "claude-haiku-4"],
        ));
        let mut cat = ModelCatalog::in_memory(source);
        match cat.list(&fam(ProviderKind::Anthropic)) {
            Some(m) => assert_eq!(m, vec!["claude-opus-4", "claude-haiku-4"]),
            None => panic!("expected a list"),
        }
        let _ = cat.list(&fam(ProviderKind::Anthropic));
        assert_eq!(
            cat.source.calls.get(),
            1,
            "memo must prevent a second fetch"
        );
    }

    /// The routing slug is the endpoint `tag` prefix; a tag with no `/`
    /// (a single-variant provider) is its own slug.
    #[test]
    fn endpoint_slug_is_tag_prefix() {
        let with_variant = ProviderEndpoint::from_wire(EndpointWire {
            provider_name: "DeepInfra".into(),
            tag: "deepinfra/fp4".into(),
            context_length: Some(163_840),
            quantization: Some("fp4".into()),
        });
        assert_eq!(with_variant.slug, "deepinfra");
        let bare = ProviderEndpoint::from_wire(EndpointWire {
            provider_name: "StreamLake".into(),
            tag: "streamlake".into(),
            context_length: Some(128_000),
            quantization: None,
        });
        assert_eq!(bare.slug, "streamlake");
    }

    /// Recorded serving providers are served back from the in-session memo.
    #[test]
    fn endpoints_memo_round_trips() {
        let model = "deepseek/deepseek-chat";
        let mut endpoints = BTreeMap::new();
        endpoints.insert(
            model.to_string(),
            Ok(vec![ProviderEndpoint {
                provider_name: "DeepInfra".into(),
                slug: "deepinfra".into(),
                context_length: Some(163_840),
                quantization: Some("fp4".into()),
            }]),
        );
        let source = FakeSource::new(BTreeMap::new()).with_endpoints(endpoints);
        let mut cat = ModelCatalog::in_memory(source);
        assert!(cat.cached_endpoints(model).is_none());
        let fetched = cat.source().endpoints(model).unwrap();
        cat.record_endpoints(model, fetched.clone());
        assert_eq!(cat.cached_endpoints(model), Some(fetched));
    }

    /// A custom provider lists through the same catalog/source seam as a
    /// famous one, keyed by its label.
    #[test]
    fn custom_provider_lists_through_catalog() {
        let id = custom("local-llama");
        let source = FakeSource::new(one(id.clone(), &["llama-3", "llama-3-instruct"]));
        let mut cat = ModelCatalog::in_memory(source);
        assert_eq!(
            cat.list(&id),
            Some(vec!["llama-3".to_string(), "llama-3-instruct".to_string()])
        );
    }

    /// A failed fetch degrades to `None` (the picker then offers manual
    /// entry); the reason is surfaced by the source seam, which the picker
    /// reads directly for its note.
    #[test]
    fn failed_fetch_is_none_with_reason_at_the_source() {
        let mut lists = BTreeMap::new();
        lists.insert(fam(ProviderKind::Deepseek), Err("network down".to_string()));
        let mut cat = ModelCatalog::in_memory(FakeSource::new(lists));
        assert!(cat.list(&fam(ProviderKind::Deepseek)).is_none());
        assert!(
            cat.source()
                .list(&fam(ProviderKind::Deepseek))
                .unwrap_err()
                .contains("network down")
        );
    }

    /// `--model` resolves to the available provider whose list contains it.
    #[test]
    fn resolve_prefers_listing_match() {
        let mut lists = BTreeMap::new();
        lists.insert(
            fam(ProviderKind::Anthropic),
            Ok(vec!["claude-opus-4".into()]),
        );
        lists.insert(
            fam(ProviderKind::Deepseek),
            Ok(vec!["deepseek-chat".into()]),
        );
        let mut cat = ModelCatalog::in_memory(FakeSource::new(lists));
        let available = [fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)];
        assert_eq!(
            resolve_model_provider("deepseek-chat", &available, &mut cat).unwrap(),
            fam(ProviderKind::Deepseek)
        );
    }

    /// `--model` resolves to a custom provider whose live list contains the
    /// name, exactly as it does for a famous one.
    #[test]
    fn resolve_prefers_custom_listing_match() {
        let llama = custom("local-llama");
        let mut lists = BTreeMap::new();
        lists.insert(
            fam(ProviderKind::Anthropic),
            Ok(vec!["claude-opus-4".into()]),
        );
        lists.insert(llama.clone(), Ok(vec!["llama-3".into()]));
        let mut cat = ModelCatalog::in_memory(FakeSource::new(lists));
        let available = [fam(ProviderKind::Anthropic), llama.clone()];
        assert_eq!(
            resolve_model_provider("llama-3", &available, &mut cat).unwrap(),
            llama
        );
    }

    /// A `vendor/model` slug falls back to OpenRouter when no list matches.
    #[test]
    fn resolve_slug_falls_back_to_openrouter() {
        let mut cat = ModelCatalog::in_memory(FakeSource::new(BTreeMap::new()));
        let available = [fam(ProviderKind::Anthropic), fam(ProviderKind::Openrouter)];
        assert_eq!(
            resolve_model_provider("x-ai/grok-9", &available, &mut cat).unwrap(),
            fam(ProviderKind::Openrouter)
        );
    }

    /// A single available provider serves a bare name even when its list
    /// does not contain it — a scripted run need not name the provider.
    #[test]
    fn resolve_bare_name_to_sole_provider() {
        let mut cat = ModelCatalog::in_memory(FakeSource::new(BTreeMap::new()));
        let available = [fam(ProviderKind::Anthropic)];
        assert_eq!(
            resolve_model_provider("claude-future", &available, &mut cat).unwrap(),
            fam(ProviderKind::Anthropic)
        );
    }

    /// An unknown name with several providers and no slug shape errors
    /// clearly rather than guessing.
    #[test]
    fn resolve_unknown_with_many_providers_errors() {
        let mut cat = ModelCatalog::in_memory(FakeSource::new(BTreeMap::new()));
        let available = [fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)];
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

    /// `--provider` pins by label with no catalog lookup — the point being to
    /// reach a model the provider does not advertise.
    #[test]
    fn pin_provider_matches_by_label() {
        let available = [fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)];
        assert_eq!(
            resolve_pinned_provider("deepseek", &available).unwrap(),
            fam(ProviderKind::Deepseek)
        );
    }

    /// A pin matches a custom provider by its config-map label just the same.
    #[test]
    fn pin_provider_matches_custom_label() {
        let llama = custom("local-llama");
        let available = [fam(ProviderKind::Anthropic), llama.clone()];
        assert_eq!(
            resolve_pinned_provider("local-llama", &available).unwrap(),
            llama
        );
    }

    /// Pinning an unavailable provider names the available ones rather than
    /// silently falling back.
    #[test]
    fn pin_unavailable_provider_errors() {
        let available = [fam(ProviderKind::Anthropic)];
        let err = resolve_pinned_provider("openai", &available).unwrap_err();
        assert!(err.contains("not available"), "got: {err}");
        assert!(err.contains("anthropic"), "got: {err}");
    }

    /// Pinning with no providers at all is a clear error, not a panic.
    #[test]
    fn pin_with_no_providers_errors() {
        let err = resolve_pinned_provider("anthropic", &[]).unwrap_err();
        assert!(err.contains("no provider available"), "got: {err}");
    }
}
