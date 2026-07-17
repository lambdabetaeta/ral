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
//! endpoint becomes available the moment its key is in the environment. A
//! custom provider declared with *no* `key` is a no-auth local endpoint
//! (Ollama, llama.cpp, LM Studio): it is available immediately, with no env
//! var to read or scrub, resolving to an inert [`NO_AUTH_PLACEHOLDER`] bearer
//! that such servers ignore.
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

/// The inert bearer bound for a custom provider declared with no `key` — a
/// no-auth local endpoint (Ollama et al.). It is not a secret and never
/// authenticates anything: such servers ignore the `Authorization` header
/// entirely, but genai's OpenAI adapter always attaches one, so a placeholder
/// stands in for a real key. Distinct, recognisable text so it is obvious in a
/// captured request that no real credential is in play. `pub` so the
/// environment-scenario tests in `tests/credential_env.rs` can assert against
/// it symbolically.
pub const NO_AUTH_PLACEHOLDER: &str = "no-auth";

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
            // A provider with no key env var (`key_env()` is `None`) is one of
            // two things: a ChatGPT account, resolved from the token store
            // below rather than the environment — skipped here; or a keyless
            // custom provider, a no-auth local endpoint bound to an inert
            // placeholder bearer so it is available with no env var and nothing
            // to scrub.
            let Some(var) = id.key_env() else {
                if matches!(id, ProviderId::Custom(_)) {
                    ready.insert(id.clone(), Credential::ApiKey(NO_AUTH_PLACEHOLDER.to_string()));
                }
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
mod tests {
    use super::*;

    // `resolve_and_scrub`'s environment scenarios live in
    // `tests/credential_env.rs`: they mutate the process-global environment,
    // which must not share a process with the library tests that read it.

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
}
