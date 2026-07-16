//! Provider auto-discovery and credential resolution.
//!
//! A famous provider whose conventional key variable
//! (field `.2` of [`ProviderKind::info`]) holds a usable value is *available*. At
//! startup every available provider's key is read from its variable into
//! the in-memory [`CredentialStore`], and every key variable that fed a
//! credential — or was present but malformed — is then scrubbed from the
//! process environment. The scrub keeps a child a tool call spawns from
//! inheriting a live key and, because the variable is gone, forecloses
//! re-reading it later, which is why resolution is eager. The sweep covers
//! every [`ProviderKind`], with provider knowledge sourced once from
//! [`ProviderKind::info`].
//!
//! Custom providers declared in `config.ral` ([`crate::config`]) flow through
//! the same sweep: each is keyed by its [`ProviderId`] and its declared key
//! env var is read and scrubbed exactly like a famous provider's, so a custom
//! endpoint becomes available the moment its key is in the environment.
//!
//! Signed-in `ChatGPT` accounts ([`crate::provider::oauth`]) are the one source that does
//! *not* come from the environment: each persisted login becomes its own
//! [`ProviderId::ChatGpt`] bound to an [`Credential::OAuth`], loaded from the
//! token store. Several can be available at once, and an account is a distinct
//! identity from the API-key `OpenAI` provider — so a login and an
//! `OPENAI_API_KEY` coexist as separate selectable providers.

use crate::provider::{ChatGptAccount, CustomProvider, ProviderId, ProviderKind};
use clap::ValueEnum;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// A resolved credential — what a provider's requests authenticate with.
///
/// An [`ApiKey`](Self::ApiKey) is the bearer string read from a provider's
/// key variable. An [`OAuth`](Self::OAuth) credential is a `ChatGPT` plan
/// login held behind a shared cell: a turn reads the current access token
/// through it, and a refresh (in [`crate::provider`]) writes a renewed token
/// back into the same cell, so a session that outlives the access token keeps
/// authenticating without rebuilding the provider.
#[derive(Clone)]
pub enum Credential {
    /// An API key read from the environment at startup.
    ApiKey(String),
    /// A signed-in `ChatGPT` account's plan login, shared so a mid-session
    /// refresh is visible to the request path.
    OAuth(Arc<Mutex<crate::provider::oauth::OAuthToken>>),
}

impl Credential {
    /// Whether this credential is a `ChatGPT` plan login (OAuth) rather than
    /// an API key — narrower than "is a flat subscription", since a flat rate
    /// can also be a `ProviderId` property (opencode Go) on an ordinary key.
    pub fn is_subscription(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }
}

/// The in-memory store of resolved credentials, keyed by [`ProviderId`].
/// Built once at startup; a turn draws exactly one credential from it.
pub struct CredentialStore {
    ready: BTreeMap<ProviderId, Credential>,
    /// The famous providers, in [`ProviderKind`] declaration order, then the
    /// signed-in `ChatGPT` accounts (by label), then the custom providers in
    /// config order — the order [`Self::available`] preserves. Holds every
    /// known provider, available or not, so iteration order is stable
    /// regardless of which keys happen to be set.
    all: Vec<ProviderId>,
}

