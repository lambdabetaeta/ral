//! Provider auto-discovery and credential resolution.
//!
//! At startup each built-in service's conventional key variable is read into
//! the in-memory [`CredentialStore`] and then scrubbed from the environment,
//! so no child a tool call spawns inherits a live key; the scrub is also why
//! resolution is eager, since afterwards there is nothing left to re-read.
//! Declared endpoints ([`crate::config`]) ride the same sweep — one with no
//! `key:` is a local no-auth server, available at once behind
//! [`NO_AUTH_PLACEHOLDER`].
//!
//! Signed-in `ChatGPT` accounts ([`crate::provider::oauth`]) come from the
//! token store rather than the environment, one account per login, and are
//! identities distinct from an API-key `openai` account: a login and an
//! `OPENAI_API_KEY` are two selectable accounts, not one.

use crate::provider::identity::{self, Account, AccountId, Auth, Service};
use crate::sync::LockExt;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

/// The inert bearer bound to a keyless declared endpoint.
///
/// Ollama and its kin ignore `Authorization` entirely, but `genai`'s `OpenAI`
/// adapter always attaches one; `pub` so `tests/credential_env.rs` can name
/// it.
pub const NO_AUTH_PLACEHOLDER: &str = "no-auth";

/// A resolved credential — what an account's requests authenticate with.
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

/// A place a running product keeps typed-in secrets.
///
/// synod's platform credential manager; exarch itself has none. Specified by
/// whoever has a vault to offer, and implemented there
/// ([`crate::provider::keychain::Keychain`]).
pub trait SecretVault {
    fn read(&self, account: &Account) -> Option<String>;
}

/// The in-memory store of resolved credentials, keyed by [`AccountId`].
pub struct CredentialStore {
    /// Every known account, available or not — the one owner of the account
    /// records; every field below names one by its [`AccountId`] alone.
    all: Vec<Account>,
    ready: BTreeMap<AccountId, Credential>,
    /// Those whose key arrived through [`Self::admit_key`] — handed in by the
    /// embedding application — rather than swept from the environment.
    /// Recorded at the binding: only the store knows which of its two doors
    /// a key came through.
    admitted: BTreeSet<AccountId>,
    /// What the environment sweep found, kept as the layer *beneath*
    /// [`Self::ready`] rather than overwritten by an admission.
    ///
    /// The sweep scrubs what it reads and so can never run twice. Without
    /// this layer, admitting a key over an environment-supplied one would
    /// destroy it for the life of the process, and withdrawing the admitted
    /// key would leave the account unbound rather than falling back to the
    /// variable that was there all along.
    environment: BTreeMap<AccountId, Credential>,
}

