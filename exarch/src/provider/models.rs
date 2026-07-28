//! Live model lists, cached with a TTL, behind a network seam.
//!
//! A provider's model list is the provider's to know: API-key providers list
//! through genai, `ChatGPT` subscriptions through the Codex backend. Fetching
//! is lazy and never load-bearing — a list that fails, or that omits the
//! wanted model, still leaves manual entry.
//!
//! All network I/O sits behind [`ModelSource`], so tests drive the resolution
//! logic against a fake; they take [`ModelCatalog::memo_only`], leaving the
//! disk cache and its staleness path unexercised.

use crate::provider::credential::{Credential, CredentialStore};
use crate::provider::oauth;
use crate::provider::{ProviderId, ProviderKind};
use crate::sync::LockExt;
use genai::Client;
use genai::resolver::{AuthData, Endpoint, ProviderConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// How long a cached model list stays fresh — a provider's catalog moves on
/// the order of weeks.
const TTL: Duration = Duration::from_hours(24);

const CHATGPT_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";

/// One upstream provider `OpenRouter` can route a given model to — a row of the
/// `/model` overlay's provider control, distilled from the per-model
/// `/endpoints` listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderEndpoint {
    pub provider_name: String,
    /// What `provider.order` wants in the request body: the endpoint `tag`'s
    /// prefix, which equals the `/api/v1/providers` slug.
    pub slug: String,
    pub context_length: Option<u64>,
    pub quantization: Option<String>,
}

/// The seam every fetch of a provider's model list goes through: live against
/// genai and `OpenRouter`'s REST API, an in-memory fake in tests.
pub trait ModelSource {
    /// `id`'s full model-name list.
    ///
    /// # Errors
    /// Returns `Err` with a message describing why the fetch failed; the
    /// caller degrades to manual entry.
    fn list(&self, id: &ProviderId) -> Result<Vec<String>, String>;

    /// The upstream serving providers `OpenRouter` lists for `model`. Only
    /// `OpenRouter` routes, so the picker calls this for nothing else.
    ///
    /// # Errors
    /// Returns `Err` with a message describing why the fetch failed.
    fn endpoints(&self, model: &str) -> Result<Vec<ProviderEndpoint>, String>;
}

/// The live source: genai's `all_model_names` over the in-memory credentials.
/// `Clone` is cheap, which is what lets the picker hand a copy to a background
/// fetch thread without sharing the catalog's caches.
#[derive(Clone)]
pub struct LiveSource {
    /// Cloned out of the store so a listing thread needs neither the store nor
    /// the UI thread; OAuth cells stay shared, so a refreshed token is visible.
    credentials: BTreeMap<ProviderId, Credential>,
    /// One client for every provider's listing call — the endpoint and key ride
    /// the per-call `ProviderConfig`.
    client: Client,
}

impl LiveSource {
    pub fn new(store: &CredentialStore) -> Self {
        let credentials = store
            .available()
            .into_iter()
            .filter_map(|id| store.get(&id).cloned().map(|credential| (id, credential)))
            .collect();
        Self {
            credentials,
            client: Client::builder()
                .with_reqwest(crate::provider::tls::client())
                .build(),
        }
    }

    /// Admit a credential resolved after startup — a sign-in this session, say
    /// — so its listing reads what the store now holds, with no rebuild.
    pub fn add_credential(&mut self, id: ProviderId, credential: Credential) {
        self.credentials.insert(id, credential);
    }
}

/// A runtime for one blocking listing call — listing happens a handful of
/// times a session, so a runtime per call beats holding one open. `what` names
/// the caller in the build-failure message.
fn blocking_runtime(what: &str) -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build {what} runtime: {e}"))
}

impl ModelSource for LiveSource {
    fn list(&self, id: &ProviderId) -> Result<Vec<String>, String> {
        let credential = self
            .credentials
            .get(id)
            .ok_or_else(|| format!("{} has no resolved credential", id.label()))?;
        match credential {
            Credential::ApiKey(key) => self.list_api_key(id, key),
            Credential::OAuth(cell) => Self::list_chatgpt(id, cell),
        }
    }

