#![allow(clippy::disallowed_methods)]

//! [`CredentialStore::resolve_and_scrub`] scenarios, in their own test
//! binary.
//!
//! `resolve_and_scrub` is process-global — it reads the real environment and
//! scrubs every key var from it — so these scenarios must mutate
//! `XDG_STATE_HOME` and the service key vars for real. Inside the library's
//! test binary that mutation raced every parallel test that *reads* the
//! environment (`policy::base` resolving `xdg:state`, most visibly): a
//! reader cannot know to take the writers' lock. Here the process is the
//! isolation boundary — the scenarios still serialise among themselves
//! through [`with_env`], and no bystander shares their environment.

use exarch::provider::credential::{Credential, CredentialStore, NO_AUTH_PLACEHOLDER};
use exarch::provider::identity::{
    Account, AccountId, Auth, Billing, Service, ServiceName, built_in_services, chatgpt_service,
};
use exarch::provider::oauth::{self, OAuthToken};

// Mirror the binary's pre-`main` re-exec dispatch — helper re-exec dispatch,
// then the OS-sandbox stage — before libtest sees the flags either would
// reject; see [`exarch::dispatch_pre_main`].
exarch::pre_main_ctor!();

/// A built-in, key-bearing account's id — the common case in these tests.
fn fam(name: &str) -> AccountId {
    AccountId::of_service(&ServiceName::declared(name).unwrap())
}

/// A `ChatGPT` login's account id, by its issued id.
fn login(issued: &str) -> AccountId {
    AccountId::of_login(&chatgpt_service().name, issued)
}

fn oauth_token(issued: &str, email: Option<&str>) -> OAuthToken {
    OAuthToken {
        access_token: "at".into(),
        refresh_token: "rt".into(),
        issued: issued.into(),
        email: email.map(str::to_string),
        workspace: None,
        plan: None,
        expires_at: u64::MAX,
    }
}

/// `resolve_and_scrub` is process-global (it mutates the real environment),
/// so each scenario uses a guard that snapshots every service key var, sets
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
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SERIAL: Mutex<()> = Mutex::new(());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A fresh empty state dir per call, so `oauth::accounts` finds no
    // tokens unless the scenario sets one up under its own XDG_STATE_HOME.
    let state_dir = std::env::temp_dir().join(format!(
        "exarch-cred-env-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&state_dir);
    // Snapshot every service key var and every var the scenario sets, so a
    // scenario that touches a non-service var (e.g. XDG_STATE_HOME) restores
    // it too rather than leaking into the next test.
    let mut names: Vec<String> = built_in_services()
        .into_iter()
        .filter_map(|service| match service.auth {
            Auth::Env(var) => Some(var),
            Auth::OAuth | Auth::Unnamed => None,
        })
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
        for name in &names {
            std::env::remove_var(name);
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

/// A service whose key var holds a usable value resolves into the
/// store, its key is trimmed, and the var is scrubbed; a service with
/// no key var is not available.
#[test]
fn available_service_resolves_trimmed_and_scrubs() {
    with_env(
        &[
            ("ANTHROPIC_API_KEY", Some("  sk-secret  ")),
            ("OPENAI_API_KEY", None),
            ("OPENROUTER_API_KEY", None),
            ("DEEPSEEK_API_KEY", None),
        ],
        || {
            let store = CredentialStore::resolve_and_scrub(Vec::new());
            match store.get(&fam("anthropic")) {
                Some(Credential::ApiKey(k)) => assert_eq!(k, "sk-secret", "key is trimmed"),
                _ => panic!("anthropic should resolve to an ApiKey"),
            }
            assert!(
                std::env::var("ANTHROPIC_API_KEY").is_err(),
                "a resolved service's key var must be scrubbed"
            );
            assert!(!store.is_available(&fam("openai")));
            assert_eq!(
                store
                    .available()
                    .into_iter()
                    .map(|a| a.id)
                    .collect::<Vec<_>>(),
                vec![fam("anthropic")]
            );
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
            assert!(!store.is_available(&fam("openai")));
            assert!(
                std::env::var("OPENAI_API_KEY").is_err(),
                "a malformed-but-present key var must still be scrubbed"
            );
        },
    );
}

/// Several available services all resolve, in declaration order.
#[test]
fn multiple_available_services_in_declaration_order() {
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
                store
                    .available()
                    .into_iter()
                    .map(|a| a.id)
                    .collect::<Vec<_>>(),
                vec![fam("anthropic"), fam("openrouter"), fam("deepseek")]
            );
        },
    );
}

