//! Provider auto-discovery and credential resolution.
//!
//! A famous provider whose conventional key variable
//! ([`ProviderKind::info`]`.2`) holds a usable value is *available*. At
//! startup every available provider's key is read from its variable into
//! the in-memory [`CredentialStore`], and every key variable that fed a
//! credential — or was present but malformed — is then scrubbed from the
//! process environment. The scrub keeps a child a tool call spawns from
//! inheriting a live key and, because the variable is gone, forecloses
//! re-reading it later, which is why resolution is eager.
//!
//! This generalises the single-key read-and-scrub that lived inline in
//! `run`: it now sweeps every [`ProviderKind`] rather than the one a
//! `--provider` flag named, with provider knowledge still sourced once
//! from [`ProviderKind::info`].

use crate::provider::ProviderKind;
use clap::ValueEnum;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// A resolved credential — what a provider's requests authenticate with.
///
/// An [`ApiKey`](Self::ApiKey) is the bearer string read from a provider's
/// key variable. An [`OAuth`](Self::OAuth) credential is a ChatGPT plan
/// login held behind a shared cell: a turn reads the current access token
/// through it, and a refresh (in [`crate::provider`]) writes a renewed token
/// back into the same cell, so a session that outlives the access token keeps
/// authenticating without rebuilding the provider.
#[derive(Clone)]
pub enum Credential {
    /// An API key read from the environment at startup.
    ApiKey(String),
    /// A ChatGPT plan login for the OpenAI provider, shared so a mid-session
    /// refresh is visible to the request path.
    OAuth(Arc<Mutex<crate::oauth::OAuthToken>>),
}

impl Credential {
    /// Whether this credential is a ChatGPT plan login (a flat subscription)
    /// rather than an API key. The single spelling of the OAuth-vs-key
    /// distinction the store answers; the provider it builds carries the
    /// same distinction via [`crate::provider::Provider::is_subscription`].
    pub fn is_subscription(&self) -> bool {
        matches!(self, Credential::OAuth(_))
    }
}

/// The in-memory store of resolved credentials, keyed by [`ProviderKind`].
/// Built once at startup; a turn draws exactly one credential from it.
pub struct CredentialStore {
    ready: BTreeMap<ProviderKind, Credential>,
}

impl CredentialStore {
    /// Sweep every known provider: read its conventional key variable, and
    /// resolve a usable value into the store. Then scrub every key variable
    /// that was *present* — whether or not it yielded a usable key — from
    /// the environment. Scrubbing the set-but-malformed variable too (a key
    /// with a pasted newline, say) matters: leaving it set would let a child
    /// a tool call spawns inherit the live secret.
    ///
    /// SAFETY: the caller must invoke this while the process is still
    /// single-threaded (before any session worker thread is created), so
    /// the env scrub cannot race another thread.
    pub fn resolve_and_scrub() -> Self {
        let mut ready = BTreeMap::new();
        let mut scrub: Vec<&'static str> = Vec::new();

        for kind in ProviderKind::value_variants() {
            let var = kind.info().2;
            // Scrub any var that is *present*, valid or not — a malformed
            // key is still a live secret in the child env.
            #[allow(clippy::disallowed_methods)]
            if std::env::var(var).is_ok() {
                scrub.push(var);
            }
            if let EnvKey::Valid(key) = read_env_key(var) {
                ready.insert(*kind, Credential::ApiKey(key));
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

        // A stored ChatGPT login drives the OpenAI provider off the plan
        // subscription; it supersedes any `OPENAI_API_KEY` (already read and
        // scrubbed above), so a present login always wins for that provider.
        if let Some(token) = crate::oauth::load() {
            ready.insert(
                ProviderKind::Openai,
                Credential::OAuth(Arc::new(Mutex::new(token))),
            );
        }

        Self { ready }
    }

    /// The credential for an available provider, or `None` when its key was
    /// absent or malformed.
    pub fn get(&self, kind: ProviderKind) -> Option<&Credential> {
        self.ready.get(&kind)
    }

    /// Whether `kind`'s credential is in the store.
    pub fn is_available(&self, kind: ProviderKind) -> bool {
        self.ready.contains_key(&kind)
    }

    /// The available providers, in [`ProviderKind`] declaration order.
    pub fn available(&self) -> Vec<ProviderKind> {
        ProviderKind::value_variants()
            .iter()
            .copied()
            .filter(|k| self.ready.contains_key(k))
            .collect()
    }
}

/// The state of a provider's key environment variable.
enum EnvKey {
    /// A usable key (trimmed, non-empty, no control characters).
    Valid(String),
    /// The variable is unset, or set but empty / whitespace-only / carrying
    /// a control character (a stray newline pasted into the value) — it
    /// would fail at the provider, so it is not a usable key.
    Unusable,
}

/// Classify an environment variable as a credential key. The validity rule
/// matches the inline single-key check `run` used before auto-discovery: a
/// control character (a pasted newline) means the value cannot be a key.
fn read_env_key(var: &str) -> EnvKey {
    #[allow(clippy::disallowed_methods)]
    let Ok(raw) = std::env::var(var) else {
        return EnvKey::Unusable;
    };
    let key = raw.trim().to_string();
    if key.is_empty() || key.bytes().any(|b| b < 0x20 || b == 0x7f) {
        EnvKey::Unusable
    } else {
        EnvKey::Valid(key)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[io-door:test] test fs/process scaffolding")]
mod tests {
    use super::*;

    /// `resolve_and_scrub` is process-global (it mutates the real
    /// environment), so each scenario uses a guard that snapshots every
    /// provider key var, sets the scenario's values, runs the body, and
    /// restores. A process-wide lock serialises env-mutating tests under
    /// `RUST_TEST_THREADS > 1` so they cannot interleave their mutations.
    ///
    /// `resolve_and_scrub` also reads the persisted ChatGPT login from the
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
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // A fresh empty state dir per call, so `oauth::load` finds no token
        // unless the scenario sets one up under its own XDG_STATE_HOME.
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
                let store = CredentialStore::resolve_and_scrub();
                match store.get(ProviderKind::Anthropic) {
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
                assert!(!store.is_available(ProviderKind::Openai));
                assert_eq!(store.available(), vec![ProviderKind::Anthropic]);
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
                let store = CredentialStore::resolve_and_scrub();
                assert!(!store.is_available(ProviderKind::Openai));
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
            ],
            || {
                let store = CredentialStore::resolve_and_scrub();
                assert_eq!(
                    store.available(),
                    vec![
                        ProviderKind::Anthropic,
                        ProviderKind::Openrouter,
                        ProviderKind::Deepseek
                    ]
                );
            },
        );
    }

    /// A stored ChatGPT login resolves the OpenAI provider to an OAuth
    /// credential and wins over a present `OPENAI_API_KEY`, which is still
    /// scrubbed.
    #[test]
    fn oauth_login_supersedes_openai_key() {
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
                crate::oauth::save(&crate::oauth::OAuthToken {
                    access_token: "at".into(),
                    refresh_token: "rt".into(),
                    account_id: "acc".into(),
                    expires_at: u64::MAX,
                })
                .expect("save token");
                let store = CredentialStore::resolve_and_scrub();
                assert!(
                    matches!(store.get(ProviderKind::Openai), Some(Credential::OAuth(_))),
                    "a stored login must win over OPENAI_API_KEY"
                );
                #[allow(clippy::disallowed_methods)]
                {
                    assert!(
                        std::env::var("OPENAI_API_KEY").is_err(),
                        "the superseded key var must still be scrubbed"
                    );
                }
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
