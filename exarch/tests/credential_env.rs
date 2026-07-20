#![allow(clippy::disallowed_methods)]

//! [`CredentialStore::resolve_and_scrub`] scenarios, in their own test
//! binary.
//!
//! `resolve_and_scrub` is process-global — it reads the real environment and
//! scrubs every key var from it — so these scenarios must mutate
//! `XDG_STATE_HOME` and the provider key vars for real. Inside the library's
//! test binary that mutation raced every parallel test that *reads* the
//! environment (`policy::base` resolving `xdg:state`, most visibly): a
//! reader cannot know to take the writers' lock. Here the process is the
//! isolation boundary — the scenarios still serialise among themselves
//! through [`with_env`], and no bystander shares their environment.

use exarch::provider::credential::{Credential, CredentialStore, NO_AUTH_PLACEHOLDER};
use exarch::provider::oauth::{self, OAuthToken};
use exarch::provider::{ChatGptAccount, CustomProvider, ProviderId, ProviderKind};

// Mirror the binary's pre-`main` re-exec dispatch — helper re-exec dispatch,
// then the OS-sandbox stage — before libtest sees the flags either would
// reject; see [`exarch::dispatch_pre_main`].
exarch::pre_main_ctor!();

/// A famous provider's id — the common case in these tests.
fn fam(kind: ProviderKind) -> ProviderId {
    ProviderId::Famous(kind)
}

/// A signed-in `ChatGPT` account, keyed by its login email.
fn account(account_id: &str, email: &str) -> ProviderId {
    ProviderId::ChatGpt(std::sync::Arc::new(ChatGptAccount {
        account_id: account_id.into(),
        label: email.into(),
    }))
}

/// `resolve_and_scrub` is process-global (it mutates the real environment),
/// so each scenario uses a guard that snapshots every provider key var, sets
/// the scenario's values, runs the body, and restores. A process-wide lock
/// serialises the scenarios under `RUST_TEST_THREADS > 1` so they cannot
/// interleave their mutations.
///
/// `resolve_and_scrub` also reads the persisted `ChatGPT` login from the
/// XDG state dir, so the guard points `XDG_STATE_HOME` at a fresh empty
/// temp dir: a scenario that does not set up a login then sees no
/// credential there, isolated from any real `oauth.json` on the
/// developer's machine. A scenario that exercises the login overrides
/// `XDG_STATE_HOME` with its own value in `values`, which wins.
fn with_env(values: &[(&str, Option<&str>)], body: impl FnOnce()) {
    use clap::ValueEnum;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SERIAL: Mutex<()> = Mutex::new(());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        .map(|v| (v.clone(), std::env::var(v).ok()))
        .collect();
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
            assert!(
                std::env::var("ANTHROPIC_API_KEY").is_err(),
                "a resolved provider's key var must be scrubbed"
            );
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
            assert!(
                std::env::var("OPENAI_API_KEY").is_err(),
                "a malformed-but-present key var must still be scrubbed"
            );
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
            assert!(
                std::env::var("OPENCODE_API_KEY").is_err(),
                "the shared opencode key var must be scrubbed"
            );
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
            oauth::save_one(&OAuthToken {
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
            assert!(
                std::env::var("OPENAI_API_KEY").is_err(),
                "the key var must still be scrubbed"
            );
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
                oauth::save_one(&OAuthToken {
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
        key_env: Some("LOCAL_LLAMA_KEY".into()),
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
            assert!(
                std::env::var("LOCAL_LLAMA_KEY").is_err(),
                "a custom provider's key var must be scrubbed too"
            );
            // Famous first, then the custom provider.
            assert_eq!(store.available(), vec![fam(ProviderKind::Anthropic), id]);
        },
    );
}

/// A custom provider declared with no `key` (a no-auth local endpoint like
/// Ollama) is available with no env var set at all, resolving to the inert
/// [`NO_AUTH_PLACEHOLDER`] bearer rather than a real credential. Nothing is
/// read from or scrubbed from the environment on its behalf.
#[test]
fn keyless_custom_provider_resolves_to_placeholder() {
    let custom = CustomProvider {
        label: "ollama".into(),
        key_env: None,
        endpoint: "http://localhost:11434/v1/".into(),
        adapter: genai::adapter::AdapterKind::OpenAI,
    };
    with_env(
        &[
            ("ANTHROPIC_API_KEY", None),
            ("OPENAI_API_KEY", None),
            ("OPENROUTER_API_KEY", None),
            ("DEEPSEEK_API_KEY", None),
        ],
        || {
            let store = CredentialStore::resolve_and_scrub(vec![custom.clone()]);
            let id = ProviderId::Custom(std::sync::Arc::new(custom.clone()));
            match store.get(&id) {
                Some(Credential::ApiKey(k)) => assert_eq!(k, NO_AUTH_PLACEHOLDER),
                _ => panic!("a keyless custom provider should resolve to the placeholder"),
            }
            assert_eq!(store.available(), vec![id]);
        },
    );
}