impl CredentialStore {
    /// Sweep every built-in and declared service, binding each usable key and
    /// then scrubbing every key variable that was *present* — the malformed
    /// one too, since a value with a pasted newline is still a live secret
    /// for a child to inherit. `ChatGPT` logins are folded in afterwards, from
    /// the token store rather than the environment.
    ///
    /// SAFETY: call this while the process is still single-threaded (before any
    /// session worker thread exists), so the env scrub races nothing.
    pub fn resolve_and_scrub(declared: Vec<Service>) -> Self {
        let built_in: Vec<Service> = identity::built_in_services()
            .into_iter()
            .filter(|service| !matches!(service.auth, Auth::OAuth))
            .collect();
        // Where the declared block begins, once the chatgpt logins below are
        // spliced in between it and the built-in one.
        let boundary = built_in.len();
        let mut all: Vec<Account> = built_in
            .into_iter()
            .chain(declared)
            .map(Account::of_service)
            .collect();

        let mut ready = BTreeMap::new();
        let mut scrub: Vec<String> = Vec::new();

        for account in &all {
            match &account.service.auth {
                Auth::Env(var) => match read_env_key(var) {
                    EnvKey::Absent => {}
                    EnvKey::Malformed => scrub.push(var.clone()),
                    EnvKey::Valid(key) => {
                        scrub.push(var.clone());
                        ready.insert(account.id.clone(), Credential::ApiKey(key));
                    }
                },
                Auth::Unnamed => {
                    ready.insert(
                        account.id.clone(),
                        Credential::ApiKey(NO_AUTH_PLACEHOLDER.to_string()),
                    );
                }
                // No key variable to sweep: `ChatGPT`'s accounts are folded
                // in below, from the token store.
                Auth::OAuth => {}
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
        // to. A keyless declared endpoint's inert bearer is, though —
        // reverting to it is exactly what forgetting that key means.
        let environment = ready.clone();

        let mut logins: Vec<(Account, crate::provider::oauth::OAuthToken)> =
            crate::provider::oauth::load_all()
                .into_iter()
                .map(|token| (crate::provider::oauth::to_account(&token), token))
                .collect();
        logins.sort_by(|(a, _), (b, _)| a.handle.cmp(&b.handle));
        for (offset, (account, token)) in logins.into_iter().enumerate() {
            ready.insert(
                account.id.clone(),
                Credential::OAuth(Arc::new(Mutex::new(token))),
            );
            all.insert(boundary + offset, account);
        }

        Self {
            ready,
            all,
            admitted: BTreeSet::new(),
            environment,
        }
    }

    /// The credential bound to `id`, or `None` when it is not available.
    pub fn get(&self, id: &AccountId) -> Option<&Credential> {
        self.ready.get(id)
    }

    /// Whether `id`'s credential is in the store.
    pub fn is_available(&self, id: &AccountId) -> bool {
        self.ready.contains_key(id)
    }

    /// The available accounts, in [`Self::all`]'s order.
    pub fn available(&self) -> Vec<Account> {
        self.all
            .iter()
            .filter(|account| self.ready.contains_key(&account.id))
            .cloned()
            .collect()
    }

    /// Admit a freshly signed-in `ChatGPT` account into the live store, the
    /// mid-session counterpart of [`Self::resolve_and_scrub`]'s one-shot load.
    /// A re-login for an already-listed account writes the fresh token into
    /// the *existing* shared cell — the one a live provider authenticates
    /// through — so it takes effect with no provider rebuild, and it stays
    /// the same account throughout however its handle changes. Needs no set
    /// of sibling logins: the handle a `ChatGPT` account draws is a local
    /// fact about its own credential alone.
    ///
    /// The admitted [`Account`] comes back rather than just its id, because
    /// the store has it in hand and a caller that had to search for it again
    /// would be asserting an invariant only this method can keep.
    pub fn add_oauth(
        &mut self,
        token: &crate::provider::oauth::OAuthToken,
    ) -> (Account, Credential) {
        let account = crate::provider::oauth::to_account(token);
        let credential = if let Some(Credential::OAuth(cell)) = self.ready.get(&account.id) {
            *cell.lock_ignore_poison() = token.clone();
            Credential::OAuth(Arc::clone(cell))
        } else {
            let credential = Credential::OAuth(Arc::new(Mutex::new(token.clone())));
            self.ready.insert(account.id.clone(), credential.clone());
            credential
        };
        // Re-placed under its current handle: an arrival takes its spot among
        // the other logins, and a refresh that has finally learned the email
        // or workspace claim moves there without disturbing its identity.
        self.all.retain(|listed| listed.id != account.id);
        self.insert_login(account.clone());
        (account, credential)
    }

    /// Every account this store knows of, available or not — an account with
    /// no key yet is precisely the one an accounts screen exists for.
    pub fn known(&self) -> &[Account] {
        &self.all
    }

    /// Bind `key` to `account`, replacing whatever it was bound to. Admits
    /// `account` itself too, if the store does not already know it — a
    /// declared endpoint's arrival and its key are one act here, as they are
    /// wherever a window offers to type both in together.
    ///
    /// The mid-session counterpart of the startup sweep, as [`Self::add_oauth`]
    /// is for a login — which matters because [`Self::resolve_and_scrub`] can
    /// never be run twice in one process.
    pub fn admit_key(&mut self, account: &Account, key: String) -> Credential {
        let credential = Credential::ApiKey(key);
        if !self.all.iter().any(|known| known.id == account.id) {
            self.all.push(account.clone());
        }
        self.ready.insert(account.id.clone(), credential.clone());
        self.admitted.insert(account.id.clone());
        credential
    }

    /// Whether `id`'s key was handed in by the application rather than
    /// swept from the environment — only the first kind is a window's to
    /// withdraw.
    pub fn was_admitted(&self, id: &AccountId) -> bool {
        self.admitted.contains(id)
    }

    /// Withdraw the key [`Self::admit_key`] bound, revealing the environment
    /// layer beneath it — so an account the launching environment supplied
    /// all along goes back to that key rather than becoming unavailable, and
    /// one the environment never supplied is left known but unbound.
    pub fn forget(&mut self, id: &AccountId) {
        self.admitted.remove(id);
        match self.environment.get(id) {
            Some(credential) => {
                self.ready.insert(id.clone(), credential.clone());
            }
            None => {
                self.ready.remove(id);
            }
        }
    }

    /// Drop `id` from the store entirely, credential and identity both —
    /// what a *withdrawn declaration* means, as against [`Self::forget`]'s
    /// merely-unbound key. Nothing is left to fall back to.
    pub fn retire(&mut self, id: &AccountId) {
        self.ready.remove(id);
        self.admitted.remove(id);
        self.environment.remove(id);
        self.all.retain(|known| &known.id != id);
    }

    /// Lay `vault` over the resolved environment sweep: a key it holds
    /// outranks a stale environment variable, exactly as typing one into
    /// synod's accounts screen always has. Signed-in logins are not offered —
    /// a vault answers for keys, and a stray entry must never shadow a
    /// login's token cell. An ordinary second call, run after
    /// [`Self::resolve_and_scrub`] — it scrubs nothing itself, and the eager
    /// scrub's SAFETY contract is that function's alone to keep.
    pub fn admit_from(&mut self, vault: &impl SecretVault) {
        let found: Vec<(Account, String)> = self
            .all
            .iter()
            .filter(|account| !matches!(account.service.auth, Auth::OAuth))
            .filter_map(|account| vault.read(account).map(|key| (account.clone(), key)))
            .collect();
        for (account, key) in found {
            self.admit_key(&account, key);
        }
    }

    /// Where a login belongs among the accounts already known: after every
    /// built-in service, among the other logins in handle order, before the
    /// first declared service.
    fn insert_login(&mut self, account: Account) {
        let pos = self
            .all
            .iter()
            .position(|other| match &other.service.auth {
                Auth::OAuth => other.handle > account.handle,
                _ => identity::built_in(&other.service.name).is_none(),
            })
            .unwrap_or(self.all.len());
        self.all.insert(pos, account);
    }
}

/// The state of an account's key environment variable, from a single read.
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
    use crate::provider::oauth::OAuthToken;
    use genai::adapter::AdapterKind;
    use identity::{Billing, ServiceName};

    // The `resolve_and_scrub` scenarios live in `tests/credential_env.rs`: they
    // mutate the process-global environment, which no library test may share.

    fn built_in(name: &str) -> Service {
        identity::built_in(&ServiceName::declared(name).unwrap()).unwrap()
    }

    fn declared(name: &str) -> Account {
        Account::of_service(Service {
            name: ServiceName::declared(name).unwrap(),
            endpoint: Some(format!("http://{name}/v1/")),
            adapter: AdapterKind::OpenAI,
            default_model: None,
            auth: Auth::Unnamed,
            billing: Billing::Metered,
            routes: false,
        })
    }

    fn oauth_token(issued: &str, email: Option<&str>) -> OAuthToken {
        OAuthToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            issued: issued.into(),
            email: email.map(str::to_string),
            workspace: None,
            plan: None,
            expires_at: 0,
        }
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
        let anthropic = Account::of_service(built_in("anthropic"));
        let mut store = CredentialStore {
            ready: BTreeMap::from([(anthropic.id.clone(), Credential::ApiKey("from-env".into()))]),
            all: vec![anthropic.clone()],
            admitted: BTreeSet::new(),
            environment: BTreeMap::from([(
                anthropic.id.clone(),
                Credential::ApiKey("from-env".into()),
            )]),
        };

        store.admit_key(&anthropic, "from-window".into());
        assert!(store.was_admitted(&anthropic.id));
        assert!(
            matches!(store.get(&anthropic.id), Some(Credential::ApiKey(k)) if k == "from-window")
        );

        store.forget(&anthropic.id);
        assert!(!store.was_admitted(&anthropic.id));
        assert!(matches!(store.get(&anthropic.id), Some(Credential::ApiKey(k)) if k == "from-env"));
    }

