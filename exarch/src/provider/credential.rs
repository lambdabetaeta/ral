//! The credentials each account authenticates with.
//!
//! A key comes from two layers, the launch environment's outranking the
//! vault's: a variable set for one run is the narrower and more deliberate
//! act. Each key variable is read once at startup and then scrubbed, so no
//! child a tool call spawns inherits a live key; the scrub is also why that
//! layer never changes afterwards. The vault, the computer's credential
//! manager ([`crate::provider::keychain`]), is read just after, and changes as
//! an accounts screen saves and forgets keys. A keyless declared endpoint
//! authenticates with [`NO_AUTH_PLACEHOLDER`].
//!
//! Signed-in `ChatGPT` accounts ([`crate::provider::oauth`]) come from the
//! token store, one account per login, and are identities distinct from an
//! API-key `openai` account: a login and an `OPENAI_API_KEY` are two
//! selectable accounts, not one.

use crate::provider::identity::{self, Account, AccountId, Auth, Service};
use crate::provider::oauth::{self, OAuthToken};
use ral_core::sync::LockExt;
use std::collections::BTreeMap;
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
    OAuth(Arc<Mutex<OAuthToken>>),
}

/// A place a running product keeps typed-in secrets.
///
/// The platform credential manager, implemented by
/// [`crate::provider::keychain::Keychain`].
pub trait SecretVault {
    fn read(&self, account: &Account) -> Option<String>;
}

/// Every known account, and the layers its credential is drawn from.
#[derive(Default)]
pub struct CredentialStore {
    /// Every known account, available or not — the one owner of the account
    /// records; every map below names one by its [`AccountId`] alone.
    all: Vec<Account>,
    /// What the launch environment supplied.
    environment: BTreeMap<AccountId, String>,
    /// What the vault holds, beneath the environment.
    vault: BTreeMap<AccountId, String>,
    /// Each login's token, in the cell a live provider refreshes in place.
    logins: BTreeMap<AccountId, Arc<Mutex<OAuthToken>>>,
}

