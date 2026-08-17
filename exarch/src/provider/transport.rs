//! Credential binding and the per-process transport cache.

use super::credential::Credential;
use super::identity::{Account, AccountId, Billing, Service, adapter_for_model};
use super::oauth;
use crate::sync::LockExt;
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};
use genai::{Client, Headers, ModelIden, ServiceTarget};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// One [`Transport`] per account, credential, and wire adapter, not per
/// model — only API-key `OpenAI` splits, where the model picks the adapter.
/// Two `ChatGPT` accounts share a service, hence an endpoint and an adapter,
/// but never a token cell, so the key must name the account and not just the
/// service.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TransportKey {
    account: AccountId,
    fingerprint: CredFingerprint,
    adapter: AdapterKind,
}

impl TransportKey {
    fn for_selection(account: &Account, model: &str, credential: &Credential) -> Self {
        Self {
            account: account.id.clone(),
            fingerprint: CredFingerprint::of(credential),
            adapter: adapter_for_model(&account.service, model),
        }
    }
}

/// An API key is fingerprinted by content, so a rotated key misses the cache and
/// rebuilds. OAuth collapses to one variant: the transport holds the token cell,
/// not a token, so a refresh never needs a new transport.
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
    billing: Billing,
}

impl Transport {
    fn build(service: &Service, model: &str, credential: &Credential) -> Arc<Self> {
        let token_cell = match credential {
            Credential::OAuth(cell) => Some(cell.clone()),
            Credential::ApiKey(_) => None,
        };
        let (client, adapter) = build_client(service, model, credential);
        Arc::new(Self {
            client,
            adapter,
            token_cell,
            billing: service.billing,
        })
    }

    pub(super) fn client(&self) -> &Client {
        &self.client
    }

    pub(super) fn adapter(&self) -> AdapterKind {
        self.adapter
    }

    /// The sole authority on whether this transport's turns cost money —
    /// `service.billing`, read once at build time, and nothing else.
    pub(super) fn metered(&self) -> bool {
        self.billing == Billing::Metered
    }
}

/// The shared async runtime and credential-keyed transport cache.
pub struct Engine {
    runtime: tokio::runtime::Runtime,
    /// Sent as the provider's prompt-cache key on every request, so this run's
    /// requests share one cache lineage instead of colliding with another
    /// process's.
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
        account: &Account,
        model: &str,
        credential: &Credential,
    ) -> Arc<Transport> {
        self.transports
            .lock_ignore_poison()
            .entry(TransportKey::for_selection(account, model, credential))
            .or_insert_with(|| Transport::build(&account.service, model, credential))
            .clone()
    }

    /// Called first by `complete` and `summarize`; a failed refresh only logs, so
    /// the stale token rides the request rather than a hiccup killing the turn.
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

fn build_client(service: &Service, model: &str, credential: &Credential) -> (Client, AdapterKind) {
    let key = match credential {
        Credential::ApiKey(key) => key.clone(),
        Credential::OAuth(cell) => {
            return (build_oauth_client(cell.clone()), AdapterKind::OpenAIResp);
        }
    };
    let adapter = adapter_for_model(service, model);
    // An explicit endpoint needs a service-target resolver to repoint genai at
    // it; otherwise genai knows the default, so auth keyed on the adapter is all.
    let client = if let Some(base_url) = service.endpoint.clone() {
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

/// The auth resolver reads the cell on every request, so a refresh by
/// [`Engine::refresh_if_stale`] reaches the next call without rebuilding the client.
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

/// Fill the pricing cache up front so the first usage lookup never pays the fetch.
fn prime_pricing(runtime: &tokio::runtime::Runtime) {
    runtime.block_on(super::pricing::ensure_loaded());
}

#[cfg(test)]
mod tests {
    use super::super::identity::{ServiceName, built_in, chatgpt_service};
    use super::*;

    fn token(access_token: &str) -> oauth::OAuthToken {
        oauth::OAuthToken {
            access_token: access_token.into(),
            refresh_token: format!("refresh-{access_token}"),
            issued: "account".into(),
            email: Some("me@example.com".into()),
            workspace: None,
            plan: None,
            expires_at: u64::MAX,
        }
    }

    fn service(name: &str) -> Service {
        built_in(&ServiceName::declared(name).unwrap()).unwrap()
    }

    fn login(issued: &str) -> Account {
        let service = chatgpt_service();
        Account {
            id: AccountId::of_login(&service.name, issued),
            service,
            handle: "me@example.com".into(),
        }
    }

    #[test]
    fn transport_key_separates_rotated_api_keys() {
        let account = Account::of_service(service("anthropic"));
        let key = |secret: &str| {
            TransportKey::for_selection(
                &account,
                "claude-opus-4",
                &Credential::ApiKey(secret.into()),
            )
        };
        assert_eq!(key("sk-original"), key("sk-original"));
        assert_ne!(key("sk-original"), key("sk-rotated"));
    }

    #[test]
    fn oauth_keys_ignore_rotated_secret() {
        let chat = login("account");
        let credential = |secret: &str| Credential::OAuth(Arc::new(Mutex::new(token(secret))));
        assert_eq!(
            TransportKey::for_selection(&chat, "gpt-5.5", &credential("first")),
            TransportKey::for_selection(&chat, "gpt-5.5", &credential("second")),
        );
    }

    /// `metered` decides whether a turn is priced at all, so the flavours are
    /// checked where they are decided and again where the user reads them.
    #[test]
    fn subscription_turns_report_tokens_but_never_a_cost() {
        let flat_rate = Transport::build(
            &service("opencode-go"),
            "glm-5.2",
            &Credential::ApiKey("k".into()),
        );
        let keyed = Transport::build(
            &service("anthropic"),
            "claude-opus-4",
            &Credential::ApiKey("k".into()),
        );
        let chatgpt = Transport::build(
            &chatgpt_service(),
            "gpt-5.5",
            &Credential::OAuth(Arc::new(Mutex::new(token("live")))),
        );
        assert!(!flat_rate.metered());
        assert!(keyed.metered());
        assert!(!chatgpt.metered());

        let raw = genai::chat::Usage {
            prompt_tokens: Some(1_000),
            completion_tokens: Some(50),
            ..Default::default()
        };
        let usage = |transport: &Transport, model: &str| {
            super::super::usage::usage_from(model, &raw, transport.metered(), transport.adapter())
        };
        for (transport, model) in [(&flat_rate, "glm-5.2"), (&chatgpt, "gpt-5.5")] {
            let usage = usage(transport, model);
            assert!(usage.unmetered);
            assert_eq!(usage.parts().cost, "subscription");
            assert_eq!(usage.parts().input, "1000");
        }
        let keyed = usage(&keyed, "claude-opus-4");
        assert!(!keyed.unmetered);
        assert_ne!(keyed.parts().cost, "subscription");
        assert_eq!(keyed.parts().input, "1000");
    }
}
