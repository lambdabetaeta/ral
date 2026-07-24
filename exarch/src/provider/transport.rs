//! Credential binding and the per-process transport cache.

use super::credential::Credential;
use super::identity::{ProviderId, Subscription, adapter_for_provider_model};
use super::oauth;
use crate::sync::LockExt;
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};
use genai::{Client, Headers, ModelIden, ServiceTarget};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Cache key for [`Engine::transport_for`]: one [`Transport`] per distinct
/// provider label + credential + wire adapter, not per model. Two models
/// under the same provider that resolve to the same [`AdapterKind`] share a
/// client; the API-key `OpenAI` provider is the exception, where the model
/// itself picks between the `OpenAI` and `OpenAIResp` adapters
/// ([`adapter_for_provider_model`]), so those still split.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TransportKey {
    provider: String,
    fingerprint: CredFingerprint,
    adapter: AdapterKind,
}

impl TransportKey {
    fn for_selection(id: &ProviderId, model: &str, credential: &Credential) -> Self {
        Self {
            provider: id.label().to_string(),
            fingerprint: CredFingerprint::of(credential),
            adapter: adapter_for_provider_model(id, model),
        }
    }
}

/// A credential's cache identity. An API key is fingerprinted by content, so
/// a rotated key misses the cache and rebuilds a fresh [`Transport`]. An
/// OAuth credential collapses to one variant regardless of the current
/// token: the `Transport` holds the shared cell
/// ([`Transport::token_cell`]), not a token value, so a mid-session refresh
/// mutates that cell in place and never needs a new transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum CredFingerprint {
    ApiKey(u64),
    OAuth,
}

impl CredFingerprint {
    fn of(credential: &Credential) -> Self {
        match credential {
            Credential::ApiKey(key) => {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                key.hash(&mut hasher);
                Self::ApiKey(hasher.finish())
            }
            Credential::OAuth(_) => Self::OAuth,
        }
    }
}

/// A genai client bound to one credential and wire adapter.
pub(super) struct Transport {
    client: Client,
    adapter: AdapterKind,
    /// Present only for an OAuth-backed transport; a refresh
    /// ([`Engine::refresh_if_stale`]) mutates the token in place through
    /// this shared cell, so the `Transport` itself never needs rebuilding
    /// when the token rotates.
    token_cell: Option<Arc<Mutex<oauth::OAuthToken>>>,
    flat_rate: bool,
}

impl Transport {
    fn build(id: &ProviderId, model: &str, credential: &Credential) -> Arc<Self> {
        let token_cell = match credential {
            Credential::OAuth(cell) => Some(cell.clone()),
            Credential::ApiKey(_) => None,
        };
        let (client, adapter) = build_client(id, model, credential);
        Arc::new(Self {
            client,
            adapter,
            token_cell,
            flat_rate: id.flat_rate(),
        })
    }

    pub(super) fn client(&self) -> &Client {
        &self.client
    }

    pub(super) fn adapter(&self) -> AdapterKind {
        self.adapter
    }

    pub(super) fn metered(&self) -> bool {
        self.token_cell.is_none() && !self.flat_rate
    }

    pub(super) fn subscription(&self) -> Subscription {
        if self.token_cell.is_some() {
            Subscription::ChatGpt
        } else if self.flat_rate {
            Subscription::FlatRate
        } else {
            Subscription::Metered
        }
    }
}

/// The shared async runtime and credential-keyed transport cache.
pub struct Engine {
    runtime: tokio::runtime::Runtime,
    /// A process-unique id threaded into every request as the provider's
    /// prompt-cache key ([`super::request::complete_options`]), so this
    /// run's requests land on one cache lineage rather than colliding with
    /// a concurrent or prior process's.
    cache_key: String,
    transports: Mutex<HashMap<TransportKey, Arc<Transport>>>,
}