    fn endpoints(&self, model: &str) -> Result<Vec<ProviderEndpoint>, String> {
        let url = format!("https://openrouter.ai/api/v1/models/{model}/endpoints");
        let key = self.credentials.iter().find_map(|(id, credential)| {
            (id.famous() == Some(ProviderKind::Openrouter))
                .then(|| match credential {
                    Credential::ApiKey(key) => Some(key.clone()),
                    Credential::OAuth(_) => None,
                })
                .flatten()
        });
        let runtime = blocking_runtime("endpoints")?;
        runtime.block_on(async {
            let mut request = crate::provider::tls::client().get(&url);
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

impl LiveSource {
    fn list_api_key(&self, id: &ProviderId, key: &str) -> Result<Vec<String>, String> {
        // Endpoint and key passed explicitly, so a catalog request never leans
        // on the client's auth resolver. Both come from the `ProviderId`, so a
        // custom provider lists exactly as a famous one does.
        let provider_config = ProviderConfig {
            endpoint: id.endpoint().map(Endpoint::from_owned),
            auth: Some(AuthData::from_single(key.to_owned())),
        };
        let runtime = blocking_runtime("listing")?;
        runtime
            .block_on(
                self.client
                    .all_model_names(id.default_adapter(), provider_config),
            )
            .map_err(|e| format!("list models for {}: {e}", id.label()))
    }

    /// The subscription catalog. `client_version` must be a real Codex CLI
    /// version (`oauth::codex_client_version`), never exarch's own: the backend
    /// gates the returned models on it and answers a low one with an empty
    /// list. Authenticated from the live OAuth cell, so a token refreshed here
    /// needs no new source.
    fn list_chatgpt(
        id: &ProviderId,
        cell: &std::sync::Arc<std::sync::Mutex<oauth::OAuthToken>>,
    ) -> Result<Vec<String>, String> {
        let runtime = blocking_runtime("subscription model-list")?;
        runtime.block_on(async {
            oauth::refresh_cell_if_stale(cell)
                .await
                .map_err(|e| format!("refresh login for {}: {e}", id.label()))?;
            let token = cell.lock_ignore_poison().clone();
            let url = format!(
                "{CHATGPT_MODELS_URL}?client_version={}",
                oauth::codex_client_version()
            );
            let request = oauth::request_headers(&token, "application/json")
                .into_iter()
                .fold(crate::provider::tls::client().get(url), |r, (k, v)| {
                    r.header(k, v)
                });
            let response = request
                .send()
                .await
                .map_err(|e| format!("list models for {}: {e}", id.label()))?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(format!(
                    "list models for {}: Codex backend returned HTTP {status}: {body}",
                    id.label()
                ));
            }
            let body: CodexModelsResponse = response
                .json()
                .await
                .map_err(|e| format!("parse models for {}: {e}", id.label()))?;
            Ok(body.models.into_iter().map(|model| model.slug).collect())
        })
    }
}

#[derive(Deserialize)]
struct CodexModelsResponse {
    models: Vec<CodexModel>,
}

#[derive(Deserialize)]
struct CodexModel {
    slug: String,
}

/// `OpenRouter`'s `/endpoints` envelope; only the fields the picker shows are
/// read, and the rest of each entry (pricing, uptime, latency) ignored.
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
    /// A tag carrying no `/` is its own slug — a provider with one variant.
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

#[derive(Serialize, Deserialize, Clone)]
struct CacheEntry {
    /// Unix seconds at fetch time, checked against [`TTL`].
    fetched_at: u64,
    models: Vec<String>,
}

/// The on-disk cache, JSON under the XDG cache dir.
#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    /// Keyed by [`ProviderId::label`], so the file stays readable and outlives
    /// any reordering of [`ProviderKind`].
    providers: BTreeMap<String, CacheEntry>,
}

/// A [`ModelSource`] in front of the XDG-cached, TTL'd lists, plus a
/// per-process memo so a provider's list is fetched at most once a session.
pub struct ModelCatalog<S: ModelSource> {
    source: S,
    /// `None` disables the disk cache; `Some` is `<xdg cache>/models.json`.
    cache_path: Option<PathBuf>,
    memo: BTreeMap<ProviderId, Vec<String>>,
    /// Keyed by `OpenRouter` model id. Memo-only: serving-provider availability
    /// is volatile and the fetch cheap, so it is refetched, never persisted.
    endpoints_memo: BTreeMap<String, Vec<ProviderEndpoint>>,
}

impl<S: ModelSource> ModelCatalog<S> {
    /// A catalog persisting to `app`'s XDG cache path, when one resolves.
    pub fn new(source: S, app: crate::bootstrap::App) -> Self {
        Self {
            source,
            cache_path: cache_path(app),
            memo: BTreeMap::new(),
            endpoints_memo: BTreeMap::new(),
        }
    }

    /// A catalog with the memo alone — for tests, and any caller that must
    /// never touch a user's cache dir.
    pub fn memo_only(source: S) -> Self {
        Self {
            source,
            cache_path: None,
            memo: BTreeMap::new(),
            endpoints_memo: BTreeMap::new(),
        }
    }

    /// `model`'s serving providers if already fetched this session. The picker
    /// seeds from this and spawns a background fetch only on a miss.
    pub fn cached_endpoints(&self, model: &str) -> Option<Vec<ProviderEndpoint>> {
        self.endpoints_memo.get(model).cloned()
    }

    /// Fold a background fetch's result into the memo, on the main thread —
    /// [`Self::record`]'s counterpart for serving providers.
    pub fn record_endpoints(&mut self, model: &str, endpoints: Vec<ProviderEndpoint>) {
        self.endpoints_memo.insert(model.to_string(), endpoints);
    }

    /// `id`'s model list: the memo, then a fresh disk entry, then a live fetch
    /// that refreshes both. `None` on failure — callers degrade to manual entry.
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

    /// `id`'s list if already cached, never fetching. `Listing::open` fills
    /// from this on open and spawns a fetch only where it returns `None`.
    pub fn cached(&mut self, id: &ProviderId) -> Option<Vec<String>> {
        if let Some(models) = self.memo.get(id) {
            return Some(models.clone());
        }
        let models = self.fresh_from_disk(id)?;
        self.memo.insert(id.clone(), models.clone());
        Some(models)
    }

    /// Fold a freshly-fetched list into both caches. Fetches run on background
    /// threads and land here, on the main thread, so the disk write is serial.
    pub fn record(&mut self, id: &ProviderId, models: Vec<String>) {
        self.write_disk(id, &models);
        self.memo.insert(id.clone(), models);
    }

    /// The seam, for a background thread to clone; it reports back through
    /// [`Self::record`] rather than touching the caches itself.
    pub fn source(&self) -> &S {
        &self.source
    }

    /// `None` when the cache is absent, unreadable, missing `id`, or stale.
    fn fresh_from_disk(&self, id: &ProviderId) -> Option<Vec<String>> {
        let path = self.cache_path.as_ref()?;
        let file = read_cache(path)?;
        let entry = file.providers.get(id.label())?;
        let age = crate::bootstrap::now_secs().saturating_sub(entry.fetched_at);
        (age < TTL.as_secs()).then(|| entry.models.clone())
    }

    /// Best-effort: a cache the process cannot write is not fatal, since the
    /// memo still serves the session.
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

impl ModelCatalog<LiveSource> {
    /// Admit a freshly signed-in account without exposing the generic source.
    pub fn add_credential(&mut self, id: ProviderId, credential: Credential) {
        self.source.add_credential(id, credential);
    }
}

/// `None` when no cache base resolves (`$HOME` unset, no absolute override) —
/// the catalog then runs memo-only.
fn cache_path(app: crate::bootstrap::App) -> Option<PathBuf> {
    let dir = app.xdg_dir(ral_core::path::basedir::XdgKind::Cache);
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

/// Resolve a `--model` name to the provider that should serve it: the
/// available provider whose live list holds it, failing that a name-shape
/// match. Fetches on a cache miss, so it runs only for an explicit `--model`.
///
/// # Errors
/// Returns `Err` if no provider is available, or if no available provider
/// lists or plausibly serves `name`.
pub fn resolve_model_provider<S: ModelSource>(
    name: &str,
    available: &[ProviderId],
    catalog: &mut ModelCatalog<S>,
) -> Result<ProviderId, String> {
    if available.is_empty() {
        return Err(no_provider_error());
    }
    for id in available {
        if let Some(models) = catalog.list(id)
            && models.iter().any(|m| m == name)
        {
            return Ok(id.clone());
        }
    }
    // No listed match — fall back to the name's shape. `vendor/model` is an
    // OpenRouter convention; a bare name goes to the sole available provider,
    // so a scripted run with one key set need not name it.
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
        available_labels(available)
    ))
}

fn no_provider_error() -> String {
    "no provider available — set a provider API key (e.g. ANTHROPIC_API_KEY)".into()
}

fn available_labels(available: &[ProviderId]) -> String {
    available
        .iter()
        .map(ProviderId::label)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve an explicit `--provider` label, pinning it verbatim. No listing
/// lookup — that is the point of pinning, so the caller may then name a model
/// the provider does not advertise.
///
/// # Errors
/// Returns `Err` if no provider is available, or if no available provider's
/// label matches `name`.
pub fn resolve_pinned_provider(name: &str, available: &[ProviderId]) -> Result<ProviderId, String> {
    if let Some(id) = available.iter().find(|id| id.label() == name) {
        return Ok(id.clone());
    }
    if available.is_empty() {
        return Err(no_provider_error());
    }
    Err(format!(
        "provider '{name}' is not available ({}); set its API key or name one that is",
        available_labels(available)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::adapter::AdapterKind;
    use std::cell::Cell;
    use std::sync::Arc;

    fn fam(kind: ProviderKind) -> ProviderId {
        ProviderId::Famous(kind)
    }

    /// The facts besides `label` are immaterial: catalog and resolver key on
    /// the label alone.
    fn custom(label: &str) -> ProviderId {
        ProviderId::Custom(Arc::new(crate::provider::CustomProvider {
            label: label.into(),
            key_env: Some(format!("{}_KEY", label.to_uppercase())),
            endpoint: format!("https://{label}.example/v1/"),
            adapter: AdapterKind::OpenAI,
        }))
    }

    /// Counts fetches, so a test can assert the memo prevents a second one.
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

    #[test]
    fn lists_then_memoises() {
        let source = FakeSource::new(one(
            fam(ProviderKind::Anthropic),
            &["claude-opus-4", "claude-haiku-4"],
        ));
        let mut cat = ModelCatalog::memo_only(source);
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
        let mut cat = ModelCatalog::memo_only(source);
        assert!(cat.cached_endpoints(model).is_none());
        let fetched = cat.source().endpoints(model).unwrap();
        cat.record_endpoints(model, fetched.clone());
        assert_eq!(cat.cached_endpoints(model), Some(fetched));
    }

    #[test]
    fn custom_provider_lists_through_catalog() {
        let id = custom("local-llama");
        let source = FakeSource::new(one(id.clone(), &["llama-3", "llama-3-instruct"]));
        let mut cat = ModelCatalog::memo_only(source);
        assert_eq!(
            cat.list(&id),
            Some(vec!["llama-3".to_string(), "llama-3-instruct".to_string()])
        );
    }

    /// The catalog drops the reason on the floor; the picker reads it off the
    /// source seam instead, for the note beside its manual-entry row.
    #[test]
    fn failed_fetch_is_none_with_reason_at_the_source() {
        let mut lists = BTreeMap::new();
        lists.insert(fam(ProviderKind::Deepseek), Err("network down".to_string()));
        let mut cat = ModelCatalog::memo_only(FakeSource::new(lists));
        assert!(cat.list(&fam(ProviderKind::Deepseek)).is_none());
        assert!(
            cat.source()
                .list(&fam(ProviderKind::Deepseek))
                .unwrap_err()
                .contains("network down")
        );
    }

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
        let mut cat = ModelCatalog::memo_only(FakeSource::new(lists));
        let available = [fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)];
        assert_eq!(
            resolve_model_provider("deepseek-chat", &available, &mut cat).unwrap(),
            fam(ProviderKind::Deepseek)
        );
    }

    #[test]
    fn resolve_prefers_custom_listing_match() {
        let llama = custom("local-llama");
        let mut lists = BTreeMap::new();
        lists.insert(
            fam(ProviderKind::Anthropic),
            Ok(vec!["claude-opus-4".into()]),
        );
        lists.insert(llama.clone(), Ok(vec!["llama-3".into()]));
        let mut cat = ModelCatalog::memo_only(FakeSource::new(lists));
        let available = [fam(ProviderKind::Anthropic), llama.clone()];
        assert_eq!(
            resolve_model_provider("llama-3", &available, &mut cat).unwrap(),
            llama
        );
    }

    #[test]
    fn resolve_slug_falls_back_to_openrouter() {
        let mut cat = ModelCatalog::memo_only(FakeSource::new(BTreeMap::new()));
        let available = [fam(ProviderKind::Anthropic), fam(ProviderKind::Openrouter)];
        assert_eq!(
            resolve_model_provider("x-ai/grok-9", &available, &mut cat).unwrap(),
            fam(ProviderKind::Openrouter)
        );
    }

    /// Even when its list does not contain the name — a scripted run with one
    /// key set need not name the provider.
    #[test]
    fn resolve_bare_name_to_sole_provider() {
        let mut cat = ModelCatalog::memo_only(FakeSource::new(BTreeMap::new()));
        let available = [fam(ProviderKind::Anthropic)];
        assert_eq!(
            resolve_model_provider("claude-future", &available, &mut cat).unwrap(),
            fam(ProviderKind::Anthropic)
        );
    }

    #[test]
    fn resolve_unknown_with_many_providers_errors() {
        let mut cat = ModelCatalog::memo_only(FakeSource::new(BTreeMap::new()));
        let available = [fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)];
        let err = resolve_model_provider("mystery", &available, &mut cat).unwrap_err();
        assert!(err.contains("not listed"), "got: {err}");
    }

    #[test]
    fn resolve_with_no_providers_errors() {
        let mut cat = ModelCatalog::memo_only(FakeSource::new(BTreeMap::new()));
        let err = resolve_model_provider("anything", &[], &mut cat).unwrap_err();
        assert!(err.contains("no provider available"), "got: {err}");
    }

    /// No catalog is even threaded through — pinning skips the lookup.
    #[test]
    fn pin_provider_matches_by_label() {
        let available = [fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)];
        assert_eq!(
            resolve_pinned_provider("deepseek", &available).unwrap(),
            fam(ProviderKind::Deepseek)
        );
    }

    #[test]
    fn pin_provider_matches_custom_label() {
        let llama = custom("local-llama");
        let available = [fam(ProviderKind::Anthropic), llama.clone()];
        assert_eq!(
            resolve_pinned_provider("local-llama", &available).unwrap(),
            llama
        );
    }

    /// The error names the available providers rather than falling back to one.
    #[test]
    fn pin_unavailable_provider_errors() {
        let available = [fam(ProviderKind::Anthropic)];
        let err = resolve_pinned_provider("openai", &available).unwrap_err();
        assert!(err.contains("not available"), "got: {err}");
        assert!(err.contains("anthropic"), "got: {err}");
    }

    #[test]
    fn pin_with_no_providers_errors() {
        let err = resolve_pinned_provider("anthropic", &[]).unwrap_err();
        assert!(err.contains("no provider available"), "got: {err}");
    }
}
