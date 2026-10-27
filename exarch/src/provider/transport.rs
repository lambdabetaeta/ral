//! Credential binding and the per-process transport cache.

use super::credential::Credential;
use super::identity::{Account, AccountId, Auth, Billing, Login, Service, adapter_for_model};
use super::oauth;
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use ral_core::sync::LockExt;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// One [`Transport`] per account, credential, and wire adapter, not per
/// model — only API-key `OpenAI` splits, where the model picks the adapter.
/// Two signed-in accounts share a service, hence an endpoint and an adapter,
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
    credential: Option<oauth::CredentialHandle>,
    billing: Billing,
    live_search: bool,
}

impl Transport {
    fn build(service: &Service, model: &str, credential: &Credential) -> Arc<Self> {
        let (client, adapter) = build_client(service, model, credential);
        Arc::new(Self {
            client,
            adapter,
            credential: match credential {
                Credential::OAuth(cell) => Some(cell.clone()),
                Credential::ApiKey(_) => None,
            },
            billing: service.billing,
            live_search: matches!(service.auth, Auth::OAuth(Login::Codex)),
        })
    }

    pub(super) async fn prepare(
        &self,
        cancel: &crate::cancel::Token,
    ) -> Result<(), super::ProviderError> {
        use super::error::CancelSite;
        let cancelled = || super::ProviderError::Cancelled(CancelSite::BeforeRequest);
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let Some(credential) = &self.credential else {
            return Ok(());
        };
        tokio::select! {
            biased;
            () = super::retry::wait_for_cancel(cancel) => Err(cancelled()),
            result = credential.prepare() => result.map_err(super::ProviderError::Other),
        }
    }

    pub(super) fn client(&self) -> &Client {
        &self.client
    }

    pub(super) fn adapter(&self) -> AdapterKind {
        self.adapter
    }

    pub(super) fn live_search(&self) -> bool {
        self.live_search
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
            return (
                build_oauth_client(service.adapter, cell.clone()),
                service.adapter,
            );
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
            .expect("with_reqwest supplies the client, so build() cannot fail")
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
            .expect("with_reqwest supplies the client, so build() cannot fail")
    };
    (client, adapter)
}

/// Read current authentication for every request, without rebuilding the client.
fn build_oauth_client(adapter: AdapterKind, cell: oauth::CredentialHandle) -> Client {
    Client::builder()
        .with_reqwest(super::tls::client())
        .with_adapter_kind(adapter)
        .with_auth_resolver(oauth_auth_resolver(adapter, cell))
        .build()
        .expect("with_reqwest supplies the client, so build() cannot fail")
}

fn oauth_auth_resolver(adapter: AdapterKind, cell: oauth::CredentialHandle) -> AuthResolver {
    AuthResolver::from_resolver_fn(move |identity: ModelIden| {
        if identity.adapter_kind == adapter && !cell.is_retired() {
            Ok(Some(cell.snapshot().auth_data()))
        } else {
            Ok(None)
        }
    })
}

