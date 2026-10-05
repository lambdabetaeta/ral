//! Live model lists, cached with a TTL, behind a network seam.
//!
//! An account's model list is that account's to know: Anthropic, `DeepSeek` and
//! Gemini list through their own endpoints, other API-key services through
//! genai, `ChatGPT` accounts through the Codex backend. A listing carries each
//! model's context window where the wire reports one. Fetching is lazy and
//! never load-bearing — a list that fails, or that omits the wanted model,
//! still leaves manual entry.
//!
//! All network I/O sits behind [`ModelSource`], so tests drive the resolution
//! logic against a fake; the unit tests here take [`ModelCatalog::memo_only`],
//! and `tests/model_cache.rs` drives the disk cache and its staleness path
//! against a real `XDG_CACHE_HOME`.

use crate::provider::credential::{Credential, CredentialStore, Roster};
use crate::provider::error::body_detail;
use crate::provider::identity::{self, Account, AccountId};
use crate::provider::oauth;
use genai::Client;
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint, ProviderConfig};
use ral_core::sync::LockExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// How long a cached model list stays fresh.
const TTL: Duration = Duration::from_hours(6);

const CHATGPT_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const ANTHROPIC_BASE: &str = "https://api.anthropic.com/v1/";
const DEEPSEEK_BASE: &str = "https://api.deepseek.com/v1/";
const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// A model a listing names, with the context window its provider reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    pub id: String,
    pub context_window: Option<u64>,
}

impl Listed {
    /// A model whose provider reports no window.
    pub fn bare(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            context_window: None,
        }
    }
}

/// One page of a listing, and the cursor of the next, if any.
type Page = (Vec<Listed>, Option<String>);

/// A window of `0` or `null` on the wire is no window.
fn window(wire: &Value) -> Option<u64> {
    wire.as_u64().filter(|&n| n > 0)
}

fn id_of(entry: &Value, key: &str) -> Option<String> {
    entry.get(key)?.as_str().map(str::to_owned)
}

/// One page of Anthropic's `GET /models`, and the `after_id` of the next.
fn anthropic_page(body: &Value) -> Page {
    let listed = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(Listed {
                id: id_of(m, "id")?,
                context_window: window(&m["max_input_tokens"]),
            })
        })
        .collect();
    let next = body["has_more"]
        .as_bool()
        .unwrap_or(false)
        .then(|| id_of(body, "last_id"))
        .flatten();
    (listed, next)
}

/// `DeepSeek`'s `GET /models`, which has no paging.
fn deepseek_page(body: &Value) -> Page {
    let listed = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(Listed {
                id: id_of(m, "id")?,
                context_window: window(&m["context_window"]),
            })
        })
        .collect();
    (listed, None)
}

/// One page of Gemini's `GET /models`, and the `pageToken` of the next.
fn gemini_page(body: &Value) -> Page {
    let listed = body["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let name = id_of(m, "name")?;
            Some(Listed {
                id: name
                    .strip_prefix("models/")
                    .map_or_else(|| name.clone(), str::to_owned),
                context_window: window(&m["inputTokenLimit"]),
            })
        })
        .collect();
    (listed, id_of(body, "nextPageToken"))
}

/// The account's `models` URL — its declared endpoint, else `default` — with
/// `params` percent-encoded onto it.
fn paged(
    account: &Account,
    default: &str,
    params: &[(&str, &str)],
) -> Result<reqwest::Url, String> {
    let base = account.service.endpoint.as_deref().unwrap_or(default);
    let sep = if base.ends_with('/') { "" } else { "/" };
    reqwest::Url::parse_with_params(&format!("{base}{sep}models"), params)
        .map_err(|e| format!("bad endpoint {base}: {e}"))
}

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

/// The seam every fetch of an account's model list goes through: live against
/// genai and `OpenRouter`'s REST API, an in-memory fake in tests.
pub trait ModelSource {
    /// `account`'s full model list.
    ///
    /// # Errors
    /// Returns `Err` with a message describing why the fetch failed; the
    /// caller degrades to manual entry.
    fn list(&self, account: &AccountId) -> Result<Vec<Listed>, String>;