impl Engine {
    /// Build the process engine and prime pricing.
    pub fn new() -> Arc<Self> {
        let runtime = make_runtime();
        prime_pricing(&runtime);
        Arc::new(Self {
            runtime,
            cache_key: fresh_cache_key(),
            transports: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn transport_for(
        &self,
        id: &ProviderId,
        model: &str,
        credential: &Credential,
    ) -> Arc<Transport> {
        self.transports
            .lock_ignore_poison()
            .entry(TransportKey::for_selection(id, model, credential))
            .or_insert_with(|| Transport::build(id, model, credential))
            .clone()
    }

    /// Best-effort refresh before every live request ([`super::stream`]'s
    /// `complete`/`summarize` call this first). A no-op for an API-key
    /// transport; for an OAuth transport a failed refresh only logs — the
    /// stale token still rides the request, which then fails on its own
    /// terms rather than this call blocking the turn on a refresh hiccup.
    pub(super) fn refresh_if_stale(&self, transport: &Transport) {
        let Some(cell) = &transport.token_cell else {
            return;
        };
        if let Err(error) = self.runtime.block_on(oauth::refresh_cell_if_stale(cell)) {
            eprintln!("exarch: ChatGPT token refresh failed: {error}");
        }
    }

    pub(super) fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    pub(super) fn cache_key(&self) -> &str {
        &self.cache_key
    }
}

fn fresh_cache_key() -> String {
    format!("exarch-{}", std::process::id())
}

fn make_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio multi-thread runtime")
}

fn build_client(id: &ProviderId, model: &str, credential: &Credential) -> (Client, AdapterKind) {
    let key = match credential {
        Credential::ApiKey(key) => key.clone(),
        Credential::OAuth(cell) => {
            return (build_oauth_client(cell.clone()), AdapterKind::OpenAIResp);
        }
    };
    let adapter = adapter_for_provider_model(id, model);
    // A custom/OpenAI-compatible endpoint needs a service-target resolver to
    // repoint genai's request at `base_url`; a famous provider's default
    // endpoint is already known to genai, so an auth resolver keyed on the
    // adapter is enough.
    let client = if let Some(base_url) = id.endpoint() {
        let endpoint = Endpoint::from_owned(base_url);
        let resolver = ServiceTargetResolver::from_resolver_fn(move |target: ServiceTarget| {
            Ok(ServiceTarget {
                endpoint: endpoint.clone(),
                auth: AuthData::from_single(key.clone()),
                model: ModelIden::new(adapter, target.model.model_name),
            })
        });
        Client::builder()
            .with_reqwest(super::tls::client())
            .with_service_target_resolver(resolver)
            .build()
    } else {
        let auth = AuthResolver::from_resolver_fn(move |identity: ModelIden| {
            if identity.adapter_kind == adapter {
                Ok(Some(AuthData::from_single(key.clone())))
            } else {
                Ok(None)
            }
        });
        Client::builder()
            .with_reqwest(super::tls::client())
            .with_adapter_kind(adapter)
            .with_auth_resolver(auth)
            .build()
    };
    (client, adapter)
}

/// A client whose auth resolver reads the token cell fresh on every request
/// — so [`Engine::refresh_if_stale`]'s in-place update is visible to the
/// very next call with no client rebuild.
fn build_oauth_client(cell: Arc<Mutex<oauth::OAuthToken>>) -> Client {
    let auth = AuthResolver::from_resolver_fn(move |identity: ModelIden| {
        if identity.adapter_kind == AdapterKind::OpenAIResp {
            let token = cell.lock_ignore_poison();
            Ok(Some(AuthData::RequestOverride {
                url: oauth::RESPONSES_URL.to_string(),
                headers: Headers::from(oauth::request_headers(&token, "text/event-stream")),
            }))
        } else {
            Ok(None)
        }
    });
    Client::builder()
        .with_reqwest(super::tls::client())
        .with_adapter_kind(AdapterKind::OpenAIResp)
        .with_auth_resolver(auth)
        .build()
}

/// Populate the pricing/caps cache before any turn needs it, so the first
/// [`super::usage::usage_from`] lookup never pays the catalog fetch.
fn prime_pricing(runtime: &tokio::runtime::Runtime) {
    runtime.block_on(super::pricing::ensure_loaded());
}

#[cfg(test)]
mod tests {
    use super::super::identity::ProviderKind;
    use super::*;

    fn token(access_token: &str) -> oauth::OAuthToken {
        oauth::OAuthToken {
            access_token: access_token.into(),
            refresh_token: format!("refresh-{access_token}"),
            account_id: "account".into(),
            email: Some("me@example.com".into()),
            expires_at: u64::MAX,
        }
    }

    #[test]
    fn transport_key_separates_rotated_api_keys() {
        let id = ProviderId::Famous(ProviderKind::Anthropic);
        let key = |secret: &str| {
            TransportKey::for_selection(&id, "claude-opus-4", &Credential::ApiKey(secret.into()))
        };
        assert_eq!(key("sk-original"), key("sk-original"));
        assert_ne!(key("sk-original"), key("sk-rotated"));
    }

    #[test]
    fn oauth_keys_ignore_rotated_secret() {
        let chat = ProviderId::ChatGpt(Arc::new(super::super::identity::ChatGptAccount {
            account_id: "account".into(),
            label: "me@example.com".into(),
        }));
        let credential = |secret: &str| Credential::OAuth(Arc::new(Mutex::new(token(secret))));
        assert_eq!(
            TransportKey::for_selection(&chat, "gpt-5.5", &credential("first")),
            TransportKey::for_selection(&chat, "gpt-5.5", &credential("second")),
        );
    }
}