/// The two opencode services share one `OPENCODE_API_KEY`: setting it
/// makes both opencode-zen and opencode-go available off the single key,
/// each resolving to the same trimmed bearer, and the one shared var is
/// scrubbed (deduped) afterwards. They follow the other built-in services
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
            for name in ["opencode-zen", "opencode-go"] {
                match store.get(&fam(name)) {
                    Some(Credential::ApiKey(k)) => assert_eq!(k, "oc-secret", "key is trimmed"),
                    _ => panic!("{name} should resolve off the shared OPENCODE_API_KEY"),
                }
            }
            assert!(
                std::env::var("OPENCODE_API_KEY").is_err(),
                "the shared opencode key var must be scrubbed"
            );
            assert_eq!(
                store
                    .available()
                    .into_iter()
                    .map(|a| a.id)
                    .collect::<Vec<_>>(),
                vec![fam("opencode-zen"), fam("opencode-go")]
            );
        },
    );
}

/// xAI and Qwen are ordinary metered API-key services, each resolving
/// off its own conventional key var (`XAI_API_KEY`, `DASHSCOPE_API_KEY`)
/// and neither flat-rate — they bill per token like the other API-key
/// services.
#[test]
fn xai_and_qwen_resolve_as_metered_api_key_services() {
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
            for name in ["xai", "qwen"] {
                assert!(
                    matches!(store.get(&fam(name)), Some(Credential::ApiKey(_))),
                    "{name} should resolve to an ApiKey credential"
                );
                let service = built_in_services()
                    .into_iter()
                    .find(|s| s.name.as_str() == name)
                    .unwrap();
                assert_eq!(service.billing, Billing::Metered, "{name} bills per token");
            }
            assert_eq!(
                store
                    .available()
                    .into_iter()
                    .map(|a| a.id)
                    .collect::<Vec<_>>(),
                vec![fam("xai"), fam("qwen")]
            );
        },
    );
}