    /// With nothing underneath, the same withdrawal leaves the account known
    /// but unbound — still a service, simply with nothing to speak to it with.
    #[test]
    fn forgetting_a_key_the_environment_never_supplied_leaves_it_unbound() {
        let llama = declared("house-llm");
        let mut store = CredentialStore {
            ready: BTreeMap::new(),
            all: vec![llama.clone()],
            admitted: BTreeSet::new(),
            environment: BTreeMap::new(),
        };

        store.admit_key(&llama, "typed".into());
        store.forget(&llama.id);

        assert!(store.get(&llama.id).is_none());
        assert_eq!(store.known(), [llama]);
    }

    #[test]
    fn add_oauth_new_account_sorts_after_built_in_and_before_declared() {
        let anthropic = Account::of_service(built_in("anthropic"));
        let llama = declared("local-llama");
        let mut store = CredentialStore {
            ready: BTreeMap::from([
                (anthropic.id.clone(), Credential::ApiKey("key".into())),
                (
                    llama.id.clone(),
                    Credential::ApiKey(NO_AUTH_PLACEHOLDER.into()),
                ),
            ]),
            all: vec![anthropic.clone(), llama.clone()],
            admitted: BTreeSet::new(),
            environment: BTreeMap::new(),
        };

        let (bravo, _) = store.add_oauth(&oauth_token("acc_b", Some("bravo@work")));
        let (alpha, _) = store.add_oauth(&oauth_token("acc_a", Some("alpha@work")));

        assert_eq!(
            store
                .available()
                .into_iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            vec![anthropic.id, alpha.id, bravo.id, llama.id],
            "built-in, then chatgpt logins by handle, then declared"
        );
    }

