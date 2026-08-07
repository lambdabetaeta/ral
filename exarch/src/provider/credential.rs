//! Provider auto-discovery and credential resolution.
//!
//! At startup each provider's conventional key variable is read into the
//! in-memory [`CredentialStore`] and then scrubbed from the environment, so no
//! child a tool call spawns inherits a live key; the scrub is also why
//! resolution is eager, since afterwards there is nothing left to re-read.
//! Famous [`ProviderKind`]s and the custom endpoints declared in `config.ral`
//! ([`crate::config`]) ride the same sweep — a custom provider with no `key` is
//! a local no-auth server, available at once behind [`NO_AUTH_PLACEHOLDER`].
//!
//! Signed-in `ChatGPT` accounts ([`crate::provider::oauth`]) come from the token
//! store rather than the environment, one [`ProviderId::ChatGpt`] each, and are
//! identities distinct from the API-key `OpenAI` provider: a login and an
//! `OPENAI_API_KEY` are two selectable providers, not one.

use crate::provider::{ChatGptAccount, CustomProvider, ProviderId, ProviderKind};
use crate::sync::LockExt;
use clap::ValueEnum;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

/// The inert bearer bound to a keyless custom provider.
///
/// Ollama and its kin ignore `Authorization` entirely, but `genai`'s `OpenAI`
/// adapter always attaches one; `pub` so `tests/credential_env.rs` can name
/// it.
pub const NO_AUTH_PLACEHOLDER: &str = "no-auth";

/// A resolved credential — what a provider's requests authenticate with.
///
/// An OAuth credential holds its `ChatGPT` token behind a shared cell that
/// `Engine::refresh_if_stale` mutates in place, so a session outliving an
/// access token keeps authenticating with no provider rebuild. Accounts own
/// distinct cells; their writes meet only at the on-disk token store, where
/// [`crate::provider::oauth`] serializes them.
#[derive(Clone)]
pub enum Credential {
    ApiKey(String),
    OAuth(Arc<Mutex<crate::provider::oauth::OAuthToken>>),
}

impl Credential {
    /// Whether this is a `ChatGPT` plan login — narrower than "unmetered",
    /// since a flat rate is separately a `ProviderId` property (opencode Go)
    /// riding an ordinary key.
    pub fn is_subscription(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }
}

/// The in-memory store of resolved credentials, keyed by [`ProviderId`].
pub struct CredentialStore {
    ready: BTreeMap<ProviderId, Credential>,
    /// Every known provider, available or not, in the order [`Self::available`]
    /// preserves: famous in [`ProviderKind`] declaration order, then `ChatGPT`
    /// accounts by label, then custom in config order.
    all: Vec<ProviderId>,
    /// Those whose key arrived through [`Self::admit_key`] — handed in by the
    /// embedding application — rather than swept from the environment.
    /// Recorded at the binding: only the store knows which of its two doors
    /// a key came through.
    admitted: BTreeSet<ProviderId>,
    /// What the environment sweep found, kept as the layer *beneath*
    /// [`Self::ready`] rather than overwritten by an admission.
    ///
    /// The sweep scrubs what it reads and so can never run twice. Without
    /// this layer, admitting a key over an environment-supplied one would
    /// destroy it for the life of the process, and withdrawing the admitted
    /// key would leave the provider unbound rather than falling back to the
    /// variable that was there all along.
    environment: BTreeMap<ProviderId, Credential>,
}

impl CredentialStore {
    /// Sweep every known provider, binding each usable key and then scrubbing
    /// every key variable that was *present* — the malformed one too, since a
    /// value with a pasted newline is still a live secret for a child to
    /// inherit.
    ///
    /// SAFETY: call this while the process is still single-threaded (before any
    /// session worker thread exists), so the env scrub races nothing.
    pub fn resolve_and_scrub(custom: Vec<CustomProvider>) -> Self {
        let mut accounts: Vec<(ProviderId, crate::provider::oauth::OAuthToken)> =
            crate::provider::oauth::load_all()
                .into_iter()
                .map(|tok| (chatgpt_id(&tok), tok))
                .collect();
        // By label, so the token store's file order never reaches the picker.
        accounts.sort_by(|(a, _), (b, _)| a.label().cmp(b.label()));

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
            // No key variable: a ChatGPT account, bound from the token store
            // below; or a keyless custom provider, a no-auth local endpoint.
            let Some(var) = id.key_env() else {
                if matches!(id, ProviderId::Custom(_)) {
                    ready.insert(
                        id.clone(),
                        Credential::ApiKey(NO_AUTH_PLACEHOLDER.to_string()),
                    );
                }
                continue;
            };
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
        // SAFETY: single-threaded startup, per this function's contract.
        #[allow(clippy::disallowed_methods)]
        for var in &scrub {
            unsafe {
                std::env::remove_var(var);
            }
        }

        // Taken before the logins are folded in: a plan login is not an
        // environment key, and must never be what a withdrawn key falls back
        // to. A keyless custom provider's inert bearer is, though — reverting
        // to it is exactly what forgetting that key means.
        let environment = ready.clone();

        for (id, token) in accounts {
            ready.insert(id, Credential::OAuth(Arc::new(Mutex::new(token))));
        }

        Self {
            ready,
            all,
            admitted: BTreeSet::new(),
            environment,
        }
    }