/// A stored `ChatGPT` login resolves to its own OAuth-backed account,
/// distinct from the API-key `OpenAI` service, so a present
/// `OPENAI_API_KEY` *also* resolves (as an `ApiKey`) and the two coexist.
/// The key var is still scrubbed.
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
            oauth::save_one(&oauth_token("acc", Some("alex@work"))).expect("save token");
            let store = CredentialStore::resolve_and_scrub(Vec::new());
            assert!(
                matches!(store.get(&login("acc")), Some(Credential::OAuth(_))),
                "the login resolves to its own OAuth-backed account"
            );
            assert!(
                matches!(
                    store.get(&fam("openai")),
                    Some(Credential::ApiKey(k)) if k == "sk-env"
                ),
                "OPENAI_API_KEY still resolves the API-key OpenAI service alongside the login"
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
/// account, distinct by id and never shadowing one another however they
/// were saved.
#[test]
fn multiple_chatgpt_accounts_stay_distinct_and_available() {
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
            oauth::save_one(&oauth_token("acc_w", Some("alex@work"))).expect("save token");
            oauth::save_one(&oauth_token("acc_p", Some("alex@home"))).expect("save token");
            let store = CredentialStore::resolve_and_scrub(Vec::new());
            let available = store.available();
            let ids: Vec<AccountId> = available.iter().map(|a| a.id.clone()).collect();
            assert!(ids.contains(&fam("anthropic")));
            assert!(ids.contains(&login("acc_w")));
            assert!(ids.contains(&login("acc_p")));
            assert_eq!(ids.len(), 3, "neither login shadows the other");
            for account in [login("acc_p"), login("acc_w")] {
                assert!(matches!(store.get(&account), Some(Credential::OAuth(_))));
            }
        },
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A declared service from `config.ral` is swept exactly like a built-in
/// one: its declared key env var is read into the store, scrubbed from the
/// environment, and it appears in `available()` after the built-in
/// services. An absent declared key leaves it unavailable while the
/// built-in ones still resolve — the declaration is additive, never a
/// precondition.
#[test]
fn declared_service_resolves_and_scrubs_its_key() {
    let declared = Service {
        name: ServiceName::declared("local-llama").unwrap(),
        endpoint: Some("https://llama.example/v1/".into()),
        adapter: genai::adapter::AdapterKind::OpenAI,
        default_model: None,
        auth: Auth::Env("LOCAL_LLAMA_KEY".into()),
        billing: Billing::Metered,
        routes: false,
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
            let store = CredentialStore::resolve_and_scrub(vec![declared.clone()]);
            let id = AccountId::of_service(&declared.name);
            match store.get(&id) {
                Some(Credential::ApiKey(k)) => assert_eq!(k, "llama-secret"),
                _ => panic!("a declared service should resolve to a trimmed ApiKey"),
            }
            assert!(
                std::env::var("LOCAL_LLAMA_KEY").is_err(),
                "a declared service's key var must be scrubbed too"
            );
            assert_eq!(
                store
                    .available()
                    .into_iter()
                    .map(|a| a.id)
                    .collect::<Vec<_>>(),
                vec![fam("anthropic"), id]
            );
        },
    );
}

/// A declared service with no `key` (a no-auth local endpoint like Ollama)
/// is available with no env var set at all, resolving to the inert
/// [`NO_AUTH_PLACEHOLDER`] bearer rather than a real credential. Nothing is
/// read from or scrubbed from the environment on its behalf.
#[test]
fn keyless_declared_service_resolves_to_placeholder() {
    let declared = Service {
        name: ServiceName::declared("ollama").unwrap(),
        endpoint: Some("http://localhost:11434/v1/".into()),
        adapter: genai::adapter::AdapterKind::OpenAI,
        default_model: None,
        auth: Auth::Unnamed,
        billing: Billing::Metered,
        routes: false,
    };
    with_env(
        &[
            ("ANTHROPIC_API_KEY", None),
            ("OPENAI_API_KEY", None),
            ("OPENROUTER_API_KEY", None),
            ("DEEPSEEK_API_KEY", None),
        ],
        || {
            let store = CredentialStore::resolve_and_scrub(vec![declared.clone()]);
            let id = AccountId::of_service(&declared.name);
            match store.get(&id) {
                Some(Credential::ApiKey(k)) => assert_eq!(k, NO_AUTH_PLACEHOLDER),
                _ => panic!("a keyless declared service should resolve to the placeholder"),
            }
            assert_eq!(
                store
                    .available()
                    .into_iter()
                    .map(|a| a.id)
                    .collect::<Vec<_>>(),
                vec![id]
            );
        },
    );
}

/// The account itself, not only its id, is what `available()` hands back —
/// `Account::of_service` names both a key-bearing account's id and its
/// handle after the service's own name.
#[test]
fn a_key_bearing_account_names_itself_after_its_service() {
    with_env(
        &[
            ("ANTHROPIC_API_KEY", Some("a")),
            ("OPENAI_API_KEY", None),
            ("OPENROUTER_API_KEY", None),
            ("DEEPSEEK_API_KEY", None),
        ],
        || {
            let store = CredentialStore::resolve_and_scrub(Vec::new());
            let anthropic: Account = store
                .available()
                .into_iter()
                .find(|a| a.id == fam("anthropic"))
                .expect("anthropic is available");
            assert_eq!(anthropic.handle, "anthropic");
            assert_eq!(anthropic.id.as_str(), "anthropic");
        },
    );
}