    /// The `Arc` a live provider still holds is the one that sees the refresh.
    #[test]
    fn add_oauth_relogin_updates_the_shared_cell_in_place() {
        let seed = oauth_token("acc_1", Some("alex@work"));
        let account = crate::provider::oauth::to_account(&seed);
        let cell = Arc::new(Mutex::new(seed));
        let mut store = CredentialStore {
            ready: BTreeMap::from([(account.id.clone(), Credential::OAuth(Arc::clone(&cell)))]),
            all: vec![account.clone()],
            admitted: BTreeSet::new(),
            environment: BTreeMap::new(),
        };
        let before = store.available();

        let refreshed = OAuthToken {
            access_token: "fresh-at".into(),
            ..oauth_token("acc_1", Some("alex@work"))
        };
        let (returned, returned_credential) = store.add_oauth(&refreshed);

        assert_eq!(returned.id, account.id);
        let Credential::OAuth(returned_cell) = returned_credential else {
            panic!("expected an OAuth credential");
        };
        assert!(Arc::ptr_eq(&returned_cell, &cell));
        assert_eq!(store.available(), before, "a re-login adds no new account");
        assert_eq!(
            cell.lock().unwrap().access_token,
            "fresh-at",
            "the pre-existing cell — the one a live provider reads through — sees the refresh"
        );
    }