    /// The credential bound to `id`, or `None` when it is not available.
    pub fn get(&self, id: &ProviderId) -> Option<&Credential> {
        self.ready.get(id)
    }

    /// Whether `id`'s credential is in the store.
    pub fn is_available(&self, id: &ProviderId) -> bool {
        self.ready.contains_key(id)
    }

    /// Whether `id` is bound to a `ChatGPT` plan login rather than an API key.
    pub fn is_subscription(&self, id: &ProviderId) -> bool {
        self.get(id).is_some_and(Credential::is_subscription)
    }

    /// The available providers, in [`Self::all`]'s order.
    pub fn available(&self) -> Vec<ProviderId> {
        self.all
            .iter()
            .filter(|id| self.ready.contains_key(id))
            .cloned()
            .collect()
    }

    /// Admit a freshly signed-in `ChatGPT` account into the live store, the
    /// mid-session counterpart of [`Self::resolve_and_scrub`]'s one-shot load.
    /// A re-login for an already-listed account writes the fresh token into the
    /// *existing* shared cell — the one a live provider authenticates through —
    /// so it takes effect with no provider rebuild. The returned pair is what
    /// `ModelCatalog::add_credential` and the other live views need.
    pub fn add_oauth(
        &mut self,
        token: &crate::provider::oauth::OAuthToken,
    ) -> (ProviderId, Credential) {
        let existing_id = self.all.iter().find(
            |id| matches!(id, ProviderId::ChatGpt(acc) if acc.account_id == token.account_id),
        );
        let Some(old_id) = existing_id.cloned() else {
            let id = chatgpt_id(token);
            let credential = Credential::OAuth(Arc::new(Mutex::new(token.clone())));
            self.ready.insert(id.clone(), credential.clone());
            self.insert_chatgpt(id.clone());
            return (id, credential);
        };

        let Some(Credential::OAuth(cell)) = self.ready.get(&old_id) else {
            unreachable!("a ChatGpt ProviderId is always bound to an OAuth credential");
        };
        let cell = Arc::clone(cell);
        *cell.lock_ignore_poison() = token.clone();
        let credential = Credential::OAuth(cell);

        if old_id.label() == token.label() {
            return (old_id, credential);
        }
        // The picker and the persisted selection key by label, so a login that
        // now carries an email re-keys the id; the cell moves across untouched.
        self.ready.remove(&old_id);
        let new_id = chatgpt_id(token);
        self.all.retain(|id| *id != old_id);
        self.ready.insert(new_id.clone(), credential.clone());
        self.insert_chatgpt(new_id.clone());
        (new_id, credential)
    }

    /// Every provider this store knows of, available or not — a provider
    /// with no key yet is precisely the one an accounts screen exists for.
    pub fn known(&self) -> &[ProviderId] {
        &self.all
    }

    /// Bind `key` to `id`, replacing whatever it was bound to.
    ///
    /// The mid-session counterpart of the startup sweep, as
    /// [`Self::add_oauth`] is for a login — which matters because
    /// [`Self::resolve_and_scrub`] can never be run twice in one process.
    /// A provider not yet in [`Self::all`] — a custom endpoint declared
    /// moments ago — is admitted at the end, where custom providers belong.
    pub fn admit_key(&mut self, id: &ProviderId, key: String) -> Credential {
        let credential = Credential::ApiKey(key);
        if !self.all.contains(id) {
            self.all.push(id.clone());
        }
        self.ready.insert(id.clone(), credential.clone());
        self.admitted.insert(id.clone());
        credential
    }

    /// Whether `id`'s key was handed in by the application rather than
    /// swept from the environment — only the first kind is a window's to
    /// withdraw.
    pub fn was_admitted(&self, id: &ProviderId) -> bool {
        self.admitted.contains(id)
    }

    /// Withdraw the key [`Self::admit_key`] bound, revealing the environment
    /// layer beneath it — so a provider the launching environment supplied
    /// all along goes back to that key rather than becoming unavailable, and
    /// one the environment never supplied is left known but unbound.
    pub fn forget(&mut self, id: &ProviderId) {
        self.admitted.remove(id);
        match self.environment.get(id) {
            Some(credential) => self.ready.insert(id.clone(), credential.clone()),
            None => self.ready.remove(id),
        };
    }

