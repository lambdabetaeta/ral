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