    /// A workspace claim arriving on re-login updates the handle without
    /// disturbing the account's identity.
    #[test]
    fn add_oauth_relogin_with_a_new_claim_renames_the_handle_in_place() {
        let mut store = CredentialStore {
            ready: BTreeMap::new(),
            all: Vec::new(),
            admitted: BTreeSet::new(),
            environment: BTreeMap::new(),
        };

        let (old, _) = store.add_oauth(&oauth_token("acc_1", None));
        assert_eq!(store.all[0].handle, "acc_1");

        let (new, returned_credential) = store.add_oauth(&oauth_token("acc_1", Some("alex@work")));

        assert_eq!(new.id, old.id, "it is the same account throughout");
        assert_eq!(store.all.len(), 1, "renamed, not duplicated");
        assert_eq!(store.all[0].handle, "alex@work");
        assert!(matches!(returned_credential, Credential::OAuth(_)));
    }

    /// Answers for everything — the misconfigured vault `admit_from` must
    /// stay guarded against.
    struct StickyVault;
    impl SecretVault for StickyVault {
        fn read(&self, _account: &Account) -> Option<String> {
            Some("from-vault".into())
        }
    }

    /// A vault answers for keys; even one claiming an entry for every account
    /// must never shadow a login's token cell.
    #[test]
    fn admit_from_never_touches_a_signed_in_login() {
        let anthropic = Account::of_service(built_in("anthropic"));
        let mut store = CredentialStore {
            ready: BTreeMap::new(),
            all: vec![anthropic.clone()],
            admitted: BTreeSet::new(),
            environment: BTreeMap::new(),
        };
        let (login, _) = store.add_oauth(&oauth_token("acc_1", Some("alex@work")));

        store.admit_from(&StickyVault);

        assert!(
            matches!(store.get(&login.id), Some(Credential::OAuth(_))),
            "the login still authenticates through its own token cell"
        );
        assert!(
            matches!(store.get(&anthropic.id), Some(Credential::ApiKey(k)) if k == "from-vault")
        );
    }

    /// A personal account and a workspace account can share one login email.
    /// Neither may shadow the other, and a refresh of one never reaches the
    /// other's token cell.
    #[test]
    fn two_accounts_on_one_email_stay_distinct() {
        let mut store = CredentialStore {
            ready: BTreeMap::new(),
            all: Vec::new(),
            admitted: BTreeSet::new(),
            environment: BTreeMap::new(),
        };

        let (personal, _) = store.add_oauth(&oauth_token("acc_personal", Some("alex@work")));
        assert_eq!(
            store.all[0].handle, "alex@work",
            "alone, the email names it"
        );

        let (team, team_credential) = store.add_oauth(&oauth_token("acc_team", Some("alex@work")));
        assert_ne!(
            personal.id, team.id,
            "distinct accounts, distinct identities"
        );
        assert_eq!(store.all.len(), 2, "neither shadows the other");

        let Credential::OAuth(team_cell) = team_credential else {
            panic!("expected an OAuth credential");
        };
        let personal_cell = match store.get(&personal.id) {
            Some(Credential::OAuth(cell)) => Arc::clone(cell),
            _ => panic!("the first account still resolves"),
        };
        assert!(
            !Arc::ptr_eq(&personal_cell, &team_cell),
            "each account authenticates through its own token cell"
        );

        // A refresh of one account never touches its sibling's cell.
        let refreshed_personal = OAuthToken {
            access_token: "fresh-at".into(),
            ..oauth_token("acc_personal", Some("alex@work"))
        };
        store.add_oauth(&refreshed_personal);
        assert_eq!(personal_cell.lock().unwrap().access_token, "fresh-at");
        assert_eq!(
            team_cell.lock().unwrap().access_token,
            "at",
            "the sibling's token is untouched by the other's refresh"
        );
    }
}