    /// Drop `id` from the store entirely, credential and identity both —
    /// what a *withdrawn declaration* means, as against [`Self::forget`]'s
    /// merely-unbound key. Nothing is left to fall back to.
    pub fn retire(&mut self, id: &ProviderId) {
        self.ready.remove(id);
        self.admitted.remove(id);
        self.environment.remove(id);
        self.all.retain(|known| known != id);
    }

    /// Insert an account where [`Self::all`]'s ordering requires: after every
    /// famous provider and every account whose label sorts before it, before
    /// the first custom provider.
    fn insert_chatgpt(&mut self, id: ProviderId) {
        let pos = self
            .all
            .iter()
            .position(|other| match other {
                ProviderId::Famous(_) => false,
                ProviderId::ChatGpt(_) => other.label() > id.label(),
                ProviderId::Custom(_) => true,
            })
            .unwrap_or(self.all.len());
        self.all.insert(pos, id);
    }
}

fn chatgpt_id(token: &crate::provider::oauth::OAuthToken) -> ProviderId {
    ProviderId::ChatGpt(Arc::new(ChatGptAccount::from_token(token)))
}

/// The state of a provider's key environment variable, from a single read.
enum EnvKey {
    Absent,
    /// Blank, or carrying a control character (a pasted newline): it binds no
    /// credential, but is still a live secret to scrub.
    Malformed,
    Valid(String),
}

fn read_env_key(var: &str) -> EnvKey {
    #[allow(clippy::disallowed_methods)]
    let Ok(raw) = std::env::var(var) else {
        return EnvKey::Absent;
    };
    well_formed_key(&raw).map_or(EnvKey::Malformed, EnvKey::Valid)
}

