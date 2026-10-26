//! Live model lists, cached with a TTL, behind a network seam.
//!
//! An account's model list is that account's to know: Anthropic, `DeepSeek` and
//! Gemini list through their own endpoints ([`native`]), other API-key services
//! through genai. Signed-in accounts use their service's own catalog. A listing
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
use crate::provider::identity::{self, Account, AccountId, Login};
use crate::provider::oauth;
use genai::Client;
use genai::resolver::{AuthData, Endpoint, ProviderConfig};
use native::Native;
use ral_core::own::OwnDir;
use ral_core::path::{Visibility, XdgKind};
use ral_core::sync::LockExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

mod native;

/// How long a cached model list stays fresh.
const TTL: Duration = Duration::from_hours(6);

const PLAN_MODELS_URL: &str = "https://api.openai.com/v1/models";

/// The cache's file in the app's cache directory.
const CACHE_FILE: &str = "models.json";

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

/// One upstream `OpenRouter` can serve a given model through — a tag of the
/// `/model` overlay's via row, distilled from the per-model `/endpoints`
/// listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderEndpoint {
    pub provider_name: String,
    /// The full endpoint tag `provider.order` takes, variant included, e.g.
    /// `deepinfra/fp4`.
    pub tag: String,
    pub context_length: Option<u64>,
    pub quantization: Option<String>,
}

/// Why an account's model list could not be had. Says nothing of which
/// account: the caller names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListFailure {
    UnknownAccount,
    NoCredential,
    Runtime(String),
    Request(String),
    Refresh(String),
    Http {
        status: reqwest::StatusCode,
        detail: String,
    },
    Parse(String),
}

impl std::fmt::Display for ListFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownAccount => f.write_str("not a known account"),
            Self::NoCredential => f.write_str("no resolved credential"),
            Self::Runtime(e) | Self::Request(e) => f.write_str(e),
            Self::Refresh(e) => write!(f, "could not refresh the login: {e}"),
            Self::Http { status, detail } => write!(f, "OpenAI returned HTTP {status}: {detail}"),
            Self::Parse(e) => write!(f, "could not parse the model list: {e}"),
        }
    }
}

/// The seam every fetch of an account's model list goes through: live against
/// genai and `OpenRouter`'s REST API, an in-memory fake in tests.
pub trait ModelSource {
    /// `account`'s full model list.
    ///
    /// # Errors
    /// Returns `Err` saying why the fetch failed, never which account.
    fn list(&self, account: &AccountId) -> Result<Vec<Listed>, ListFailure>;

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
    fn list(&self, id: &AccountId) -> Result<Vec<Listed>, ListFailure> {
        let account = self.roster.account(id).ok_or(ListFailure::UnknownAccount)?;
        let credential = self
            .roster
            .credential(id)
            .ok_or(ListFailure::NoCredential)?;
        match credential {
            Credential::ApiKey(key) => self.list_api_key(account, key),
            Credential::OAuth(cell) => self.list_login(account, cell),
        }
    }