    /// The upstream serving providers `OpenRouter` lists for `model`. Only a
    /// routing service reaches this, so the picker calls it for nothing else.
    ///
    /// # Errors
    /// Returns `Err` with a message describing why the fetch failed.
    fn endpoints(&self, model: &str) -> Result<Vec<ProviderEndpoint>, String>;
}

/// The live source over the in-memory credentials.
///
/// `Clone` is cheap, which is what lets the picker hand a copy to a
/// background fetch thread without sharing the catalog's caches.
#[derive(Clone)]
pub struct LiveSource {
    roster: Roster,
    /// One client for every account's listing call — the endpoint and key ride
    /// the per-call `ProviderConfig`.
    client: Client,
}

impl LiveSource {
    /// # Panics
    /// Never: `with_reqwest` supplies the client, so genai's `build()` cannot fail.
    pub fn new(store: &CredentialStore) -> Self {
        Self {
            roster: store.roster(),
            client: Client::builder()
                .with_reqwest(crate::provider::tls::client())
                .build()
                .expect("with_reqwest supplies the client, so build() cannot fail"),
        }
    }

    pub fn add_credential(&mut self, account: Account, credential: Credential) {
        self.roster.admit(account, credential);
    }
}

/// A runtime for one blocking listing call — listing happens a handful of
/// times a session, so a runtime per call beats holding one open. `what` names
/// the caller in the build-failure message.
pub(super) fn blocking_runtime(what: &str) -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build {what} runtime: {e}"))
}

impl ModelSource for LiveSource {
    fn list(&self, id: &AccountId) -> Result<Vec<Listed>, String> {
        let account = self
            .roster
            .account(id)
            .ok_or_else(|| format!("{id} is not a known account"))?;
        let credential = self
            .roster
            .credential(id)
            .ok_or_else(|| format!("{} has no resolved credential", self.roster.label(account)))?;
        match credential {
            Credential::ApiKey(key) => self.list_api_key(account, key),
            Credential::OAuth(cell) => self.list_chatgpt(account, cell),
        }
    }