/// A key as it will actually be used, or `None` when it authenticates
/// nothing: blank, or carrying a control character — a newline copied along
/// with the key being the usual way.
///
/// One rule wherever a key arrives, so a secret refused at one door is not
/// quietly accepted at another: the environment here, a window, or the
/// computer's credential manager ([`crate::provider::keychain`]).
pub fn well_formed_key(raw: &str) -> Option<String> {
    let key = raw.trim();
    (!key.is_empty() && !key.bytes().any(|b| b < 0x20 || b == 0x7f)).then(|| key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `resolve_and_scrub` scenarios live in `tests/credential_env.rs`: they
    // mutate the process-global environment, which no library test may share.

    use crate::provider::oauth::OAuthToken;

    fn oauth_token(account_id: &str, email: Option<&str>) -> OAuthToken {
        OAuthToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: account_id.into(),
            email: email.map(str::to_string),
            expires_at: 0,
        }
    }

    fn custom(label: &str) -> ProviderId {
        ProviderId::Custom(Arc::new(CustomProvider {
            label: label.into(),
            key_env: None,
            endpoint: format!("http://{label}/v1/"),
            adapter: genai::adapter::AdapterKind::OpenAI,
        }))
    }

    /// One rule at every door: surrounding whitespace is not part of a key,
    /// and a key carrying a pasted newline authenticates nothing.
    #[test]
    fn a_key_is_trimmed_or_refused_outright() {
        assert_eq!(well_formed_key(" sk-real ").as_deref(), Some("sk-real"));
        assert_eq!(well_formed_key("   "), None);
        assert_eq!(well_formed_key(""), None);
        assert_eq!(well_formed_key("sk-real\nGET /"), None);
        assert_eq!(well_formed_key("sk\u{7f}real"), None);
    }

    /// A window may withdraw the key it put away; it may not thereby destroy
    /// the one the launching environment supplied, which no later sweep could
    /// ever recover.
    #[test]
    fn forgetting_an_admitted_key_reveals_the_environment_beneath_it() {
        let id = ProviderId::Famous(ProviderKind::Anthropic);
        let mut store = CredentialStore {
            ready: BTreeMap::from([(id.clone(), Credential::ApiKey("from-env".into()))]),
            all: vec![id.clone()],
            admitted: BTreeSet::new(),
            environment: BTreeMap::from([(id.clone(), Credential::ApiKey("from-env".into()))]),
        };

        store.admit_key(&id, "from-window".into());
        assert!(store.was_admitted(&id));
        assert!(matches!(store.get(&id), Some(Credential::ApiKey(k)) if k == "from-window"));

        store.forget(&id);
        assert!(!store.was_admitted(&id));
        assert!(matches!(store.get(&id), Some(Credential::ApiKey(k)) if k == "from-env"));
    }

    /// With nothing underneath, the same withdrawal leaves the provider known
    /// but unbound — still a service, simply with nothing to speak to it with.
    #[test]
    fn forgetting_a_key_the_environment_never_supplied_leaves_it_unbound() {
        let id = custom("house-llm");
        let mut store = CredentialStore {
            ready: BTreeMap::new(),
            all: vec![id.clone()],
            admitted: BTreeSet::new(),
            environment: BTreeMap::new(),
        };

        store.admit_key(&id, "typed".into());
        store.forget(&id);

        assert!(store.get(&id).is_none());
        assert_eq!(store.known(), [id]);
    }

    #[test]
    fn add_oauth_new_account_sorts_after_famous_and_before_custom() {
        let llama = custom("local-llama");
        let mut store = CredentialStore {
            ready: BTreeMap::from([
                (
                    ProviderId::Famous(ProviderKind::Anthropic),
                    Credential::ApiKey("key".into()),
                ),
                (
                    llama.clone(),
                    Credential::ApiKey(NO_AUTH_PLACEHOLDER.into()),
                ),
            ]),
            all: vec![ProviderId::Famous(ProviderKind::Anthropic), llama.clone()],
            admitted: BTreeSet::new(),
            environment: BTreeMap::new(),
        };

        let (bravo, _) = store.add_oauth(&oauth_token("acc_b", Some("bravo@work")));
        let (alpha, _) = store.add_oauth(&oauth_token("acc_a", Some("alpha@work")));

        assert_eq!(
            store.available(),
            vec![
                ProviderId::Famous(ProviderKind::Anthropic),
                alpha,
                bravo,
                llama,
            ],
            "famous, then ChatGPT accounts in label order, then custom"
        );
    }

    /// The `Arc` a live provider still holds is the one that sees the refresh.
    #[test]
    fn add_oauth_relogin_updates_the_shared_cell_in_place() {
        let id = ProviderId::ChatGpt(Arc::new(ChatGptAccount {
            account_id: "acc_1".into(),
            label: "alex@work".into(),
        }));
        let cell = Arc::new(Mutex::new(oauth_token("acc_1", Some("alex@work"))));
        let mut store = CredentialStore {
            ready: BTreeMap::from([(id.clone(), Credential::OAuth(Arc::clone(&cell)))]),
            all: vec![id.clone()],
            admitted: BTreeSet::new(),
            environment: BTreeMap::new(),
        };
        let before = store.available();

        let refreshed = OAuthToken {
            access_token: "fresh-at".into(),
            ..oauth_token("acc_1", Some("alex@work"))
        };
        let (returned, returned_credential) = store.add_oauth(&refreshed);

        assert_eq!(returned, id);
        let Credential::OAuth(returned_cell) = returned_credential else {
            panic!("expected an OAuth credential");
        };
        assert!(Arc::ptr_eq(&returned_cell, &cell));
        assert_eq!(store.available(), before, "a re-login adds no new provider");
        assert_eq!(
            cell.lock().unwrap().access_token,
            "fresh-at",
            "the pre-existing cell — the one a live provider reads through — sees the refresh"
        );
    }

    /// An email arriving on re-login re-keys the id; the shared cell survives.
    #[test]
    fn add_oauth_relogin_with_changed_label_rekeys_the_provider_id() {
        let old_id = ProviderId::ChatGpt(Arc::new(ChatGptAccount {
            account_id: "acc_1".into(),
            label: "acc_1".into(),
        }));
        let cell = Arc::new(Mutex::new(oauth_token("acc_1", None)));
        let mut store = CredentialStore {
            ready: BTreeMap::from([(old_id.clone(), Credential::OAuth(Arc::clone(&cell)))]),
            all: vec![old_id.clone()],
            admitted: BTreeSet::new(),
            environment: BTreeMap::new(),
        };

        let (new_id, returned_credential) =
            store.add_oauth(&oauth_token("acc_1", Some("alex@work")));

        assert_eq!(new_id.label(), "alex@work");
        assert_ne!(new_id, old_id);
        assert!(
            !store.is_available(&old_id),
            "the old id no longer resolves"
        );
        assert!(
            store.is_available(&new_id),
            "the credential survives under the new id"
        );
        assert_eq!(
            store.all,
            vec![new_id.clone()],
            "re-keyed in place, not duplicated"
        );
        let Some(Credential::OAuth(moved)) = store.get(&new_id) else {
            panic!("expected an OAuth credential");
        };
        assert!(
            Arc::ptr_eq(moved, &cell),
            "the same shared cell moves to the new id"
        );
        let Credential::OAuth(returned_cell) = returned_credential else {
            panic!("expected an OAuth credential");
        };
        assert!(Arc::ptr_eq(&returned_cell, &cell));
    }

    /// Zen and Go share a gateway and one API key; only Go bills flat. The
    /// split is a `ProviderId` property, independent of the credential.
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