impl CredentialStore {
    /// Sweep every built-in and declared service's key variable, then scrub
    /// every one that was *present* — the malformed one too, since a value
    /// with a pasted newline is still a live secret for a child to inherit.
    /// `ChatGPT` logins are folded in from the token store.
    ///
    /// SAFETY: call this while the process is still single-threaded (before any
    /// session worker thread exists), so the env scrub races nothing.
    #[allow(
        clippy::disallowed_methods,
        reason = "the one read of the key variables, and their scrub"
    )]
    pub fn resolve_and_scrub(declared: Vec<Service>) -> Self {
        let all: Vec<Account> = identity::built_in_services()
            .into_iter()
            .filter(|service| service.auth != Auth::OAuth)
            .chain(declared)
            .map(Account::of_service)
            .collect();
        let mut environment = BTreeMap::new();
        let mut scrub = Vec::new();
        for account in &all {
            if let Auth::Key(var) = &account.service.auth
                && let Ok(raw) = std::env::var(var)
            {
                scrub.push(var.clone());
                if let Some(key) = well_formed_key(&raw) {
                    environment.insert(account.id.clone(), key);
                }
            }
        }
        scrub.sort_unstable();
        scrub.dedup();
        for var in &scrub {
            // SAFETY: single-threaded startup, per this function's contract.
            unsafe { std::env::remove_var(var) };
        }

        let mut store = Self {
            all,
            environment,
            ..Self::default()
        };
        for token in oauth::load_all() {
            store.add_oauth(&token);
        }
        store
    }

    /// What `account` authenticates with now, if anything.
    fn credential(&self, account: &Account) -> Option<Credential> {
        let id = &account.id;
        match &account.service.auth {
            Auth::Key(_) => self
                .environment
                .get(id)
                .or_else(|| self.vault.get(id))
                .cloned()
                .map(Credential::ApiKey),
            Auth::OAuth => self.logins.get(id).cloned().map(Credential::OAuth),
            Auth::Keyless => Some(Credential::ApiKey(NO_AUTH_PLACEHOLDER.into())),
        }
    }

    /// What `id` authenticates with, or `None` when it is not available.
    pub fn get(&self, id: &AccountId) -> Option<Credential> {
        self.all
            .iter()
            .find(|account| &account.id == id)
            .and_then(|account| self.credential(account))
    }

    pub fn is_available(&self, id: &AccountId) -> bool {
        self.get(id).is_some()
    }

    /// The available accounts, in [`Self::known`]'s order.
    pub fn available(&self) -> Vec<Account> {
        self.all
            .iter()
            .filter(|account| self.credential(account).is_some())
            .cloned()
            .collect()
    }

    /// Every account this store knows of, available or not — an account with
    /// no key yet is precisely the one an accounts screen exists for.
    pub fn known(&self) -> &[Account] {
        &self.all
    }

    /// The key the launch environment supplied for `id`.
    pub fn from_environment(&self, id: &AccountId) -> Option<&str> {
        self.environment.get(id).map(String::as_str)
    }

    /// The key the vault holds for `id`, whether or not it is in force.
    pub fn from_vault(&self, id: &AccountId) -> Option<&str> {
        self.vault.get(id).map(String::as_str)
    }

    /// Admit a signed-in login. A re-login writes the fresh token into the
    /// *existing* cell — the one a live provider authenticates through — so
    /// it takes effect with no provider rebuild, and stays the same account
    /// however its handle changes.
    pub fn add_oauth(&mut self, token: &OAuthToken) -> Account {
        let account = oauth::to_account(token);
        self.logins
            .entry(account.id.clone())
            .and_modify(|cell| *cell.lock_ignore_poison() = token.clone())
            .or_insert_with(|| Arc::new(Mutex::new(token.clone())));
        // Re-placed under its current handle: an arrival takes its spot among
        // the other logins, and a refresh that has finally learned the email
        // or plan claim moves there without disturbing its identity.
        self.all.retain(|listed| listed.id != account.id);
        self.insert_login(account.clone());
        account
    }

    /// Know `account` from now on — a declared endpoint's arrival.
    pub fn declare(&mut self, account: Account) {
        if !self.all.iter().any(|known| known.id == account.id) {
            self.all.push(account);
        }
    }

    /// The vault now holds `key` for `id`.
    pub fn save_key(&mut self, id: &AccountId, key: String) {
        self.vault.insert(id.clone(), key);
    }

    /// The vault no longer holds a key for `id`; the environment's, if any,
    /// is untouched.
    pub fn forget_key(&mut self, id: &AccountId) {
        self.vault.remove(id);
    }

    /// Drop `id` entirely: a withdrawn declaration, or a signed-out login.
    pub fn retire(&mut self, id: &AccountId) {
        self.environment.remove(id);
        self.vault.remove(id);
        self.logins.remove(id);
        self.all.retain(|known| &known.id != id);
    }

    /// Read `vault`'s key for every account that takes one. An ordinary second
    /// call after [`Self::resolve_and_scrub`], which keeps that function's
    /// SAFETY contract its own.
    pub fn read_vault(&mut self, vault: &impl SecretVault) {
        self.vault = self
            .all
            .iter()
            .filter(|account| matches!(account.service.auth, Auth::Key(_)))
            .filter_map(|account| Some((account.id.clone(), vault.read(account)?)))
            .collect();
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

    /// This store's available accounts and credentials as a [`Roster`].
    pub fn roster(&self) -> Roster {
        let (accounts, credentials) = self
            .all
            .iter()
            .filter_map(|account| {
                let credential = self.credential(account)?;
                Some((account.clone(), (account.id.clone(), credential)))
            })
            .unzip();
        Roster {
            accounts,
            credentials,
        }
    }
}