impl CredentialStore {
    /// Sweep every known provider — the famous [`ProviderKind`]s and the
    /// `custom` providers from `config.ral` — reading each one's conventional
    /// key variable and resolving a usable value into the store. Then scrub
    /// every key variable that was *present* — whether or not it yielded a
    /// usable key — from the environment. Scrubbing the set-but-malformed
    /// variable too (a key with a pasted newline, say) matters: leaving it set
    /// would let a child a tool call spawns inherit the live secret.
    ///
    /// SAFETY: the caller must invoke this while the process is still
    /// single-threaded (before any session worker thread is created), so
    /// the env scrub cannot race another thread.
    pub fn resolve_and_scrub(custom: Vec<CustomProvider>) -> Self {
        // Each signed-in ChatGPT account is its own provider identity, loaded
        // from the OAuth token store rather than swept from the environment.
        // Sorted by label so the picker lists accounts deterministically.
        let mut accounts: Vec<(ProviderId, crate::provider::oauth::OAuthToken)> =
            crate::provider::oauth::load_all()
                .into_iter()
                .map(|tok| {
                    (
                        ProviderId::ChatGpt(Arc::new(ChatGptAccount::from_token(&tok))),
                        tok,
                    )
                })
                .collect();
        accounts.sort_by(|(a, _), (b, _)| a.label().cmp(b.label()));

        // Famous providers, then the signed-in ChatGPT accounts, then the
        // custom providers — the order `available()` preserves.
        let all: Vec<ProviderId> = ProviderKind::value_variants()
            .iter()
            .copied()
            .map(ProviderId::Famous)
            .chain(accounts.iter().map(|(id, _)| id.clone()))
            .chain(custom.into_iter().map(|c| ProviderId::Custom(Arc::new(c))))
            .collect();

        let mut ready = BTreeMap::new();
        let mut scrub: Vec<String> = Vec::new();

        for id in &all {
            // A ChatGPT account has no key env var (`key_env()` is `None`); it
            // is resolved from the token store below, not the environment.
            let Some(var) = id.key_env() else {
                continue;
            };
            // Read the var once. A present var — valid or malformed — is
            // scrubbed so a child a tool call spawns cannot inherit a live
            // secret; only a valid key becomes an available credential.
            match read_env_key(var) {
                EnvKey::Absent => {}
                EnvKey::Malformed => scrub.push(var.to_string()),
                EnvKey::Valid(key) => {
                    scrub.push(var.to_string());
                    ready.insert(id.clone(), Credential::ApiKey(key));
                }
            }
        }

        scrub.sort_unstable();
        scrub.dedup();
        // SAFETY: startup is single-threaded here — see the doc comment.
        #[allow(clippy::disallowed_methods)]
        for var in &scrub {
            unsafe {
                std::env::remove_var(var);
            }
        }

        // Bind each ChatGPT account to its OAuth credential. An account is a
        // distinct provider identity from the API-key OpenAI provider, so a
        // stored login and an `OPENAI_API_KEY` coexist as separate selectable
        // providers rather than one superseding the other.
        for (id, token) in accounts {
            ready.insert(id, Credential::OAuth(Arc::new(Mutex::new(token))));
        }

        Self { ready, all }
    }

    /// The credential for an available provider, or `None` when its key was
    /// absent or malformed.
    pub fn get(&self, id: &ProviderId) -> Option<&Credential> {
        self.ready.get(id)
    }

    /// Whether `id`'s credential is in the store.
    pub fn is_available(&self, id: &ProviderId) -> bool {
        self.ready.contains_key(id)
    }

    /// Whether `id`'s bound credential is a `ChatGPT` plan login (OAuth)
    /// rather than an API key. `false` for an absent or metered provider.
    pub fn is_subscription(&self, id: &ProviderId) -> bool {
        self.get(id).is_some_and(Credential::is_subscription)
    }

    /// The available providers, in declaration order: famous first, then the
    /// signed-in `ChatGPT` accounts (by label), then the custom providers.
    pub fn available(&self) -> Vec<ProviderId> {
        self.all
            .iter()
            .filter(|id| self.ready.contains_key(id))
            .cloned()
            .collect()
    }
}

/// The state of a provider's key environment variable, from a single read.
enum EnvKey {
    /// The variable is unset — nothing to bind and nothing to scrub.
    Absent,
    /// Present but unusable: empty / whitespace-only, or carrying a control
    /// character (a stray newline pasted into the value) — it would fail at
    /// the provider, so it binds no credential, but it is still a live secret
    /// in the environment and must be scrubbed.
    Malformed,
    /// A usable key (trimmed, non-empty, no control characters).
    Valid(String),
}

