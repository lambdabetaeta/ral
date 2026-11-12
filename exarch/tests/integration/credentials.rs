//! [`CredentialStore::resolve_and_scrub`] scenarios.
//!
//! `resolve_and_scrub` is process-global — it reads the real environment and
//! scrubs every key var from it — so these scenarios must mutate
//! `XDG_STATE_HOME` and the service key vars for real. Each runs isolated in
//! its own process, since the mutation would race every parallel test that
//! *reads* the environment (`policy::base` resolving `xdg:state`, most visibly).

use exarch::provider::credential::{Credential, CredentialStore, NO_AUTH_PLACEHOLDER};
use exarch::provider::identity::{
    Account, AccountId, Auth, Login, Service, ServiceName, built_in_services,
};
use exarch::provider::oauth::{self, Grant, OAuthToken};

/// A built-in, key-bearing account's id — the common case in these tests.
fn fam(name: &str) -> AccountId {
    AccountId::of_service(&ServiceName::declared(name).unwrap())
}

/// A `ChatGPT` login's account id, by its issued id.
fn login(issued: &str) -> AccountId {
    AccountId::of_login(&Login::Chatgpt.service().name, issued)
}

fn oauth_token(issued: &str, email: Option<&str>) -> OAuthToken {
    OAuthToken {
        access_token: "at".into(),
        refresh_token: "rt".into(),
        grant: Grant::Chatgpt {
            id_token: "id".into(),
            client_id: "oaiapp_test".into(),
        },
        issued: issued.into(),
        email: email.map(str::to_string),
        expires_at: u64::MAX,
    }
}

/// `resolve_and_scrub` is process-global (it mutates the real environment),
/// so each scenario uses a guard that clears every service key var, sets the
/// scenario's values, and runs the body.
///
/// `resolve_and_scrub` also reads the persisted `ChatGPT` login from the
/// XDG state dir, so the guard points `XDG_STATE_HOME` at a fresh empty
/// temp dir: a scenario that does not set up a login then sees no
/// credential there, isolated from any real `oauth.json` on the
/// developer's machine. A scenario that exercises the login overrides
/// `XDG_STATE_HOME` with its own value in `values`, which wins.
fn with_env(values: &[(&str, Option<&str>)], body: impl FnOnce()) {
    let state_dir = tempfile::tempdir().unwrap();
    let mut names: Vec<String> = built_in_services()
        .into_iter()
        .filter_map(|service| match service.auth {
            Auth::Key(var) => Some(var),
            Auth::OAuth(_) | Auth::Keyless => None,
        })
        .collect();
    names.push("XDG_STATE_HOME".to_string());
    names.extend(values.iter().map(|(k, _)| (*k).to_string()));
    names.sort_unstable();
    names.dedup();
    unsafe {
        for name in &names {
            std::env::remove_var(name);
        }
        std::env::set_var("XDG_STATE_HOME", state_dir.path());
        for (k, val) in values {
            match val {
                Some(s) => std::env::set_var(k, s),
                None => std::env::remove_var(k),
            }
        }
    }
    body();
}

/// A service whose key var holds a usable value resolves into the
/// store, its key is trimmed, and the var is scrubbed; a service with
/// no key var is not available.
#[test]
fn available_service_resolves_trimmed_and_scrubs() {
    ral_core::isolated!(available_service_resolves_trimmed_and_scrubs, || {
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
    });
}

#[test]
fn meta_key_is_captured_and_scrubbed() {
    ral_core::isolated!(meta_key_is_captured_and_scrubbed, || {
        with_env(&[("META_API_KEY", Some("  meta-secret  "))], || {
            let store = CredentialStore::resolve_and_scrub(Vec::new());
            assert!(matches!(
                store.get(&fam("meta")),
                Some(Credential::ApiKey(key)) if key == "meta-secret"
            ));
            assert!(std::env::var("META_API_KEY").is_err());
            assert_eq!(
                store
                    .available()
                    .into_iter()
                    .map(|a| a.id)
                    .collect::<Vec<_>>(),
                vec![fam("meta")]
            );
        });
    });
}