/// Fill the pricing cache up front so the first usage lookup never pays the fetch.
fn prime_pricing(runtime: &tokio::runtime::Runtime) {
    runtime.block_on(super::pricing::ensure_loaded());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(login: Login, access_token: &str) -> oauth::OAuthToken {
        let grant = match login {
            Login::Chatgpt => oauth::Grant::Chatgpt {
                id_token: "id".into(),
                client_id: "oaiapp_test".into(),
            },
            Login::Codex => oauth::Grant::Codex {
                account_id: "raw-backend-id".into(),
                workspace: None,
                plan: None,
            },
            Login::Grok => oauth::Grant::Grok,
        };
        oauth::OAuthToken {
            access_token: access_token.into(),
            refresh_token: format!("refresh-{access_token}"),
            issued: "local-composite-id".into(),
            email: Some("me@example.com".into()),
            expires_at: u64::MAX,
            grant,
        }
    }

    #[test]
    fn transport_key_separates_rotated_api_keys() {
        let account = Account::built_in("anthropic");
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
    fn oauth_keys_ignore_rotated_secret_but_not_account_or_service() {
        let mut keys = Vec::new();
        for login in Login::ALL {
            let account = Account::login(login, "account", "me@example.com");
            let state = ral_core::own::OwnDir::for_test("transport-key").unwrap();
            let credential = |secret: &str| {
                Credential::OAuth(oauth::commit_for_test(&state, &token(login, secret)))
            };
            let first = TransportKey::for_selection(&account, "m", &credential("first"));
            assert_eq!(
                first,
                TransportKey::for_selection(&account, "m", &credential("second"))
            );
            let sibling = Account::login(login, "other-account", "me@example.com");
            assert_ne!(
                first,
                TransportKey::for_selection(&sibling, "m", &credential("first"))
            );
            keys.push(first);
        }
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[0], keys[2]);
        assert_ne!(keys[1], keys[2]);
    }

    #[test]
    fn oauth_resolver_reads_the_current_cell_and_refuses_other_adapters() {
        for login in Login::ALL {
            let adapter = login.service().adapter;
            let state = ral_core::own::OwnDir::for_test("auth-resolver").unwrap();
            let cell = oauth::commit_for_test(&state, &token(login, "old"));
            let AuthResolver::ResolverFn(resolver) = oauth_auth_resolver(adapter, cell.clone())
            else {
                panic!("OAuth uses a synchronous cell resolver");
            };
            let resolve = |kind| resolver.exec_fn(ModelIden::new(kind, "m")).unwrap();
            for secret in ["old", "new"] {
                oauth::commit_for_test(&state, &token(login, secret));
                match (login, resolve(adapter).unwrap()) {
                    (Login::Chatgpt | Login::Grok, AuthData::Key(key)) => assert_eq!(key, secret),
                    (Login::Codex, AuthData::RequestOverride { url, headers }) => {
                        assert_eq!(url, oauth::codex::RESPONSES_URL);
                        let headers: std::collections::BTreeMap<_, _> =
                            headers.into_iter().collect();
                        assert_eq!(
                            headers,
                            std::collections::BTreeMap::from([
                                ("authorization".into(), format!("Bearer {secret}")),
                                ("chatgpt-account-id".into(), "raw-backend-id".into()),
                                ("openai-beta".into(), "responses=experimental".into()),
                                ("originator".into(), "codex_cli_rs".into()),
                                (
                                    "user-agent".into(),
                                    format!(
                                        "codex_cli_rs/{}",
                                        oauth::codex::codex_client_version()
                                    )
                                ),
                                ("accept".into(), "text/event-stream".into()),
                            ]),
                        );
                    }
                    _ => panic!("the login's grant must choose its authentication shape"),
                }
                for other in [
                    AdapterKind::OpenAIResp,
                    AdapterKind::Xai,
                    AdapterKind::Anthropic,
                ] {
                    if other != adapter {
                        assert!(resolve(other).is_none());
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn cancellation_does_not_wait_for_the_store_lease() {
        use super::super::error::CancelSite;
        use crate::cancel;
        use ral_core::process::CancelCause;
        use std::time::Duration;
        let state = ral_core::own::OwnDir::for_test("cancel-prepare").unwrap();
        let store = (*state).clone();
        let handle = tokio::task::spawn_blocking(move || {
            oauth::commit_for_test(&store, &token(Login::Grok, "live"))
        })
        .await
        .unwrap();
        let transport = Transport::build(
            &Login::Grok.service(),
            "m",
            &Credential::OAuth(handle.clone()),
        );
        let lock = state.open_write("oauth.lock").unwrap();
        lock.try_lock().unwrap();
        let cancel = cancel::Token::new();
        let preparation = transport.prepare(&cancel);
        tokio::pin!(preparation);
        assert!(futures_util::poll!(&mut preparation).is_pending());
        cancel.cancel(CancelCause::Interrupted);
        let result = tokio::time::timeout(Duration::from_millis(200), preparation)
            .await
            .unwrap();
        assert!(matches!(
            result,
            Err(super::super::ProviderError::Cancelled(
                CancelSite::BeforeRequest
            ))
        ));
        assert!(matches!(
            transport.prepare(&cancel).await,
            Err(super::super::ProviderError::Cancelled(
                CancelSite::BeforeRequest
            ))
        ));
        drop(lock);
        tokio::time::timeout(Duration::from_secs(5), handle.prepare())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(handle.snapshot().access_token, "live");
    }

    #[test]
    fn login_transports_keep_the_service_adapter_and_only_codex_has_live_search() {
        for login in Login::ALL {
            let service = login.service();
            let transport = Transport::build(
                &service,
                "m",
                &Credential::OAuth(oauth::commit_for_test(
                    &ral_core::own::OwnDir::for_test("transport").unwrap(),
                    &token(login, "live"),
                )),
            );
            assert_eq!(transport.adapter(), service.adapter);
            assert_eq!(transport.live_search(), login == Login::Codex);
            assert!(!transport.metered());
        }
        let keyed = Transport::build(
            &Account::built_in("openai").service,
            "gpt-5.5",
            &Credential::ApiKey("k".into()),
        );
        assert!(!keyed.live_search());
    }

    /// `metered` decides whether a turn is priced at all, so the flavours are
    /// checked where they are decided and again where the user reads them.
    #[test]
    fn subscription_turns_report_tokens_but_never_a_cost() {
        let flat_rate = Transport::build(
            &Account::built_in("opencode-go").service,
            "glm-5.2",
            &Credential::ApiKey("k".into()),
        );
        let keyed = Transport::build(
            &Account::built_in("anthropic").service,
            "claude-opus-4",
            &Credential::ApiKey("k".into()),
        );
        let logins = Login::ALL.map(|login| {
            Transport::build(
                &login.service(),
                "m",
                &Credential::OAuth(oauth::commit_for_test(
                    &ral_core::own::OwnDir::for_test("transport").unwrap(),
                    &token(login, "live"),
                )),
            )
        });
        assert!(!flat_rate.metered());
        assert!(keyed.metered());

        let raw = genai::chat::Usage {
            prompt_tokens: Some(1_000),
            completion_tokens: Some(50),
            ..Default::default()
        };
        let usage = |transport: &Transport, model: &str| {
            super::super::usage::usage_from(model, &raw, transport.metered(), transport.adapter())
        };
        for transport in std::iter::once(&flat_rate).chain(logins.iter()) {
            let model = "gpt-5.5";
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