    fn endpoints(&self, model: &str) -> Result<Vec<ProviderEndpoint>, String> {
        let url = format!("https://openrouter.ai/api/v1/models/{model}/endpoints");
        let key = self
            .roster
            .accounts()
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
    fn list_api_key(&self, account: &Account, key: &str) -> Result<Vec<Listed>, ListFailure> {
        let runtime = blocking_runtime("listing").map_err(ListFailure::Runtime)?;
        match Native::of(account.service.adapter) {
            Some(native) => runtime
                .block_on(native.list(account, key))
                .map_err(ListFailure::Request),
            None => runtime.block_on(self.list_genai(account, key)),
        }
    }

    /// Endpoint and key passed explicitly, so a catalog request never leans on
    /// the client's auth resolver. Both come from the account's service, so a
    /// declared endpoint lists exactly as a built-in one does.
    async fn list_genai(&self, account: &Account, key: &str) -> Result<Vec<Listed>, ListFailure> {
        let provider_config = ProviderConfig {
            endpoint: account.service.endpoint.clone().map(Endpoint::from_owned),
            auth: Some(AuthData::from_single(key.to_owned())),
        };
        self.client
            .all_model_names(account.service.adapter, provider_config)
            .await
            .map(|names| names.into_iter().map(Listed::bare).collect())
            .map_err(|e| ListFailure::Request(e.to_string()))
    }

    /// Catalog authentication uses the same credential owner as inference.
    fn list_login(
        &self,
        account: &Account,
        cell: &oauth::CredentialHandle,
    ) -> Result<Vec<Listed>, ListFailure> {
        let runtime = blocking_runtime("subscription model-list").map_err(ListFailure::Runtime)?;
        runtime.block_on(async {
            cell.prepare().await.map_err(ListFailure::Refresh)?;
            let token = cell.snapshot();
            match token.login() {
                Login::Chatgpt => {
                    plan_models(
                        crate::provider::tls::client()
                            .get(PLAN_MODELS_URL)
                            .bearer_auth(&token.access_token),
                    )
                    .await
                }
                Login::Codex => {
                    let request = token.request_headers("application/json").into_iter().fold(
                        crate::provider::tls::client().get(oauth::codex::models_url()),
                        |request, (key, value)| request.header(key, value),
                    );
                    plan_models(request).await
                }
                Login::Grok => self.list_genai(account, &token.access_token).await,
            }
        })
    }
}

async fn plan_models(request: reqwest::RequestBuilder) -> Result<Vec<Listed>, ListFailure> {
    let response = request
        .send()
        .await
        .map_err(|e| ListFailure::Request(e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(ListFailure::Http {
            status,
            detail: body_detail(&body),
        });
    }
    let body: PlanModels = response
        .json()
        .await
        .map_err(|e| ListFailure::Parse(e.to_string()))?;
    Ok(body.listed())
}

#[derive(Deserialize)]
struct PlanModels {
    models: Vec<PlanModel>,
}

#[derive(Deserialize)]
struct PlanModel {
    slug: String,
    /// `"list"` is what the picker may show; absent reads as listed.
    #[serde(default = "listed")]
    visibility: String,
    #[serde(default)]
    context_window: Option<u64>,
}

fn listed() -> String {
    "list".to_string()
}

impl PlanModels {
    /// The pickable models, in server order.
    fn listed(self) -> Vec<Listed> {
        self.models
            .into_iter()
            .filter(|model| model.visibility == "list")
            .map(|model| Listed {
                id: model.slug,
                context_window: model.context_window.filter(|&n| n > 0),
            })
            .collect()
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
    /// `provider/variant`, e.g. `"deepinfra/fp4"`.
    #[serde(default)]
    tag: String,
    context_length: Option<u64>,
    quantization: Option<String>,
}

impl ProviderEndpoint {
    fn from_wire(wire: EndpointWire) -> Self {
        Self {
            provider_name: wire.provider_name,
            tag: wire.tag,
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
    /// `None` disables the disk cache; `Some` is the app's XDG cache
    /// directory, which holds [`CACHE_FILE`].
    cache: Option<OwnDir>,
    memo: BTreeMap<AccountId, Vec<Listed>>,
    /// Keyed by `OpenRouter` model id. Memo-only: serving-provider availability
    /// is volatile and the fetch cheap, so it is refetched, never persisted.
    endpoints_memo: BTreeMap<String, Vec<ProviderEndpoint>>,
}

impl<S: ModelSource> ModelCatalog<S> {
    /// A catalog persisting to `app`'s XDG cache directory, when one resolves.
    pub fn new(source: S, app: crate::app::App) -> Self {
        Self {
            source,
            cache: cache_dir(app),
            memo: BTreeMap::new(),
            endpoints_memo: BTreeMap::new(),
        }
    }

    /// A catalog with the memo alone — for tests, and any caller that must
    /// never touch a user's cache dir.
    pub fn memo_only(source: S) -> Self {
        Self {
            source,
            cache: None,
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
        let file = read_cache(self.cache.as_ref()?)?;
        let entry = file.providers.get(account.as_str())?;
        let age = crate::app::now_secs().saturating_sub(entry.fetched_at);
        (age < TTL.as_secs()).then(|| entry.models.clone())
    }

    /// Best-effort: a cache the process cannot write is not fatal, since the
    /// memo still serves the session.
    fn write_disk(&self, account: &AccountId, models: &[Listed]) {
        let Some(cache) = self.cache.as_ref() else {
            return;
        };
        let mut file = read_cache(cache).unwrap_or_default();
        file.providers.insert(
            account.as_str().to_string(),
            CacheEntry {
                fetched_at: crate::app::now_secs(),
                models: models.to_vec(),
            },
        );
        let _ = cache.store(CACHE_FILE, &file, Visibility::Public);
    }
}

impl ModelCatalog<LiveSource> {
    /// List with `roster`'s credentials from now on.
    pub fn set_roster(&mut self, roster: Roster) {
        self.source.roster = roster;
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
) -> Result<Vec<Listed>, ListFailure> {
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

/// `None` when no cache directory can be had (no base resolves, or the walk
/// to it fails) — the catalog then runs memo-only.
fn cache_dir(app: crate::app::App) -> Option<OwnDir> {
    app.own(XdgKind::Cache).ok()
}

fn read_cache(cache: &OwnDir) -> Option<CacheFile> {
    // A cache: an unreadable or corrupt file reads as empty, and the next
    // listing writes it afresh.
    cache.load(CACHE_FILE).ok().flatten()
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
/// among credentials is the user's to make, as `remedy` says how.
pub fn resolve_model_provider(
    name: &str,
    available: &[Account],
    mut listing: impl FnMut(&Account) -> Result<Vec<Listed>, ListFailure>,
    remedy: &str,
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
            "model '{name}' is listed by more than one available account ({}): {remedy}",
            many.iter()
                .map(|account| label(account))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Resolve an explicit `--provider` name: an account id, then a label as
/// `exarch accounts` prints it, then a service name, then a handle.
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
    if let Some(account) = available
        .iter()
        .find(|account| identity::label(account, available) == name)
    {
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
            "provider '{name}' is not available; pass one of these exactly as shown: {}; \
             or set its API key",
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
#[allow(clippy::disallowed_methods, reason = "test code")]
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
        fn list(&self, account: &AccountId) -> Result<Vec<Listed>, ListFailure> {
            self.calls.set(self.calls.get() + 1);
            self.lists
                .get(account)
                .cloned()
                .unwrap_or_else(|| Err(ListFailure::Request("no fake list".into())))
        }

        fn endpoints(&self, model: &str) -> Result<Vec<ProviderEndpoint>, String> {
            self.endpoints
                .get(model)
                .cloned()
                .unwrap_or_else(|| Err("no fake endpoints".into()))
        }
    }

    type Lists = BTreeMap<AccountId, Result<Vec<Listed>, ListFailure>>;

    fn bare(models: &[&str]) -> Vec<Listed> {
        models.iter().map(|m| Listed::bare(*m)).collect()
    }

    /// A listing lookup over `lists`, as `resolve_model_provider` takes one.
    fn lookup(lists: &Lists) -> impl FnMut(&Account) -> Result<Vec<Listed>, ListFailure> + '_ {
        |account| {
            lists
                .get(&account.id)
                .cloned()
                .unwrap_or_else(|| Err(ListFailure::Request("unreachable".into())))
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
        let lists = Lists::from([(
            deepseek.id.clone(),
            Err(ListFailure::Request("network down".into())),
        )]);
        let catalog = Mutex::new(ModelCatalog::memo_only(FakeSource::new(lists)));
        assert_eq!(
            listing_of(&catalog, &deepseek.id),
            Err(ListFailure::Request("network down".into()))
        );
        assert_eq!(
            catalog.lock_ignore_poison().cached_listing(&deepseek.id),
            None
        );
    }

    #[test]
    fn an_endpoint_keeps_its_full_tag() {
        let with_variant = ProviderEndpoint::from_wire(EndpointWire {
            provider_name: "DeepInfra".into(),
            tag: "deepinfra/fp4".into(),
            context_length: Some(163_840),
            quantization: Some("fp4".into()),
        });
        assert_eq!(with_variant.tag, "deepinfra/fp4");
        let bare = ProviderEndpoint::from_wire(EndpointWire {
            provider_name: "StreamLake".into(),
            tag: "streamlake".into(),
            context_length: Some(128_000),
            quantization: None,
        });
        assert_eq!(bare.tag, "streamlake");
    }

    #[test]
    fn endpoints_memo_round_trips() {
        let model = "vendor/model-a";
        let mut endpoints = BTreeMap::new();
        endpoints.insert(
            model.to_string(),
            Ok(vec![ProviderEndpoint {
                provider_name: "DeepInfra".into(),
                tag: "deepinfra".into(),
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
            resolve_model_provider(
                "model-b",
                &available,
                lookup(&lists),
                "pass --provider to say which"
            ),
            Ok(llama)
        );
    }

    #[test]
    fn resolve_two_accounts_listing_one_model_is_refused_naming_both() {
        let personal = Account::login(Login::Chatgpt, "acc-1", "alex@bristol.ac.uk");
        let work = Account::login(Login::Chatgpt, "acc-2", "alex@work (Acme Ltd)");
        let lists = Lists::from([
            (personal.id.clone(), Ok(bare(&["model-a"]))),
            (work.id.clone(), Ok(bare(&["model-a"]))),
        ]);
        let available = [personal, work];
        let err = resolve_model_provider(
            "model-a",
            &available,
            lookup(&lists),
            "pass --provider to say which",
        )
        .unwrap_err();
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
        let err = resolve_model_provider(
            "vendor/model-b",
            &available,
            lookup(&lists),
            "pass --provider to say which",
        )
        .unwrap_err();
        assert!(err.contains("not listed"), "got: {err}");
    }

    #[test]
    fn resolve_names_the_accounts_it_could_not_list() {
        let anthropic = Account::built_in("anthropic");
        let deepseek = Account::built_in("deepseek");
        let lists = Lists::from([
            (anthropic.id.clone(), Ok(bare(&["model-a"]))),
            (
                deepseek.id.clone(),
                Err(ListFailure::Request("network down".into())),
            ),
        ]);
        let available = [anthropic, deepseek];
        let err = resolve_model_provider(
            "model-b",
            &available,
            lookup(&lists),
            "pass --provider to say which",
        )
        .unwrap_err();
        assert!(err.contains("deepseek (network down)"), "got: {err}");
    }

    #[test]
    fn resolve_with_no_providers_errors() {
        let err = resolve_model_provider("model-a", &[], lookup(&Lists::new()), "").unwrap_err();
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

    /// A bare login service naming two accounts is refused, naming both,
    /// rather than picking whichever the store happened to list first.
    #[test]
    fn pin_a_service_name_naming_two_accounts_is_refused() {
        for login in Login::ALL {
            let personal = Account::login(login, "acc-1", "alex@bristol.ac.uk");
            let work = Account::login(login, "acc-2", "alex@work");
            let available = [personal, work];
            let err =
                resolve_pinned_provider(login.service().name.as_str(), &available).unwrap_err();
            assert!(err.contains("account id"), "{err}");
            assert!(
                err.contains("alex@bristol.ac.uk") && err.contains("alex@work"),
                "{err}"
            );
        }
    }

    /// Two accounts sharing an email: the label `exarch accounts` prints picks
    /// one; the bare handle stays ambiguous.
    #[test]
    fn pin_a_full_label_resolves_exactly() {
        let a = Account::login(Login::Chatgpt, "acc-1", "alex@bristol.ac.uk");
        let b = Account::login(Login::Chatgpt, "acc-2", "alex@bristol.ac.uk");
        let available = [a, b];
        let label = identity::label(&available[1], &available);
        let got = resolve_pinned_provider(&label, &available).unwrap();
        assert_eq!(got.id, available[1].id);
        let err = resolve_pinned_provider("alex@bristol.ac.uk", &available).unwrap_err();
        assert!(err.contains("names 2 signed-in accounts"), "{err}");
    }

    #[test]
    fn plan_models_keep_only_the_listed() {
        let body: PlanModels = serde_json::from_value(serde_json::json!({
            "models": [
                {"slug": "model-a", "visibility": "list", "context_window": 272_000},
                {"slug": "model-b"},
                {"slug": "model-c", "visibility": "hidden", "context_window": 8_000},
                {"slug": "model-zero", "context_window": 0},
            ],
        }))
        .unwrap();
        let ids_and_windows: Vec<_> = body
            .listed()
            .into_iter()
            .map(|m| (m.id, m.context_window))
            .collect();
        assert_eq!(
            ids_and_windows,
            [
                ("model-a".to_string(), Some(272_000)),
                ("model-b".to_string(), None),
                ("model-zero".to_string(), None),
            ]
        );
    }

    #[test]
    fn login_catalogs_are_cached_by_full_account_identity() {
        let mut lists = Lists::new();
        let mut expected = Vec::new();
        for login in Login::ALL {
            for issued in ["a", "b"] {
                let account = Account::login(login, issued, "same@work");
                let window = u64::try_from(expected.len()).unwrap() + 1;
                let models = vec![Listed {
                    id: "shared-model".into(),
                    context_window: Some(window),
                }];
                lists.insert(account.id.clone(), Ok(models.clone()));
                expected.push((account.id, models));
            }
        }
        let source = FakeSource::new(lists);
        let calls = Rc::clone(&source.calls);
        let catalog = Mutex::new(ModelCatalog::memo_only(source));
        for _ in 0..2 {
            for (id, models) in &expected {
                assert_eq!(listing_of(&catalog, id), Ok(models.clone()));
            }
        }
        assert_eq!(calls.get(), 6, "each account keeps its own cached catalog");
    }

    fn catalog_token(login: Login) -> oauth::OAuthToken {
        oauth::OAuthToken {
            access_token: "catalog-access".into(),
            refresh_token: "catalog-refresh".into(),
            issued: "catalog-subject".into(),
            email: Some("same@work".into()),
            expires_at: u64::MAX,
            grant: match login {
                Login::Chatgpt => oauth::Grant::Chatgpt {
                    id_token: "id".into(),
                    client_id: "client".into(),
                },
                Login::Codex => oauth::Grant::Codex {
                    account_id: "raw-backend-id".into(),
                    workspace: None,
                    plan: None,
                },
                Login::Grok => oauth::Grant::Grok,
            },
        }
    }

    fn local_model_server(status: u16, body: &str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/", listener.local_addr().unwrap());
        let body = body.to_owned();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
            }
            write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (endpoint, server)
    }

    fn local_model_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn both_openai_catalogs_use_the_same_visibility_and_window_parser() {
        for (login, body) in [
            (
                Login::Chatgpt,
                serde_json::json!({"models": [
                    {"slug":"shown", "visibility":"list", "context_window":128_000},
                    {"slug":"default"},
                    {"slug":"zero", "context_window":0},
                    {"slug":"hidden", "visibility":"hidden"},
                ]}),
            ),
            (
                Login::Codex,
                serde_json::json!({"models": [
                    {"slug":"shown", "visibility":"list", "context_window":128_000, "display_name":"Shown"},
                    {"slug":"default", "supported_reasoning_levels":[]},
                    {"slug":"zero", "context_window":0},
                    {"slug":"hidden", "visibility":"hide", "context_window":400_000},
                ]}),
            ),
        ] {
            let (endpoint, server) = local_model_server(200, &body.to_string());
            let token = catalog_token(login);
            let request = token.request_headers("application/json").into_iter().fold(
                local_model_client().get(format!("{endpoint}models")),
                |request, (key, value)| request.header(key, value),
            );
            assert_eq!(
                plan_models(request).await.unwrap(),
                vec![
                    Listed {
                        id: "shown".into(),
                        context_window: Some(128_000)
                    },
                    Listed::bare("default"),
                    Listed::bare("zero"),
                ]
            );
            let request = server.join().unwrap().to_ascii_lowercase();
            assert!(request.starts_with("get /v1/models http/1.1\r\n"));
            assert!(request.contains("authorization: bearer catalog-access\r\n"));
            assert!(request.contains("accept: application/json\r\n"));
            assert_eq!(
                request.contains("chatgpt-account-id: raw-backend-id\r\n"),
                login == Login::Codex
            );
        }
    }

    #[tokio::test]
    async fn openai_catalog_http_and_parse_failures_keep_their_classification() {
        let (endpoint, server) =
            local_model_server(403, r#"{"error":{"message":"catalog refused"}}"#);
        let failure = plan_models(local_model_client().get(endpoint))
            .await
            .unwrap_err();
        server.join().unwrap();
        assert!(matches!(failure, ListFailure::Http { status, detail }
            if status == reqwest::StatusCode::FORBIDDEN && detail.contains("catalog refused")));

        let (endpoint, server) = local_model_server(200, "not-json");
        let failure = plan_models(local_model_client().get(endpoint))
            .await
            .unwrap_err();
        server.join().unwrap();
        assert!(matches!(failure, ListFailure::Parse(_)));
    }

    #[test]
    fn grok_catalog_uses_xai_in_the_same_listing_runtime() {
        for status in [200, 401] {
            let body = if status == 200 {
                r#"{"data":[{"id":"grok-model"}]}"#
            } else {
                r#"{"error":{"message":"refused"}}"#
            };
            let (endpoint, server) = local_model_server(status, body);
            let token = catalog_token(Login::Grok);
            let state = ral_core::own::OwnDir::for_test("catalog").unwrap();
            let cell = oauth::commit_for_test(&state, &token);
            let mut account = oauth::to_account(&token);
            account.service.endpoint = Some(endpoint);
            let source = LiveSource {
                roster: Roster::default(),
                client: Client::builder()
                    .with_reqwest(local_model_client())
                    .build()
                    .unwrap(),
            };
            let listing = source.list_login(&account, &cell);
            if status == 200 {
                assert_eq!(listing.unwrap(), vec![Listed::bare("grok-model")]);
            } else {
                assert!(matches!(listing, Err(ListFailure::Request(_))));
            }
            let request = server.join().unwrap().to_ascii_lowercase();
            assert!(request.starts_with("get /v1/models http/1.1\r\n"));
            assert!(request.contains("authorization: bearer catalog-access\r\n"));
            assert!(!request.contains("chatgpt-account-id"));
        }
    }

    #[test]
    fn old_chatgpt_account_ids_do_not_pin_a_renamed_login() {
        let account = Account::login(Login::Chatgpt, "same-id", "same@work");
        let failure = resolve_pinned_provider("chatgpt:same-id", &[account]).unwrap_err();
        assert!(failure.contains("not available"), "{failure}");
    }
}