/// A set-but-malformed key (a pasted newline) does not become available,
/// and — critically — is still scrubbed so the live secret cannot leak
/// to a child a tool call spawns.
#[test]
fn malformed_key_is_scrubbed_and_unavailable() {
    ral_core::isolated!(malformed_key_is_scrubbed_and_unavailable, || {
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
    });
}

/// The two opencode services share one `OPENCODE_API_KEY`: setting it
/// makes both opencode-zen and opencode-go available off the single key,
/// each resolving to the same trimmed bearer, and the one shared var is
/// scrubbed (deduped) afterwards. They follow the other built-in services
/// in declaration order.
#[test]
fn shared_opencode_key_makes_both_zen_and_go_available() {
    ral_core::isolated!(shared_opencode_key_makes_both_zen_and_go_available, || {
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
    });
}

/// A stored `ChatGPT` login resolves to its own OAuth-backed account,
/// distinct from the API-key `OpenAI` service, so a present
/// `OPENAI_API_KEY` *also* resolves (as an `ApiKey`) and the two coexist.
/// The key var is still scrubbed.
#[test]
fn oauth_login_is_a_distinct_account_coexisting_with_openai_key() {
    ral_core::isolated!(
        oauth_login_is_a_distinct_account_coexisting_with_openai_key,
        || {
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
    );
}

#[test]
fn all_login_routes_load_independent_cells_in_stable_order() {
    ral_core::isolated!(
        all_login_routes_load_independent_cells_in_stable_order,
        || {
            with_env(&[("ANTHROPIC_API_KEY", Some("a"))], || {
                let mut expected = Vec::new();
                for login in Login::ALL {
                    for issued in ["second", "first"] {
                        let mut token = oauth_token(issued, Some("same@example.com"));
                        token.grant = match login {
                            Login::Chatgpt => token.grant,
                            Login::Codex => Grant::Codex {
                                account_id: "workspace".into(),
                                workspace: None,
                                plan: None,
                            },
                            Login::Grok => Grant::Grok,
                        };
                        token.access_token = format!("{login:?}-{issued}");
                        assert!(!oauth::save_one(&token).unwrap());
                        expected.push((Account::login(login, issued, token.handle()).id, token));
                    }
                }
                expected.sort_by_key(|(id, token)| (token.login(), token.handle(), id.clone()));
                let mut store = CredentialStore::resolve_and_scrub(Vec::new());
                let ids: Vec<_> = store
                    .available()
                    .iter()
                    .map(|account| account.id.clone())
                    .collect();
                let wanted: Vec<_> = std::iter::once(fam("anthropic"))
                    .chain(expected.iter().map(|(id, _)| id.clone()))
                    .collect();
                assert_eq!(ids, wanted);
                let cells: Vec<_> = expected
                    .iter()
                    .map(|(id, token)| {
                        let Some(Credential::OAuth(cell)) = store.get(id) else {
                            panic!("missing {id}")
                        };
                        assert_eq!(cell.snapshot(), *token);
                        cell
                    })
                    .collect();
                for (index, cell) in cells.iter().enumerate() {
                    for other in &cells[index + 1..] {
                        assert!(!cell.shares_owner(other));
                    }
                }
                let mut renewed = expected[2].1.clone();
                renewed.access_token = "renewed".into();
                assert!(oauth::save_one(&renewed).unwrap());
                let login = oauth::credentials()
                    .unwrap()
                    .into_iter()
                    .find(|login| {
                        login.snapshot().issued == renewed.issued
                            && login.snapshot().login() == renewed.login()
                    })
                    .unwrap();
                store.add_login(&login).unwrap();
                let Some(Credential::OAuth(current)) = store.get(&expected[2].0) else {
                    panic!("lost renewed account")
                };
                assert!(current.shares_owner(&cells[2]));
                assert_eq!(cells[2].snapshot(), renewed);
                for (index, (_, token)) in expected.iter().enumerate() {
                    if index != 2 {
                        assert_eq!(cells[index].snapshot(), *token);
                    }
                }
            });
        }
    );
}

#[test]
fn login_services_never_resolve_synthetic_api_key_variables() {
    ral_core::isolated!(
        login_services_never_resolve_synthetic_api_key_variables,
        || {
            with_env(
                &[
                    ("OPENAI_CHATGPT_API_KEY", Some("not-a-login")),
                    ("OPENAI_CODEX_API_KEY", Some("not-a-login")),
                    ("XAI_GROK_API_KEY", Some("not-a-login")),
                ],
                || {
                    let store = CredentialStore::resolve_and_scrub(Vec::new());
                    for login in Login::ALL {
                        assert!(!store.is_available(&AccountId::of_service(&login.service().name)));
                    }
                    assert_eq!(store.available(), Vec::<Account>::new());
                },
            );
        }
    );
}

/// A declared service from `config.ral` is swept exactly like a built-in
/// one: its `{NAME}_API_KEY` is read into the store, scrubbed from the
/// environment, and it appears in `available()` after the built-in
/// services. An absent declared key leaves it unavailable while the
/// built-in ones still resolve — the declaration is additive, never a
/// precondition.
#[test]
fn declared_service_resolves_and_scrubs_its_key() {
    ral_core::isolated!(declared_service_resolves_and_scrubs_its_key, || {
        let declared = Service::declared(
            ServiceName::declared("local-llama").unwrap(),
            "https://llama.example/v1/".into(),
            genai::adapter::AdapterKind::OpenAI,
            Auth::Key("LOCAL_LLAMA_API_KEY".into()),
        );
        with_env(
            &[
                ("ANTHROPIC_API_KEY", Some("a")),
                ("OPENAI_API_KEY", None),
                ("OPENROUTER_API_KEY", None),
                ("DEEPSEEK_API_KEY", None),
                ("LOCAL_LLAMA_API_KEY", Some("  llama-secret  ")),
            ],
            || {
                let store = CredentialStore::resolve_and_scrub(vec![declared.clone()]);
                let id = AccountId::of_service(&declared.name);
                match store.get(&id) {
                    Some(Credential::ApiKey(k)) => assert_eq!(k, "llama-secret"),
                    _ => panic!("a declared service should resolve to a trimmed ApiKey"),
                }
                assert!(
                    std::env::var("LOCAL_LLAMA_API_KEY").is_err(),
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
    });
}

/// A declared service that checks no key (a local server like Ollama) is
/// available with no env var set at all, resolving to the inert
/// [`NO_AUTH_PLACEHOLDER`] bearer rather than a real credential. Nothing is
/// read from or scrubbed from the environment on its behalf.
#[test]
fn keyless_declared_service_resolves_to_placeholder() {
    ral_core::isolated!(keyless_declared_service_resolves_to_placeholder, || {
        let declared = Service::declared(
            ServiceName::declared("ollama").unwrap(),
            "http://localhost:11434/v1/".into(),
            genai::adapter::AdapterKind::OpenAI,
            Auth::Keyless,
        );
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
    });
}

/// The account itself, not only its id, is what `available()` hands back —
/// `Account::of_service` names both a key-bearing account's id and its
/// handle after the service's own name.
#[test]
fn a_key_bearing_account_names_itself_after_its_service() {
    ral_core::isolated!(a_key_bearing_account_names_itself_after_its_service, || {
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
    });
}

/// The store seals its process: not even the process itself can read back
/// the launch environment through `/proc`.
#[cfg(target_os = "linux")]
#[test]
fn resolution_seals_proc_environ() {
    ral_core::isolated!(resolution_seals_proc_environ, || {
        // Root's `CAP_SYS_PTRACE` reads a sealed process anyway.
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            ral_core::errln!("skipped: root can read a sealed process");
            return;
        }
        with_env(&[], || {
            let _store = CredentialStore::resolve_and_scrub(Vec::new());
            let err = std::fs::read("/proc/self/environ")
                .expect_err("a sealed process must not read its own launch environment");
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        });
    });
}