/// Classify a provider's key environment variable in a single read. A control
/// character (a pasted newline) means the value cannot be a key.
fn read_env_key(var: &str) -> EnvKey {
    #[allow(clippy::disallowed_methods)]
    let Ok(raw) = std::env::var(var) else {
        return EnvKey::Absent;
    };
    let key = raw.trim().to_string();
    if key.is_empty() || key.bytes().any(|b| b < 0x20 || b == 0x7f) {
        EnvKey::Malformed
    } else {
        EnvKey::Valid(key)
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    /// A famous provider's id — the common case in these tests.
    fn fam(kind: ProviderKind) -> ProviderId {
        ProviderId::Famous(kind)
    }

    /// `resolve_and_scrub` is process-global (it mutates the real
    /// environment), so each scenario uses a guard that snapshots every
    /// provider key var, sets the scenario's values, runs the body, and
    /// restores. A process-wide lock serialises env-mutating tests under
    /// `RUST_TEST_THREADS > 1` so they cannot interleave their mutations.
    ///
    /// `resolve_and_scrub` also reads the persisted `ChatGPT` login from the
    /// XDG state dir, so the guard points `XDG_STATE_HOME` at a fresh empty
    /// temp dir: a scenario that does not set up a login then sees no
    /// credential there, isolated from any real `oauth.json` on the
    /// developer's machine. A scenario that exercises the login overrides
    /// `XDG_STATE_HOME` with its own value in `values`, which wins.
    fn with_env(values: &[(&str, Option<&str>)], body: impl FnOnce()) {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SERIAL: Mutex<()> = Mutex::new(());
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // A fresh empty state dir per call, so `oauth::load_all` finds no
        // tokens unless the scenario sets one up under its own XDG_STATE_HOME.
        let state_dir = std::env::temp_dir().join(format!(
            "exarch-cred-env-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&state_dir);
        // Snapshot every provider key var and every var the scenario sets, so
        // a scenario that touches a non-provider var (e.g. XDG_STATE_HOME)
        // restores it too rather than leaking into the next test.
        let mut names: Vec<String> = ProviderKind::value_variants()
            .iter()
            .map(|k| k.info().2.to_string())
            .collect();
        names.push("XDG_STATE_HOME".to_string());
        names.extend(values.iter().map(|(k, _)| (*k).to_string()));
        names.sort_unstable();
        names.dedup();
        let saved: Vec<(String, Option<String>)> = names
            .iter()
            .map(|v| {
                #[allow(clippy::disallowed_methods)]
                (v.clone(), std::env::var(v).ok())
            })
            .collect();
        #[allow(clippy::disallowed_methods)]
        unsafe {
            for v in ProviderKind::value_variants().iter().map(|k| k.info().2) {
                std::env::remove_var(v);
            }
            std::env::set_var("XDG_STATE_HOME", &state_dir);
            for (k, val) in values {
                match val {
                    Some(s) => std::env::set_var(k, s),
                    None => std::env::remove_var(k),
                }
            }
        }
        body();
        #[allow(clippy::disallowed_methods)]
        unsafe {
            for (k, val) in saved {
                match val {
                    Some(s) => std::env::set_var(&k, s),
                    None => std::env::remove_var(&k),
                }
            }
        }
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// A provider whose key var holds a usable value resolves into the
    /// store, its key is trimmed, and the var is scrubbed; a provider with
    /// no key var is not available.
    #[test]
    fn available_provider_resolves_trimmed_and_scrubs() {
        with_env(
            &[
                ("ANTHROPIC_API_KEY", Some("  sk-secret  ")),
                ("OPENAI_API_KEY", None),
                ("OPENROUTER_API_KEY", None),
                ("DEEPSEEK_API_KEY", None),
            ],
            || {
                let store = CredentialStore::resolve_and_scrub(Vec::new());
                match store.get(&fam(ProviderKind::Anthropic)) {
                    Some(Credential::ApiKey(k)) => assert_eq!(k, "sk-secret", "key is trimmed"),
                    _ => panic!("anthropic should resolve to an ApiKey"),
                }
                #[allow(clippy::disallowed_methods)]
                {
                    assert!(
                        std::env::var("ANTHROPIC_API_KEY").is_err(),
                        "a resolved provider's key var must be scrubbed"
                    );
                }
                assert!(!store.is_available(&fam(ProviderKind::Openai)));
                assert_eq!(store.available(), vec![fam(ProviderKind::Anthropic)]);
            },
        );
    }

    /// A set-but-malformed key (a pasted newline) does not become available,
    /// and — critically — is still scrubbed so the live secret cannot leak
    /// to a child a tool call spawns.
    #[test]
    fn malformed_key_is_scrubbed_and_unavailable() {
        with_env(
            &[
                ("ANTHROPIC_API_KEY", None),
                ("OPENAI_API_KEY", Some("sk-secret\nwith-newline")),
                ("OPENROUTER_API_KEY", None),
                ("DEEPSEEK_API_KEY", None),
            ],
            || {
                let store = CredentialStore::resolve_and_scrub(Vec::new());
                assert!(!store.is_available(&fam(ProviderKind::Openai)));
                #[allow(clippy::disallowed_methods)]
                {
                    assert!(
                        std::env::var("OPENAI_API_KEY").is_err(),
                        "a malformed-but-present key var must still be scrubbed"
                    );
                }
            },
        );
    }

    /// Several available providers all resolve, in declaration order.
    #[test]
    fn multiple_available_providers_in_declaration_order() {
        with_env(
            &[
                ("ANTHROPIC_API_KEY", Some("a")),
                ("OPENAI_API_KEY", None),
                ("OPENROUTER_API_KEY", Some("o")),
                ("DEEPSEEK_API_KEY", Some("d")),
                ("OPENCODE_API_KEY", None),
            ],
            || {
                let store = CredentialStore::resolve_and_scrub(Vec::new());
                assert_eq!(
                    store.available(),
                    vec![
                        fam(ProviderKind::Anthropic),
                        fam(ProviderKind::Openrouter),
                        fam(ProviderKind::Deepseek)
                    ]
                );
            },
        );
    }

    /// The two opencode providers share one `OPENCODE_API_KEY`: setting it
    /// makes both opencode-zen and opencode-go available off the single key,
    /// each resolving to the same trimmed bearer, and the one shared var is
    /// scrubbed (deduped) afterwards. They follow the other famous providers
    /// in declaration order.
    #[test]
    fn shared_opencode_key_makes_both_zen_and_go_available() {
        with_env(
            &[
                ("ANTHROPIC_API_KEY", None),
                ("OPENAI_API_KEY", None),
                ("OPENROUTER_API_KEY", None),
                ("DEEPSEEK_API_KEY", None),
                ("OPENCODE_API_KEY", Some("  oc-secret  ")),
            ],
            || {
                let store = CredentialStore::resolve_and_scrub(Vec::new());
                for kind in [ProviderKind::OpencodeZen, ProviderKind::OpencodeGo] {
                    match store.get(&fam(kind)) {
                        Some(Credential::ApiKey(k)) => assert_eq!(k, "oc-secret", "key is trimmed"),
                        _ => panic!("{kind:?} should resolve off the shared OPENCODE_API_KEY"),
                    }
                }
                #[allow(clippy::disallowed_methods)]
                {
                    assert!(
                        std::env::var("OPENCODE_API_KEY").is_err(),
                        "the shared opencode key var must be scrubbed"
                    );
                }
                assert_eq!(
                    store.available(),
                    vec![
                        fam(ProviderKind::OpencodeZen),
                        fam(ProviderKind::OpencodeGo)
                    ]
                );
            },
        );
    }

    /// opencode Go is a flat-rate subscription (unmetered) while opencode Zen
    /// on the same gateway and key is pay-as-you-go (metered). The split is a
    /// `ProviderId` property, independent of the credential — both authenticate
    /// off the same API key.
    #[test]
    fn opencode_go_is_flat_rate_zen_is_metered() {
        assert!(
            ProviderId::Famous(ProviderKind::OpencodeGo).flat_rate(),
            "opencode-go is a flat subscription"
        );
        assert!(
            !ProviderId::Famous(ProviderKind::OpencodeZen).flat_rate(),
            "opencode-zen is metered"
        );
        assert!(
            !ProviderId::Famous(ProviderKind::Openai).flat_rate(),
            "an API-key OpenAI provider is metered; its plan rides OAuth, not flat_rate"
        );
    }

    /// xAI and Qwen are ordinary metered API-key providers, each resolving
    /// off its own conventional key var (`XAI_API_KEY`, `DASHSCOPE_API_KEY`)
    /// and neither flat-rate — they bill per token like the other API-key
    /// providers.
    #[test]
    fn xai_and_qwen_resolve_as_metered_api_key_providers() {
        with_env(
            &[
                ("ANTHROPIC_API_KEY", None),
                ("OPENAI_API_KEY", None),
                ("OPENROUTER_API_KEY", None),
                ("DEEPSEEK_API_KEY", None),
                ("OPENCODE_API_KEY", None),
                ("XAI_API_KEY", Some("x-secret")),
                ("DASHSCOPE_API_KEY", Some("q-secret")),
            ],
            || {
                let store = CredentialStore::resolve_and_scrub(Vec::new());
                for kind in [ProviderKind::Xai, ProviderKind::Qwen] {
                    assert!(
                        matches!(store.get(&fam(kind)), Some(Credential::ApiKey(_))),
                        "{kind:?} should resolve to an ApiKey credential"
                    );
                    assert!(
                        !ProviderId::Famous(kind).flat_rate(),
                        "{kind:?} bills per token, not flat-rate"
                    );
                }
                assert_eq!(
                    store.available(),
                    vec![fam(ProviderKind::Xai), fam(ProviderKind::Qwen)]
                );
            },
        );
    }

    /// A signed-in `ChatGPT` account, keyed by its login email.
    fn account(account_id: &str, email: &str) -> ProviderId {
        ProviderId::ChatGpt(std::sync::Arc::new(ChatGptAccount {
            account_id: account_id.into(),
            label: email.into(),
        }))
    }

    /// A stored `ChatGPT` login resolves to its own OAuth-backed provider
    /// identity, keyed by the account's email — distinct from the API-key
    /// `OpenAI` provider, so a present `OPENAI_API_KEY` *also* resolves (as an
    /// `ApiKey`) and the two coexist. The key var is still scrubbed.
    #[test]
    fn oauth_login_is_a_distinct_account_coexisting_with_openai_key() {
        let dir = std::env::temp_dir().join(format!("exarch-cred-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        with_env(
            &[
                ("XDG_STATE_HOME", Some(dir.to_str().unwrap())),
                ("ANTHROPIC_API_KEY", None),
                ("OPENAI_API_KEY", Some("sk-env")),
                ("OPENROUTER_API_KEY", None),
                ("DEEPSEEK_API_KEY", None),
            ],
            || {
                crate::provider::oauth::save_one(&crate::provider::oauth::OAuthToken {
                    access_token: "at".into(),
                    refresh_token: "rt".into(),
                    account_id: "acc".into(),
                    email: Some("alex@work".into()),
                    expires_at: u64::MAX,
                })
                .expect("save token");
                let store = CredentialStore::resolve_and_scrub(Vec::new());
                assert!(
                    matches!(
                        store.get(&account("acc", "alex@work")),
                        Some(Credential::OAuth(_))
                    ),
                    "the login resolves to its own OAuth-backed account"
                );
                assert!(
                    matches!(
                        store.get(&fam(ProviderKind::Openai)),
                        Some(Credential::ApiKey(k)) if k == "sk-env"
                    ),
                    "OPENAI_API_KEY still resolves the API-key OpenAI provider alongside the login"
                );
                #[allow(clippy::disallowed_methods)]
                {
                    assert!(
                        std::env::var("OPENAI_API_KEY").is_err(),
                        "the key var must still be scrubbed"
                    );
                }
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Several signed-in `ChatGPT` accounts are each available as their own
    /// provider, ordered by label and placed after the famous providers (here
    /// anthropic) and before any custom one.
    #[test]
    fn multiple_chatgpt_accounts_each_available_sorted_by_label() {
        let dir = std::env::temp_dir().join(format!("exarch-cred-multi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        with_env(
            &[
                ("XDG_STATE_HOME", Some(dir.to_str().unwrap())),
                ("ANTHROPIC_API_KEY", Some("a")),
                ("OPENAI_API_KEY", None),
                ("OPENROUTER_API_KEY", None),
                ("DEEPSEEK_API_KEY", None),
            ],
            || {
                // Saved out of label order; resolution sorts them.
                for (acc, email) in [("acc_w", "alex@work"), ("acc_p", "alex@home")] {
                    crate::provider::oauth::save_one(&crate::provider::oauth::OAuthToken {
                        access_token: "at".into(),
                        refresh_token: "rt".into(),
                        account_id: acc.into(),
                        email: Some(email.into()),
                        expires_at: u64::MAX,
                    })
                    .expect("save token");
                }
                let store = CredentialStore::resolve_and_scrub(Vec::new());
                assert_eq!(
                    store.available(),
                    vec![
                        fam(ProviderKind::Anthropic),
                        account("acc_p", "alex@home"),
                        account("acc_w", "alex@work"),
                    ],
                    "accounts follow the famous providers, sorted by label"
                );
                for acc in [account("acc_p", "alex@home"), account("acc_w", "alex@work")] {
                    assert!(matches!(store.get(&acc), Some(Credential::OAuth(_))));
                }
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A custom provider from `config.ral` is swept exactly like a famous one:
    /// its declared key env var is read into the store, scrubbed from the
    /// environment, and it appears in `available()` after the famous
    /// providers. An absent custom key leaves it unavailable while the famous
    /// providers still resolve — the config is additive, never a precondition.
    #[test]
    fn custom_provider_resolves_and_scrubs_its_key() {
        let custom = CustomProvider {
            label: "local-llama".into(),
            key_env: "LOCAL_LLAMA_KEY".into(),
            endpoint: "https://llama.example/v1/".into(),
            adapter: genai::adapter::AdapterKind::OpenAI,
        };
        with_env(
            &[
                ("ANTHROPIC_API_KEY", Some("a")),
                ("OPENAI_API_KEY", None),
                ("OPENROUTER_API_KEY", None),
                ("DEEPSEEK_API_KEY", None),
                ("LOCAL_LLAMA_KEY", Some("  llama-secret  ")),
            ],
            || {
                let store = CredentialStore::resolve_and_scrub(vec![custom.clone()]);
                let id = ProviderId::Custom(std::sync::Arc::new(custom.clone()));
                match store.get(&id) {
                    Some(Credential::ApiKey(k)) => assert_eq!(k, "llama-secret"),
                    _ => panic!("custom provider should resolve to a trimmed ApiKey"),
                }
                #[allow(clippy::disallowed_methods)]
                {
                    assert!(
                        std::env::var("LOCAL_LLAMA_KEY").is_err(),
                        "a custom provider's key var must be scrubbed too"
                    );
                }
                // Famous first, then the custom provider.
                assert_eq!(store.available(), vec![fam(ProviderKind::Anthropic), id]);
            },
        );
    }
}