    fn endpoints(&self, model: &str) -> Result<Vec<ProviderEndpoint>, String> {
        let url = format!("https://openrouter.ai/api/v1/models/{model}/endpoints");
        let key = self
            .roster
            .accounts()
            .iter()
            .find(|account| account.service.routes)
            .and_then(|account| match self.roster.credential(&account.id) {
                Some(Credential::ApiKey(key)) => Some(key.clone()),
                _ => None,
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
    fn list_api_key(&self, account: &Account, key: &str) -> Result<Vec<Listed>, String> {
        let label = self.roster.label(account);
        let runtime = blocking_runtime("listing")?;
        let adapter = account.service.adapter;
        if matches!(
            adapter,
            AdapterKind::Anthropic | AdapterKind::DeepSeek | AdapterKind::Gemini
        ) {
            return runtime
                .block_on(list_windowed(adapter, account, key))
                .map_err(|e| format!("list models for {label}: {e}"));
        }
        // Endpoint and key passed explicitly, so a catalog request never leans
        // on the client's auth resolver. Both come from the account's
        // service, so a declared endpoint lists exactly as a built-in one does.
        let provider_config = ProviderConfig {
            endpoint: account.service.endpoint.clone().map(Endpoint::from_owned),
            auth: Some(AuthData::from_single(key.to_owned())),
        };
        runtime
            .block_on(self.client.all_model_names(adapter, provider_config))
            .map(|names| names.into_iter().map(Listed::bare).collect())
            .map_err(|e| format!("list models for {label}: {e}"))
    }

    /// The subscription catalog. `client_version` must be a real Codex CLI
    /// version (`oauth::codex_client_version`), never exarch's own: the backend
    /// gates the returned models on it and answers a low one with an empty
    /// list. Authenticated from the live OAuth cell, so a token refreshed here
    /// needs no new source.
    fn list_chatgpt(
        &self,
        account: &Account,
        cell: &std::sync::Arc<std::sync::Mutex<oauth::OAuthToken>>,
    ) -> Result<Vec<Listed>, String> {
        let runtime = blocking_runtime("subscription model-list")?;
        runtime.block_on(async {
            oauth::refresh_cell_if_stale(cell)
                .await
                .map_err(|e| format!("refresh login for {}: {e}", self.roster.label(account)))?;
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
                .map_err(|e| format!("list models for {}: {e}", self.roster.label(account)))?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(format!(
                    "list models for {}: Codex backend returned HTTP {status}: {}",
                    self.roster.label(account),
                    body_detail(&body)
                ));
            }
            let body: CodexModelsResponse = response
                .json()
                .await
                .map_err(|e| format!("parse models for {}: {e}", self.roster.label(account)))?;
            Ok(body
                .models
                .into_iter()
                .map(|model| Listed {
                    id: model.slug,
                    context_window: model.context_window.filter(|&n| n > 0),
                })
                .collect())
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
    #[serde(default)]
    context_window: Option<u64>,
}

/// Follow one of the three windowed listings to its last page, at the largest
/// page size each allows.
async fn list_windowed(
    adapter: AdapterKind,
    account: &Account,
    key: &str,
) -> Result<Vec<Listed>, String> {
    let client = crate::provider::tls::client();
    let mut listed = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (request, parse): (_, fn(&Value) -> Page) = match adapter {
            AdapterKind::Anthropic => {
                let mut params = vec![("limit", "1000")];
                params.extend(cursor.as_deref().map(|after| ("after_id", after)));
                let request = client
                    .get(paged(account, ANTHROPIC_BASE, &params)?)
                    .header("x-api-key", key)
                    .header("anthropic-version", ANTHROPIC_VERSION);
                (request, anthropic_page)
            }
            AdapterKind::Gemini => {
                let mut params = vec![("pageSize", "1000")];
                params.extend(cursor.as_deref().map(|token| ("pageToken", token)));
                let request = client
                    .get(paged(account, GEMINI_BASE, &params)?)
                    .header("x-goog-api-key", key);
                (request, gemini_page)
            }
            _ => (
                client
                    .get(paged(account, DEEPSEEK_BASE, &[])?)
                    .bearer_auth(key),
                deepseek_page,
            ),
        };
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {}", body_detail(&body)));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("unreadable listing: {e}"))?;
        let (page, next) = parse(&body);
        listed.extend(page);
        match next {
            Some(next) => cursor = Some(next),
            None => return Ok(listed),
        }
    }
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
    models: Vec<Listed>,
}

/// The on-disk cache, JSON under the XDG cache dir.
#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    /// Keyed by [`AccountId::as_str`], so the file stays readable and outlives
    /// any change to how an account is displayed. For every account but a
    /// `ChatGPT` login the id *is* the account's own name, so an existing
    /// entry keeps resolving unchanged.
    providers: BTreeMap<String, CacheEntry>,
}

/// A [`ModelSource`] in front of the XDG-cached, TTL'd lists, plus a
/// per-process memo so an account's list is fetched at most once a session.
pub struct ModelCatalog<S: ModelSource> {
    source: S,
    /// `None` disables the disk cache; `Some` is `<xdg cache>/models.json`.
    cache_path: Option<PathBuf>,
    memo: BTreeMap<AccountId, Vec<Listed>>,
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

    /// `account`'s model names: the memo, then a fresh disk entry, then a live
    /// fetch that refreshes both. `None` on failure — callers degrade to
    /// manual entry.
    pub fn list(&mut self, account: &AccountId) -> Option<Vec<String>> {
        if let Some(listed) = self.cached_listing(account) {
            return Some(names(&listed));
        }
        let listed = self.source.list(account).ok()?;
        let models = names(&listed);
        self.record(account, listed);
        Some(models)
    }

    /// `account`'s model names if already cached, never fetching.
    /// `Listing::open` fills from this on open and spawns a fetch only where
    /// it returns `None`.
    pub fn cached(&mut self, account: &AccountId) -> Option<Vec<String>> {
        self.cached_listing(account).map(|listed| names(&listed))
    }

    /// `account`'s full listing, windows included, if already cached: the
    /// memo, then a fresh disk entry. Never fetches.
    pub fn cached_listing(&mut self, account: &AccountId) -> Option<Vec<Listed>> {
        if let Some(listed) = self.memo.get(account) {
            return Some(listed.clone());
        }
        let listed = self.fresh_from_disk(account)?;
        self.memo.insert(account.clone(), listed.clone());
        Some(listed)
    }

    /// Fold a freshly-fetched list into both caches. Fetches run on background
    /// threads and land here, on the main thread, so the disk write is serial.
    pub fn record(&mut self, account: &AccountId, listed: Vec<Listed>) {
        self.write_disk(account, &listed);
        self.memo.insert(account.clone(), listed);
    }

    /// The seam, for a background thread to clone; it reports back through
    /// [`Self::record`] rather than touching the caches itself.
    pub fn source(&self) -> &S {
        &self.source
    }

    /// `None` when the cache is absent, unreadable, missing `account`, or stale.
    fn fresh_from_disk(&self, account: &AccountId) -> Option<Vec<Listed>> {
        let path = self.cache_path.as_ref()?;
        let file = read_cache(path)?;
        let entry = file.providers.get(account.as_str())?;
        let age = crate::bootstrap::now_secs().saturating_sub(entry.fetched_at);
        (age < TTL.as_secs()).then(|| entry.models.clone())
    }

    /// Best-effort: a cache the process cannot write is not fatal, since the
    /// memo still serves the session.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:models-cache-write] persists the model catalog cache; registry infra, not turn-time data I/O"
    )]
    fn write_disk(&self, account: &AccountId, models: &[Listed]) {
        let Some(path) = self.cache_path.as_ref() else {
            return;
        };
        let mut file = read_cache(path).unwrap_or_default();
        file.providers.insert(
            account.as_str().to_string(),
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
    pub fn add_credential(&mut self, account: Account, credential: Credential) {
        self.source.add_credential(account, credential);
    }
}

pub fn names(listed: &[Listed]) -> Vec<String> {
    listed.iter().map(|m| m.id.clone()).collect()
}

/// `None` when no cache base resolves (`HOME` unset, no absolute override) —
/// the catalog then runs memo-only.
fn cache_path(app: crate::bootstrap::App) -> Option<PathBuf> {
    let dir = app.xdg_dir(ral_core::path::basedir::XdgKind::Cache);
    dir.is_absolute().then(|| dir.join("models.json"))
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:models-cache-read] reads the model catalog cache; registry infra, not turn-time data I/O"
)]
fn read_cache(path: &PathBuf) -> Option<CacheFile> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn no_provider_error() -> String {
    "no provider available — set a provider API key (e.g. ANTHROPIC_API_KEY)".into()
}

/// Resolve a stored `AccountId` rendering against the accounts present.
///
/// `state.json`'s `provider`, or a wire selection. No name arm, ever: a
/// rendering is compared only against other renderings, never parsed, so a
/// stale selection can fall back to a default but never lands on a same-named
/// stranger.
pub fn resolve_account(id: &str, available: &[Account]) -> Option<Account> {
    available
        .iter()
        .find(|account| account.id.as_str() == id)
        .cloned()
}

/// Resolve a `--model` name to the account that should serve it: every
/// available account whose live list holds it, failing that a name-shape
/// match.
///
/// Fetches on a cache miss, so it runs only for an explicit `--model`.
///
/// # Errors
/// Returns `Err` if no account is available, if no available account lists
/// or plausibly serves `name`, or if more than one account's catalog lists
/// it — a silent arbitrary choice among credentials is exactly the bug this
/// resolver exists to refuse, so it asks for `--provider` instead.
pub fn resolve_model_provider<S: ModelSource>(
    name: &str,
    available: &[Account],
    catalog: &mut ModelCatalog<S>,
) -> Result<Account, String> {
    if available.is_empty() {
        return Err(no_provider_error());
    }
    let listed: Vec<&Account> = available
        .iter()
        .filter(|account| {
            catalog
                .list(&account.id)
                .is_some_and(|models| models.iter().any(|m| m == name))
        })
        .collect();
    match listed.as_slice() {
        [one] => return Ok((*one).clone()),
        [] => {}
        many => {
            return Err(format!(
                "model '{name}' is listed by more than one available account ({}) — \
                 pass --provider to say which",
                many.iter()
                    .map(|account| identity::label(account, available))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    // No listed match — fall back to the name's shape. `vendor/model` is a
    // routing convention; a bare name goes to the sole available account, so
    // a scripted run with one key set need not name it.
    if name.contains('/')
        && let Some(account) = available.iter().find(|account| account.service.routes)
    {
        return Ok(account.clone());
    }
    if let [only] = available {
        return Ok(only.clone());
    }
    Err(format!(
        "model '{name}' is not listed by any available account ({}); \
         pass a model that one of them serves",
        identity::roster(available)
    ))
}

/// Resolve an explicit `--provider` name: an account id, then a service name,
/// then a handle.
///
/// That is the order a human is likely to type, and the order that lets a bare
/// service name still mean something once it names several `ChatGPT` accounts.
/// No listing lookup — that is the point of pinning, so the caller may then
/// name a model the account does not advertise.
///
/// # Errors
/// Returns `Err` if no account is available, if `name` answers to none, or
/// if it answers to more than one — naming the candidates rather than
/// guessing among them.
pub fn resolve_pinned_provider(name: &str, available: &[Account]) -> Result<Account, String> {
    if available.is_empty() {
        return Err(no_provider_error());
    }
    if let Some(account) = available.iter().find(|account| account.id.as_str() == name) {
        return Ok(account.clone());
    }
    let by_service: Vec<&Account> = available
        .iter()
        .filter(|account| account.service.name.as_str() == name)
        .collect();
    let candidates = if by_service.is_empty() {
        available
            .iter()
            .filter(|account| account.handle == name)
            .collect::<Vec<_>>()
    } else {
        by_service
    };
    match candidates.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(format!(
            "provider '{name}' is not available ({}); set its API key or name one that is",
            identity::roster(available)
        )),
        many => Err(format!(
            "'{name}' names {} signed-in accounts ({}) — pass the account id instead \
             (`--provider <id>`) to say which",
            many.len(),
            many.iter()
                .map(|account| identity::label(account, available))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Counts fetches, so a test can assert the memo prevents a second one.
    struct FakeSource {
        lists: BTreeMap<AccountId, Result<Vec<Listed>, String>>,
        endpoints: BTreeMap<String, Result<Vec<ProviderEndpoint>, String>>,
        calls: Cell<usize>,
    }

    impl FakeSource {
        fn new(lists: Lists) -> Self {
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
        fn list(&self, account: &AccountId) -> Result<Vec<Listed>, String> {
            self.calls.set(self.calls.get() + 1);
            self.lists
                .get(account)
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

    type Lists = BTreeMap<AccountId, Result<Vec<Listed>, String>>;

    fn bare(models: &[&str]) -> Vec<Listed> {
        models.iter().map(|m| Listed::bare(*m)).collect()
    }

    fn one(account: &Account, models: &[&str]) -> Lists {
        let mut m = BTreeMap::new();
        m.insert(account.id.clone(), Ok(bare(models)));
        m
    }

    #[test]
    fn lists_then_memoises() {
        let anthropic = Account::built_in("anthropic");
        let source = FakeSource::new(one(&anthropic, &["claude-opus-4", "claude-haiku-4"]));
        let mut cat = ModelCatalog::memo_only(source);
        match cat.list(&anthropic.id) {
            Some(m) => assert_eq!(m, vec!["claude-opus-4", "claude-haiku-4"]),
            None => panic!("expected a list"),
        }
        let _ = cat.list(&anthropic.id);
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

    /// The catalog drops the reason on the floor; the picker reads it off the
    /// source seam instead, for the note beside its manual-entry row.
    #[test]
    fn failed_fetch_is_none_with_reason_at_the_source() {
        let deepseek = Account::built_in("deepseek");
        let mut lists = BTreeMap::new();
        lists.insert(deepseek.id.clone(), Err("network down".to_string()));
        let mut cat = ModelCatalog::memo_only(FakeSource::new(lists));
        assert!(cat.list(&deepseek.id).is_none());
        assert!(
            cat.source()
                .list(&deepseek.id)
                .unwrap_err()
                .contains("network down")
        );
    }

    #[test]
    fn resolve_prefers_listing_match() {
        let anthropic = Account::built_in("anthropic");
        let deepseek = Account::built_in("deepseek");
        let mut lists = BTreeMap::new();
        lists.insert(anthropic.id.clone(), Ok(bare(&["claude-opus-4"])));
        lists.insert(deepseek.id.clone(), Ok(bare(&["deepseek-chat"])));
        let mut cat = ModelCatalog::memo_only(FakeSource::new(lists));
        let available = [anthropic, deepseek.clone()];
        assert_eq!(
            resolve_model_provider("deepseek-chat", &available, &mut cat).unwrap(),
            deepseek
        );
    }

    #[test]
    fn resolve_prefers_custom_listing_match() {
        let anthropic = Account::built_in("anthropic");
        let llama = Account::declared("local-llama");
        let mut lists = BTreeMap::new();
        lists.insert(anthropic.id.clone(), Ok(bare(&["claude-opus-4"])));
        lists.insert(llama.id.clone(), Ok(bare(&["llama-3"])));
        let mut cat = ModelCatalog::memo_only(FakeSource::new(lists));
        let available = [anthropic, llama.clone()];
        assert_eq!(
            resolve_model_provider("llama-3", &available, &mut cat).unwrap(),
            llama
        );
    }

    #[test]
    fn resolve_two_accounts_listing_one_model_is_refused_naming_both() {
        let personal = Account::chatgpt("acc-1", "alex@bristol.ac.uk");
        let work = Account::chatgpt("acc-2", "alex@work (Acme Ltd)");
        let mut lists = BTreeMap::new();
        lists.insert(personal.id.clone(), Ok(bare(&["gpt-5.5"])));
        lists.insert(work.id.clone(), Ok(bare(&["gpt-5.5"])));
        let mut cat = ModelCatalog::memo_only(FakeSource::new(lists));
        let available = [personal, work];
        let err = resolve_model_provider("gpt-5.5", &available, &mut cat).unwrap_err();
        assert!(err.contains("--provider"), "{err}");
        assert!(err.contains("alex@bristol.ac.uk"), "{err}");
        assert!(err.contains("alex@work"), "{err}");
    }

    #[test]
    fn resolve_slug_falls_back_to_openrouter() {
        let anthropic = Account::built_in("anthropic");
        let openrouter = Account::built_in("openrouter");
        let mut cat = ModelCatalog::memo_only(FakeSource::new(BTreeMap::new()));
        let available = [anthropic, openrouter.clone()];
        assert_eq!(
            resolve_model_provider("x-ai/grok-9", &available, &mut cat).unwrap(),
            openrouter
        );
    }

    /// Even when its list does not contain the name — a scripted run with one
    /// key set need not name the provider.
    #[test]
    fn resolve_bare_name_to_sole_provider() {
        let anthropic = Account::built_in("anthropic");
        let mut cat = ModelCatalog::memo_only(FakeSource::new(BTreeMap::new()));
        let available = [anthropic.clone()];
        assert_eq!(
            resolve_model_provider("claude-future", &available, &mut cat).unwrap(),
            anthropic
        );
    }

    #[test]
    fn resolve_unknown_with_many_providers_errors() {
        let available = [
            Account::built_in("anthropic"),
            Account::built_in("deepseek"),
        ];
        let mut cat = ModelCatalog::memo_only(FakeSource::new(BTreeMap::new()));
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
    fn pin_provider_matches_by_service_name() {
        let available = [
            Account::built_in("anthropic"),
            Account::built_in("deepseek"),
        ];
        assert_eq!(
            resolve_pinned_provider("deepseek", &available).unwrap(),
            Account::built_in("deepseek")
        );
    }

    #[test]
    fn pin_provider_matches_custom_name() {
        let llama = Account::declared("local-llama");
        let available = [Account::built_in("anthropic"), llama.clone()];
        assert_eq!(
            resolve_pinned_provider("local-llama", &available).unwrap(),
            llama
        );
    }

    /// The error names the available accounts rather than falling back to one.
    #[test]
    fn pin_unavailable_provider_errors() {
        let available = [Account::built_in("anthropic")];
        let err = resolve_pinned_provider("openai", &available).unwrap_err();
        assert!(err.contains("not available"), "got: {err}");
        assert!(err.contains("anthropic"), "got: {err}");
    }

    #[test]
    fn pin_with_no_providers_errors() {
        let err = resolve_pinned_provider("anthropic", &[]).unwrap_err();
        assert!(err.contains("no provider available"), "got: {err}");
    }

    /// A bare service name naming two `ChatGPT` accounts is refused, naming
    /// both, rather than picking whichever the store happened to list first.
    #[test]
    fn pin_a_service_name_naming_two_accounts_is_refused() {
        let personal = Account::chatgpt("acc-1", "alex@bristol.ac.uk");
        let work = Account::chatgpt("acc-2", "alex@work");
        let available = [personal, work];
        let err = resolve_pinned_provider("chatgpt", &available).unwrap_err();
        assert!(err.contains("account id"), "{err}");
        assert!(
            err.contains("alex@bristol.ac.uk") && err.contains("alex@work"),
            "{err}"
        );
    }

    #[test]
    fn anthropic_page_reads_windows_and_the_cursor() {
        let body = serde_json::json!({
            "data": [
                {"id": "claude-opus-4-1", "max_input_tokens": 200_000},
                {"id": "claude-old", "max_input_tokens": null},
            ],
            "has_more": true,
            "last_id": "claude-old",
        });
        let (listed, next) = anthropic_page(&body);
        assert_eq!(
            listed,
            vec![
                Listed {
                    id: "claude-opus-4-1".into(),
                    context_window: Some(200_000)
                },
                Listed::bare("claude-old"),
            ]
        );
        assert_eq!(next.as_deref(), Some("claude-old"));
        let last = serde_json::json!({"data": [], "has_more": false, "last_id": "x"});
        assert_eq!(anthropic_page(&last).1, None);
    }

    #[test]
    fn deepseek_page_reads_windows() {
        let body = serde_json::json!({"data": [
            {"id": "deepseek-flash", "context_window": 1_000_000},
            {"id": "deepseek-zero", "context_window": 0},
        ]});
        let (listed, next) = deepseek_page(&body);
        assert_eq!(listed[0].context_window, Some(1_000_000));
        assert_eq!(listed[1], Listed::bare("deepseek-zero"));
        assert_eq!(next, None);
    }

    #[test]
    fn gemini_page_strips_the_prefix_and_reads_the_token() {
        let body = serde_json::json!({
            "models": [{"name": "models/gemini-3-pro", "inputTokenLimit": 1_048_576}],
            "nextPageToken": "tok",
        });
        let (listed, next) = gemini_page(&body);
        assert_eq!(
            listed,
            vec![Listed {
                id: "gemini-3-pro".into(),
                context_window: Some(1_048_576)
            }]
        );
        assert_eq!(next.as_deref(), Some("tok"));
    }

    #[test]
    fn codex_models_parse_their_windows() {
        let body: CodexModelsResponse = serde_json::from_value(serde_json::json!({
            "models": [{"slug": "gpt-5.5", "context_window": 272_000}, {"slug": "gpt-x"}],
        }))
        .unwrap();
        assert_eq!(body.models[0].context_window, Some(272_000));
        assert_eq!(body.models[1].context_window, None);
    }
}
