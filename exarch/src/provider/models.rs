//! Live model lists, cached with a TTL, behind a network seam.
//!
//! An account's model list is that account's to know: Anthropic, `DeepSeek` and
//! Gemini list through their own endpoints ([`native`]), other API-key services
//! through genai, `ChatGPT` accounts through the Codex backend. A listing
//! carries each model's context window where the wire reports one. The
//! listing is the authority on which models an account serves: a model it
//! does not name is never run on that account.
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
use genai::resolver::{AuthData, Endpoint, ProviderConfig};
use native::Native;
use ral_core::sync::LockExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

mod native;

/// How long a cached model list stays fresh.
const TTL: Duration = Duration::from_hours(6);

const CHATGPT_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";

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
    /// Returns `Err` with a message describing why the fetch failed.
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
        let runtime = blocking_runtime("listing")?;
        let listing = match Native::of(account.service.adapter) {
            Some(native) => runtime.block_on(native.list(account, key)),
            None => runtime.block_on(self.list_genai(account, key)),
        };
        listing.map_err(|e| format!("list models for {}: {e}", self.roster.label(account)))
    }

    /// Endpoint and key passed explicitly, so a catalog request never leans on
    /// the client's auth resolver. Both come from the account's service, so a
    /// declared endpoint lists exactly as a built-in one does.
    async fn list_genai(&self, account: &Account, key: &str) -> Result<Vec<Listed>, String> {
        let provider_config = ProviderConfig {
            endpoint: account.service.endpoint.clone().map(Endpoint::from_owned),
            auth: Some(AuthData::from_single(key.to_owned())),
        };
        self.client
            .all_model_names(account.service.adapter, provider_config)
            .await
            .map(|names| names.into_iter().map(Listed::bare).collect())
            .map_err(|e| e.to_string())
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
    pub fn new(source: S, app: crate::app::App) -> Self {
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
        let age = crate::app::now_secs().saturating_sub(entry.fetched_at);
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
                fetched_at: crate::app::now_secs(),
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

/// `account`'s listing: the catalog's cache, else one fetch made with the
/// catalog unlocked and folded back in.
///
/// # Errors
/// The fetch's own reason, when nothing is cached and the fetch fails.
pub fn listing_of<S: ModelSource + Clone>(
    catalog: &Mutex<ModelCatalog<S>>,
    account: &AccountId,
) -> Result<Vec<Listed>, String> {
    let source = {
        let mut catalog = catalog.lock_ignore_poison();
        if let Some(listed) = catalog.cached_listing(account) {
            return Ok(listed);
        }
        catalog.source().clone()
    };
    let listed = source.list(account)?;
    catalog.lock_ignore_poison().record(account, listed.clone());
    Ok(listed)
}

/// `None` when no cache base resolves (`HOME` unset, no absolute override) —
/// the catalog then runs memo-only.
fn cache_path(app: crate::app::App) -> Option<PathBuf> {
    let dir = app.xdg_dir(ral_core::host::XdgKind::Cache);
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
    "no provider available: set a provider API key (e.g. ANTHROPIC_API_KEY)".into()
}

/// Resolve a stored `AccountId` rendering against the accounts present.
///
/// `state.json`'s `provider`, or a wire selection. No name arm, ever: a
/// rendering is compared only against other renderings, never parsed, so a
/// stale selection is lost rather than landing on a same-named stranger.
pub fn resolve_account(id: &str, available: &[Account]) -> Option<Account> {
    available
        .iter()
        .find(|account| account.id.as_str() == id)
        .cloned()
}

/// Resolve a `--model` name to the one available account whose listing
/// names it.
///
/// # Errors
/// Returns `Err` if no account is available, if none lists `name` — naming
/// those whose listing could not be had — or if several do, since a choice
/// among credentials is the user's to make with `--provider`.
pub fn resolve_model_provider(
    name: &str,
    available: &[Account],
    mut listing: impl FnMut(&Account) -> Result<Vec<Listed>, String>,
) -> Result<Account, String> {
    if available.is_empty() {
        return Err(no_provider_error());
    }
    let label = |account: &Account| identity::label(account, available);
    let listings: Vec<_> = available
        .iter()
        .map(|account| (account, listing(account)))
        .collect();
    let serving: Vec<&Account> = listings
        .iter()
        .filter(|(_, listed)| {
            listed
                .as_ref()
                .is_ok_and(|models| models.iter().any(|m| m.id == name))
        })
        .map(|(account, _)| *account)
        .collect();
    match serving.as_slice() {
        [one] => Ok((*one).clone()),
        [] => {
            let unlisted: Vec<String> = listings
                .iter()
                .filter_map(|(account, listed)| {
                    let why = listed.as_ref().err()?;
                    Some(format!("{} ({why})", label(account)))
                })
                .collect();
            let unlisted = if unlisted.is_empty() {
                String::new()
            } else {
                format!("; could not list the models of {}", unlisted.join(", "))
            };
            Err(format!(
                "model '{name}' is not listed by any available account ({}){unlisted}",
                identity::roster(available)
            ))
        }
        many => Err(format!(
            "model '{name}' is listed by more than one available account ({}): \
             pass --provider to say which",
            many.iter()
                .map(|account| label(account))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Resolve an explicit `--provider` name: an account id, then a service name,
/// then a handle.
///
/// That is the order a human is likely to type, and the order that lets a bare
/// service name still mean something once it names several `ChatGPT` accounts.
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
            "'{name}' names {} signed-in accounts ({}): pass the account id instead \
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
    use std::rc::Rc;

    /// Counts fetches across its clones, so a test can assert the memo
    /// prevents a second one.
    #[derive(Clone)]
    struct FakeSource {
        lists: Lists,
        endpoints: BTreeMap<String, Result<Vec<ProviderEndpoint>, String>>,
        calls: Rc<Cell<usize>>,
    }

    impl FakeSource {
        fn new(lists: Lists) -> Self {
            Self {
                lists,
                endpoints: BTreeMap::new(),
                calls: Rc::default(),
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

    /// A listing lookup over `lists`, as `resolve_model_provider` takes one.
    fn lookup(lists: &Lists) -> impl FnMut(&Account) -> Result<Vec<Listed>, String> + '_ {
        |account| {
            lists
                .get(&account.id)
                .cloned()
                .unwrap_or_else(|| Err("unreachable".into()))
        }
    }

    #[test]
    fn a_listing_is_fetched_once_then_served_from_the_memo() {
        let anthropic = Account::built_in("anthropic");
        let lists = Lists::from([(anthropic.id.clone(), Ok(bare(&["model-a", "model-b"])))]);
        let source = FakeSource::new(lists);
        let calls = Rc::clone(&source.calls);
        let catalog = Mutex::new(ModelCatalog::memo_only(source));
        assert_eq!(
            listing_of(&catalog, &anthropic.id),
            Ok(bare(&["model-a", "model-b"]))
        );
        let _ = listing_of(&catalog, &anthropic.id);
        assert_eq!(calls.get(), 1, "the memo must prevent a second fetch");
    }

    #[test]
    fn a_failed_fetch_keeps_its_reason_and_caches_nothing() {
        let deepseek = Account::built_in("deepseek");
        let lists = Lists::from([(deepseek.id.clone(), Err("network down".to_string()))]);
        let catalog = Mutex::new(ModelCatalog::memo_only(FakeSource::new(lists)));
        assert_eq!(
            listing_of(&catalog, &deepseek.id),
            Err("network down".to_string())
        );
        assert_eq!(
            catalog.lock_ignore_poison().cached_listing(&deepseek.id),
            None
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
        let model = "vendor/model-a";
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
    fn resolve_finds_the_account_that_lists_the_model() {
        let anthropic = Account::built_in("anthropic");
        let llama = Account::declared("local-llama");
        let lists = Lists::from([
            (anthropic.id.clone(), Ok(bare(&["model-a"]))),
            (llama.id.clone(), Ok(bare(&["model-b"]))),
        ]);
        let available = [anthropic, llama.clone()];
        assert_eq!(
            resolve_model_provider("model-b", &available, lookup(&lists)),
            Ok(llama)
        );
    }

    #[test]
    fn resolve_two_accounts_listing_one_model_is_refused_naming_both() {
        let personal = Account::chatgpt("acc-1", "alex@bristol.ac.uk");
        let work = Account::chatgpt("acc-2", "alex@work (Acme Ltd)");
        let lists = Lists::from([
            (personal.id.clone(), Ok(bare(&["model-a"]))),
            (work.id.clone(), Ok(bare(&["model-a"]))),
        ]);
        let available = [personal, work];
        let err = resolve_model_provider("model-a", &available, lookup(&lists)).unwrap_err();
        assert!(err.contains("--provider"), "{err}");
        assert!(err.contains("alex@bristol.ac.uk"), "{err}");
        assert!(err.contains("alex@work"), "{err}");
    }

    /// Neither a sole account nor a `vendor/model` shape stands in for a
    /// listing that names the model.
    #[test]
    fn resolve_never_guesses_from_the_name_or_the_roster() {
        let openrouter = Account::built_in("openrouter");
        let lists = Lists::from([(openrouter.id.clone(), Ok(bare(&["vendor/model-a"])))]);
        let available = [openrouter];
        let err = resolve_model_provider("vendor/model-b", &available, lookup(&lists)).unwrap_err();
        assert!(err.contains("not listed"), "got: {err}");
    }

    #[test]
    fn resolve_names_the_accounts_it_could_not_list() {
        let anthropic = Account::built_in("anthropic");
        let deepseek = Account::built_in("deepseek");
        let lists = Lists::from([
            (anthropic.id.clone(), Ok(bare(&["model-a"]))),
            (deepseek.id.clone(), Err("network down".to_string())),
        ]);
        let available = [anthropic, deepseek];
        let err = resolve_model_provider("model-b", &available, lookup(&lists)).unwrap_err();
        assert!(err.contains("deepseek (network down)"), "got: {err}");
    }

    #[test]
    fn resolve_with_no_providers_errors() {
        let err = resolve_model_provider("model-a", &[], lookup(&Lists::new())).unwrap_err();
        assert!(err.contains("no provider available"), "got: {err}");
    }

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
    fn codex_models_parse_their_windows() {
        let body: CodexModelsResponse = serde_json::from_value(serde_json::json!({
            "models": [{"slug": "model-a", "context_window": 272_000}, {"slug": "model-b"}],
        }))
        .unwrap();
        assert_eq!(body.models[0].context_window, Some(272_000));
        assert_eq!(body.models[1].context_window, None);
    }
}