/// Every available account and its resolved credential, cloned out of the
/// store at one instant — the form a background thread can hold.
///
/// OAuth cells stay shared, so a token refreshed anywhere is visible here.
#[derive(Clone, Default)]
pub struct Roster {
    accounts: Vec<Account>,
    credentials: BTreeMap<AccountId, Credential>,
}

impl Roster {
    pub fn account(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.iter().find(|account| &account.id == id)
    }

    pub fn credential(&self, id: &AccountId) -> Option<&Credential> {
        self.credentials.get(id)
    }

    /// The one place a label naming an account in an error has the full set to
    /// disambiguate against.
    pub fn label(&self, account: &Account) -> String {
        identity::label(account, &self.accounts)
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }
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
    use genai::adapter::AdapterKind;
    use identity::ServiceName;

    // The `resolve_and_scrub` scenarios live in `tests/credential_env.rs`: they
    // mutate the process-global environment, which no library test may share.

    fn keyless(name: &str) -> Account {
        Account::of_service(Service::declared(
            ServiceName::declared(name).unwrap(),
            format!("http://{name}/v1/"),
            AdapterKind::OpenAI,
            Auth::Keyless,
        ))
    }

    fn oauth_token(issued: &str, email: Option<&str>) -> OAuthToken {
        OAuthToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token: "id".into(),
            client_id: "oaiapp_test".into(),
            issued: issued.into(),
            email: email.map(str::to_string),
            expires_at: 0,
        }
    }

    fn api_key(credential: Option<Credential>) -> Option<String> {
        match credential? {
            Credential::ApiKey(key) => Some(key),
            Credential::OAuth(_) => None,
        }
    }

    /// Answers for everything — the misconfigured vault `read_vault` must
    /// stay guarded against.
    struct StickyVault;
    impl SecretVault for StickyVault {
        fn read(&self, _account: &Account) -> Option<String> {
            Some("from-vault".into())
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

    /// A variable set for this run outranks the vault's standing key, and
    /// forgetting the vault's leaves it in force.
    #[test]
    fn the_environment_outranks_the_vault() {
        let anthropic = Account::built_in("anthropic");
        let mut store = CredentialStore {
            all: vec![anthropic.clone()],
            environment: BTreeMap::from([(anthropic.id.clone(), "from-env".into())]),
            ..CredentialStore::default()
        };

        store.read_vault(&StickyVault);
        assert_eq!(
            api_key(store.get(&anthropic.id)).as_deref(),
            Some("from-env")
        );
        assert_eq!(store.from_vault(&anthropic.id), Some("from-vault"));

        store.forget_key(&anthropic.id);
        assert_eq!(
            api_key(store.get(&anthropic.id)).as_deref(),
            Some("from-env")
        );
    }

    /// With no variable set, the vault's key is in force until forgotten, and
    /// the account then stays known but unavailable.
    #[test]
    fn a_saved_key_alone_is_in_force_until_forgotten() {
        let house = Account::declared("house-llm");
        let mut store = CredentialStore::default();
        store.declare(house.clone());
        store.save_key(&house.id, "typed".into());
        assert_eq!(api_key(store.get(&house.id)).as_deref(), Some("typed"));

        store.forget_key(&house.id);
        assert!(store.get(&house.id).is_none());
        assert_eq!(store.known(), [house]);
    }

    /// A server that checks no key is available as declared, and a vault
    /// entry under its name changes nothing.
    #[test]
    fn a_keyless_endpoint_ignores_the_vault() {
        let ollama = keyless("ollama");
        let mut store = CredentialStore::default();
        store.declare(ollama.clone());
        store.read_vault(&StickyVault);
        assert_eq!(
            api_key(store.get(&ollama.id)).as_deref(),
            Some(NO_AUTH_PLACEHOLDER)
        );
        assert_eq!(store.from_vault(&ollama.id), None);
    }

    /// A vault answers for keys; even one claiming an entry for every account
    /// must never shadow a login's token cell.
    #[test]
    fn read_vault_never_touches_a_signed_in_login() {
        let mut store = CredentialStore::default();
        let login = store.add_oauth(&oauth_token("acc_1", Some("alex@work")));
        store.read_vault(&StickyVault);
        assert!(matches!(store.get(&login.id), Some(Credential::OAuth(_))));
    }

    #[test]
    fn add_oauth_new_account_sorts_after_built_in_and_before_declared() {
        let anthropic = Account::built_in("anthropic");
        let llama = keyless("local-llama");
        let mut store = CredentialStore {
            all: vec![anthropic.clone(), llama.clone()],
            environment: BTreeMap::from([(anthropic.id.clone(), "key".into())]),
            ..CredentialStore::default()
        };

        let bravo = store.add_oauth(&oauth_token("acc_b", Some("bravo@work")));
        let alpha = store.add_oauth(&oauth_token("acc_a", Some("alpha@work")));

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
        let mut store = CredentialStore::default();
        let account = store.add_oauth(&oauth_token("acc_1", Some("alex@work")));
        let Some(Credential::OAuth(cell)) = store.get(&account.id) else {
            panic!("expected an OAuth credential");
        };
        let before = store.available();

        let refreshed = store.add_oauth(&OAuthToken {
            access_token: "fresh-at".into(),
            ..oauth_token("acc_1", Some("alex@work"))
        });

        assert_eq!(refreshed.id, account.id);
        assert_eq!(store.available(), before, "a re-login adds no new account");
        assert_eq!(
            cell.lock().unwrap().access_token,
            "fresh-at",
            "the pre-existing cell (the one a live provider reads through) sees the refresh"
        );
    }

    /// An email claim arriving on re-login updates the handle without
    /// disturbing the account's identity.
    #[test]
    fn add_oauth_relogin_with_a_new_claim_renames_the_handle_in_place() {
        let mut store = CredentialStore::default();

        let old = store.add_oauth(&oauth_token("acc_1", None));
        assert_eq!(store.all[0].handle, "acc_1");

        let new = store.add_oauth(&oauth_token("acc_1", Some("alex@work")));

        assert_eq!(new.id, old.id, "it is the same account throughout");
        assert_eq!(store.all.len(), 1, "renamed, not duplicated");
        assert_eq!(store.all[0].handle, "alex@work");
    }

    /// A personal account and a workspace account can share one login email.
    /// Neither may shadow the other, and a refresh of one never reaches the
    /// other's token cell.
    #[test]
    fn two_accounts_on_one_email_stay_distinct() {
        let mut store = CredentialStore::default();

        let personal = store.add_oauth(&oauth_token("acc_personal", Some("alex@work")));
        assert_eq!(
            store.all[0].handle, "alex@work",
            "alone, the email names it"
        );

        let team = store.add_oauth(&oauth_token("acc_team", Some("alex@work")));
        assert_ne!(
            personal.id, team.id,
            "distinct accounts, distinct identities"
        );
        assert_eq!(store.all.len(), 2, "neither shadows the other");

        let cell = |id| match store.get(id) {
            Some(Credential::OAuth(cell)) => cell,
            _ => panic!("expected an OAuth credential"),
        };
        let (personal_cell, team_cell) = (cell(&personal.id), cell(&team.id));
        assert!(
            !Arc::ptr_eq(&personal_cell, &team_cell),
            "each account authenticates through its own token cell"
        );

        // A refresh of one account never touches its sibling's cell.
        store.add_oauth(&OAuthToken {
            access_token: "fresh-at".into(),
            ..oauth_token("acc_personal", Some("alex@work"))
        });
        assert_eq!(personal_cell.lock().unwrap().access_token, "fresh-at");
        assert_eq!(
            team_cell.lock().unwrap().access_token,
            "at",
            "the sibling's token is untouched by the other's refresh"
        );
    }
}
