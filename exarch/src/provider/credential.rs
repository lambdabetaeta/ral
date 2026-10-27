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
//! Signed-in accounts ([`crate::provider::oauth`]) come from the token store,
//! one account per issuer identity. They are distinct from API-key accounts:
//! a login and an `OPENAI_API_KEY` are separately selectable accounts.

use crate::provider::identity::{self, Account, AccountId, Auth, Login, Service};
#[cfg(test)]
use crate::provider::oauth::OAuthToken;
use crate::provider::oauth::{self, CredentialHandle};
use std::collections::BTreeSet;
#[cfg(test)]
use std::sync::Arc;

/// The inert bearer bound to a keyless declared endpoint.
///
/// Ollama and its kin ignore `Authorization` entirely, but `genai`'s `OpenAI`
/// adapter always attaches one; `pub` so `tests/credential_env.rs` can name
/// it.
pub const NO_AUTH_PLACEHOLDER: &str = "no-auth";

/// A resolved credential — what an account's requests authenticate with.
///
/// OAuth credentials share an owner; only the OAuth transaction boundary
/// can publish or retire them. Cached providers read its current snapshot.
#[derive(Clone)]
pub enum Credential {
    ApiKey(String),
    OAuth(CredentialHandle),
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
    entries: Vec<AccountEntry>,
    #[cfg(test)]
    test_state: Option<Arc<ral_core::own::TestDir>>,
}

struct AccountEntry {
    account: Account,
    auth: AccountAuth,
}

enum AccountAuth {
    Key {
        environment: Option<String>,
        vault: Option<String>,
    },
    OAuth(CredentialHandle),
    Keyless,
}

impl AccountEntry {
    fn declared(account: Account) -> Self {
        let auth = match &account.service.auth {
            Auth::Key(_) => AccountAuth::Key {
                environment: None,
                vault: None,
            },
            Auth::Keyless => AccountAuth::Keyless,
            Auth::OAuth(_) => unreachable!("logins must be admitted with their committed owner"),
        };
        Self { account, auth }
    }

    fn credential(&self) -> Option<Credential> {
        match &self.auth {
            AccountAuth::Key { environment, vault } => environment
                .as_ref()
                .or(vault.as_ref())
                .cloned()
                .map(Credential::ApiKey),
            AccountAuth::OAuth(login) => {
                (!login.is_retired()).then(|| Credential::OAuth(login.clone()))
            }
            AccountAuth::Keyless => Some(Credential::ApiKey(NO_AUTH_PLACEHOLDER.into())),
        }
    }
}

impl CredentialStore {
    /// Sweep every built-in and declared service's key variable, then scrub
    /// every one that was *present* — the malformed one too, since a value
    /// with a pasted newline is still a live secret for a child to inherit.
    /// The process is sealed first (`seal`), and each variable erased (`erase`).
    /// Signed-in accounts are folded in from the token store; a store that
    /// cannot be read warns on stderr and offers no logins.
    ///
    /// SAFETY: call this while the process is still single-threaded (before any
    /// session worker thread exists), so the env scrub races nothing.
    pub fn resolve_and_scrub(declared: Vec<Service>) -> Self {
        seal();
        let mut entries: Vec<AccountEntry> = identity::built_in_services()
            .into_iter()
            .filter(|service| !matches!(service.auth, Auth::OAuth(_)))
            .chain(declared)
            .map(|service| AccountEntry::declared(Account::of_service(service)))
            .collect();
        let mut scrub = BTreeSet::new();
        for entry in &mut entries {
            if let Auth::Key(var) = &entry.account.service.auth
                && let Some(raw) = ral_core::host::var(var)
                && let AccountAuth::Key { environment, .. } = &mut entry.auth
            {
                scrub.insert(var.clone());
                *environment = well_formed_key(&raw);
            }
        }
        for var in &scrub {
            // SAFETY: single-threaded startup, per this function's contract.
            unsafe { erase(var) };
        }

        let mut store = Self {
            entries,
            #[cfg(test)]
            test_state: None,
        };
        match oauth::credentials() {
            Ok(logins) => {
                for login in logins {
                    let _ = store.add_login(&login);
                }
            }
            Err(error) => ral_core::errln!(
                "warning: no signed-in accounts were loaded, so none are offered: {error}"
            ),
        }
        store
    }

    /// What `id` authenticates with, or `None` when it is not available.
    pub fn get(&self, id: &AccountId) -> Option<Credential> {
        self.entries
            .iter()
            .find(|entry| &entry.account.id == id)
            .and_then(AccountEntry::credential)
    }

    pub fn is_available(&self, id: &AccountId) -> bool {
        self.get(id).is_some()
    }

    /// The available accounts, in [`Self::known`]'s order.
    pub fn available(&self) -> Vec<Account> {
        self.entries
            .iter()
            .filter(|entry| entry.credential().is_some())
            .map(|entry| entry.account.clone())
            .collect()
    }

    /// Every account this store knows of, available or not — an account with
    /// no key yet is precisely the one an accounts screen exists for.
    pub fn known(&self) -> impl ExactSizeIterator<Item = &Account> + Clone {
        self.entries.iter().map(|entry| &entry.account)
    }

    /// The key the launch environment supplied for `id`.
    pub fn from_environment(&self, id: &AccountId) -> Option<&str> {
        self.entries.iter().find_map(|entry| match &entry.auth {
            AccountAuth::Key { environment, .. } if &entry.account.id == id => {
                environment.as_deref()
            }
            _ => None,
        })
    }

    /// The key the vault holds for `id`, whether or not it is in force.
    pub fn from_vault(&self, id: &AccountId) -> Option<&str> {
        self.entries.iter().find_map(|entry| match &entry.auth {
            AccountAuth::Key { vault, .. } if &entry.account.id == id => vault.as_deref(),
            _ => None,
        })
    }

    /// Attach the committed owner, never a delayed raw token snapshot.
    ///
    /// # Errors
    /// The account was signed out before admission.
    pub fn add_login(&mut self, login: &CredentialHandle) -> Result<Account, String> {
        if login.is_retired() {
            return Err(
                "this login was signed out before it could be admitted; sign in again".into(),
            );
        }
        let snapshot = login.snapshot();
        let account = oauth::to_account(&snapshot);
        // Re-placed under its current handle: an arrival takes its spot among
        // the other logins, and a refresh that has finally learned the email
        // or plan claim moves there without disturbing its identity.
        self.entries.retain(|entry| entry.account.id != account.id);
        self.insert_login(
            snapshot.login(),
            AccountEntry {
                account: account.clone(),
                auth: AccountAuth::OAuth(login.clone()),
            },
        );
        Ok(account)
    }

    #[cfg(test)]
    fn add_oauth(&mut self, token: &OAuthToken) -> Account {
        let state = self.test_state.get_or_insert_with(|| {
            Arc::new(ral_core::own::OwnDir::for_test("credentials").unwrap())
        });
        let login = oauth::commit_for_test(state, token);
        self.add_login(&login).unwrap()
    }

    /// Know `account` from now on — a declared endpoint's arrival.
    pub fn declare(&mut self, account: Account) {
        if !self
            .entries
            .iter()
            .any(|entry| entry.account.id == account.id)
        {
            self.entries.push(AccountEntry::declared(account));
        }
    }

    /// The vault now holds `key` for a known key-bearing account.
    pub fn save_key(&mut self, id: &AccountId, key: String) {
        if let Some(AccountEntry {
            auth: AccountAuth::Key { vault, .. },
            ..
        }) = self
            .entries
            .iter_mut()
            .find(|entry| &entry.account.id == id)
        {
            *vault = Some(key);
        }
    }

    /// The vault no longer holds a key for `id`; the environment's, if any,
    /// is untouched.
    pub fn forget_key(&mut self, id: &AccountId) {
        if let Some(AccountEntry {
            auth: AccountAuth::Key { vault, .. },
            ..
        }) = self
            .entries
            .iter_mut()
            .find(|entry| &entry.account.id == id)
        {
            *vault = None;
        }
    }

    pub(crate) fn retire_login(&mut self, id: &AccountId) {
        if self.entries.iter().any(|entry| {
            &entry.account.id == id
                && matches!(&entry.auth, AccountAuth::OAuth(login) if login.is_retired())
        }) {
            self.retire(id);
        }
    }

    /// Drop `id` entirely: a withdrawn declaration, or a signed-out login.
    pub fn retire(&mut self, id: &AccountId) {
        self.entries.retain(|entry| &entry.account.id != id);
    }

    /// Read `vault`'s key for every account that takes one. An ordinary second
    /// call after [`Self::resolve_and_scrub`], which keeps that function's
    /// SAFETY contract its own.
    pub fn read_vault(&mut self, vault: &impl SecretVault) {
        for entry in &mut self.entries {
            if let AccountAuth::Key { vault: saved, .. } = &mut entry.auth {
                *saved = vault.read(&entry.account);
            }
        }
    }

    /// Where a login belongs among the accounts already known: after every
    /// built-in key-bearing service, in login/handle/id order, before the
    /// first declared service.
    fn insert_login(&mut self, login: Login, entry: AccountEntry) {
        let account = &entry.account;
        let pos = self
            .entries
            .iter()
            .map(|entry| &entry.account)
            .position(|other| match &other.service.auth {
                Auth::OAuth(other_login) => {
                    (*other_login, &other.handle, &other.id) > (login, &account.handle, &account.id)
                }
                _ => identity::built_in(&other.service.name).is_none(),
            })
            .unwrap_or(self.entries.len());
        self.entries.insert(pos, entry);
    }

    /// This store's available accounts and credentials as a [`Roster`].
    pub fn roster(&self) -> Roster {
        Roster {
            entries: self
                .entries
                .iter()
                .filter_map(|entry| Some((entry.account.clone(), entry.credential()?)))
                .collect(),
        }
    }
}

/// Every available account and its resolved credential, cloned out of the
/// store at one instant — the form a background thread can hold.
///
/// OAuth owners stay shared, so committed refresh and retirement are live here.
#[derive(Clone, Default)]
pub struct Roster {
    entries: Vec<(Account, Credential)>,
}

impl Roster {
    pub fn account(&self, id: &AccountId) -> Option<&Account> {
        self.entries
            .iter()
            .find(|(account, _)| &account.id == id)
            .map(|(account, _)| account)
    }

    pub fn credential(&self, id: &AccountId) -> Option<&Credential> {
        self.entries
            .iter()
            .find(|(account, _)| &account.id == id)
            .map(|(_, credential)| credential)
    }

    /// The one place a label naming an account in an error has the full set to
    /// disambiguate against.
    pub fn label(&self, account: &Account) -> String {
        identity::label(account, &self.accounts().cloned().collect::<Vec<_>>())
    }

    pub fn accounts(&self) -> impl ExactSizeIterator<Item = &Account> + Clone {
        self.entries.iter().map(|(account, _)| account)
    }
}

/// A key as it will actually be used, or `None` when it authenticates
/// nothing: blank, or carrying a control character — a newline copied along
/// with the key being the usual way.
///
/// One rule wherever a key arrives, so a secret refused on one route is not
/// quietly accepted at another: the environment here, a window, or the
/// computer's credential manager ([`crate::provider::keychain`]).
pub fn well_formed_key(raw: &str) -> Option<String> {
    let key = raw.trim();
    (!key.is_empty() && !key.bytes().any(|b| b < 0x20 || b == 0x7f)).then(|| key.to_string())
}

/// Make this process non-dumpable, so its `/proc/<pid>/{environ,mem,maps}`
/// belong to root and no same-uid process, itself included, can read the keys
/// it holds. `execve` restores dumpability, so children are unaffected.
#[cfg(target_os = "linux")]
fn seal() {
    // SAFETY: changes only this process's dumpable flag.
    let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    debug_assert_eq!(rc, 0, "PR_SET_DUMPABLE is infallible with valid arguments");
}

#[cfg(not(target_os = "linux"))]
fn seal() {}

/// Unset `var`, overwriting its value first: `unsetenv` drops only libc's
/// pointer, while `/proc/<pid>/environ` (Linux) and `KERN_PROCARGS2` (macOS)
/// read the launch bytes themselves.
///
/// SAFETY: no other thread may touch the environment.
#[cfg(unix)]
unsafe fn erase(var: &str) {
    let name = std::ffi::CString::new(var).expect("a variable `std::env::var` read holds no NUL");
    // SAFETY: per this function's contract.
    let value = unsafe { libc::getenv(name.as_ptr()) };
    if !value.is_null() {
        // SAFETY: POSIX forbids writing through `getenv`'s result only because
        // libc may share it; nothing here reads it again, `unsetenv` follows at
        // once, and the bytes are writable: the `execve` stack or libc's own copy.
        unsafe {
            let len = std::ffi::CStr::from_ptr(value).count_bytes();
            std::ptr::write_bytes(value, 0, len);
        }
    }
    // SAFETY: per this function's contract.
    unsafe { std::env::remove_var(var) };
}

#[cfg(not(unix))]
unsafe fn erase(var: &str) {
    // SAFETY: per this function's contract.
    unsafe { std::env::remove_var(var) };
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use genai::adapter::AdapterKind;
    use identity::{Login, ServiceName};

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
        login_token(Login::Chatgpt, issued, email)
    }

    fn login_token(login: Login, issued: &str, email: Option<&str>) -> OAuthToken {
        let grant = match login {
            Login::Chatgpt => oauth::Grant::Chatgpt {
                id_token: "id".into(),
                client_id: "oaiapp_test".into(),
            },
            Login::Codex => oauth::Grant::Codex {
                account_id: "raw-workspace".into(),
                workspace: None,
                plan: None,
            },
            Login::Grok => oauth::Grant::Grok,
        };
        OAuthToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            issued: issued.into(),
            email: email.map(str::to_string),
            expires_at: 0,
            grant,
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

    /// One rule on every route: surrounding whitespace is not part of a key,
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
            entries: vec![AccountEntry {
                account: anthropic.clone(),
                auth: AccountAuth::Key {
                    environment: Some("from-env".into()),
                    vault: None,
                },
            }],
            ..CredentialStore::default()
        };

        store.read_vault(&StickyVault);
        assert_eq!(
            api_key(store.get(&anthropic.id)).as_deref(),
            Some("from-env")
        );
        assert_eq!(store.from_environment(&anthropic.id), Some("from-env"));
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
        assert!(store.known().eq([&house]));
    }

    /// A server that checks no key is available as declared, and a vault
    /// entry under its name changes nothing.
    #[test]
    fn a_keyless_endpoint_ignores_the_vault() {
        let ollama = keyless("ollama");
        let mut store = CredentialStore::default();
        store.declare(ollama.clone());
        store.save_key(&ollama.id, "ignored".into());
        store.read_vault(&StickyVault);
        assert_eq!(
            api_key(store.get(&ollama.id)).as_deref(),
            Some(NO_AUTH_PLACEHOLDER)
        );
        assert_eq!(store.from_vault(&ollama.id), None);
    }

    #[test]
    fn retiring_and_redeclaring_an_account_drops_its_key() {
        let account = Account::declared("house-llm");
        let mut store = CredentialStore::default();
        store.declare(account.clone());
        store.save_key(&account.id, "saved".into());
        store.declare(account.clone());
        assert!(store.known().eq([&account]));
        assert_eq!(store.from_vault(&account.id), Some("saved"));

        let roster = store.roster();
        assert!(roster.accounts().eq([&account]));
        assert_eq!(roster.account(&account.id), Some(&account));
        store.retire(&account.id);
        assert_eq!(store.known().len(), 0);
        assert_eq!(store.roster().accounts().len(), 0);
        store.declare(account.clone());
        assert!(store.get(&account.id).is_none());
        assert!(store.from_vault(&account.id).is_none());
        assert!(matches!(
            roster.credential(&account.id),
            Some(Credential::ApiKey(key)) if key == "saved"
        ));
    }

    /// A vault answers for keys; even one claiming an entry for every account
    /// must never shadow a login's token cell.
    #[test]
    fn read_vault_never_touches_a_signed_in_login() {
        let mut store = CredentialStore::default();
        for login in Login::ALL {
            let account = store.add_oauth(&login_token(login, "same-id", Some("alex@work")));
            store.read_vault(&StickyVault);
            assert!(matches!(store.get(&account.id), Some(Credential::OAuth(_))));
            assert_eq!(store.from_vault(&account.id), None);
        }
    }

    #[test]
    fn add_oauth_new_account_sorts_after_built_in_and_before_declared() {
        let anthropic = Account::built_in("anthropic");
        let llama = keyless("local-llama");
        let mut store = CredentialStore {
            entries: vec![
                AccountEntry {
                    account: anthropic.clone(),
                    auth: AccountAuth::Key {
                        environment: Some("key".into()),
                        vault: None,
                    },
                },
                AccountEntry::declared(llama.clone()),
            ],
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
            "built-in, then logins by handle, then declared"
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
            cell.snapshot().access_token,
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
        assert_eq!(store.known().next().unwrap().handle, "acc_1");

        let new = store.add_oauth(&oauth_token("acc_1", Some("alex@work")));

        assert_eq!(new.id, old.id, "it is the same account throughout");
        assert_eq!(store.known().len(), 1, "renamed, not duplicated");
        assert_eq!(store.known().next().unwrap().handle, "alex@work");
    }

    /// A personal account and a workspace account can share one login email.
    /// Neither may shadow the other, and a refresh of one never reaches the
    /// other's token cell.
    #[test]
    fn two_accounts_on_one_email_stay_distinct() {
        let mut store = CredentialStore::default();

        let personal = store.add_oauth(&oauth_token("acc_personal", Some("alex@work")));
        assert_eq!(
            store.known().next().unwrap().handle,
            "alex@work",
            "alone, the email names it"
        );

        let team = store.add_oauth(&oauth_token("acc_team", Some("alex@work")));
        assert_ne!(
            personal.id, team.id,
            "distinct accounts, distinct identities"
        );
        assert_eq!(store.known().len(), 2, "neither shadows the other");

        let cell = |id| match store.get(id) {
            Some(Credential::OAuth(cell)) => cell,
            _ => panic!("expected an OAuth credential"),
        };
        let (personal_cell, team_cell) = (cell(&personal.id), cell(&team.id));
        assert!(
            !personal_cell.shares_owner(&team_cell),
            "each account authenticates through its own token cell"
        );

        // A refresh of one account never touches its sibling's cell.
        store.add_oauth(&OAuthToken {
            access_token: "fresh-at".into(),
            ..oauth_token("acc_personal", Some("alex@work"))
        });
        assert_eq!(personal_cell.snapshot().access_token, "fresh-at");
        assert_eq!(
            team_cell.snapshot().access_token,
            "at",
            "the sibling's token is untouched by the other's refresh"
        );
    }

    #[test]
    fn six_accounts_sort_by_login_handle_and_id_and_keep_independent_cells() {
        let keyed = Account::built_in("anthropic");
        let declared = keyless("local-llama");
        let mut store = CredentialStore {
            entries: vec![
                AccountEntry {
                    account: keyed.clone(),
                    auth: AccountAuth::Key {
                        environment: Some("key".into()),
                        vault: None,
                    },
                },
                AccountEntry::declared(declared.clone()),
            ],
            ..CredentialStore::default()
        };
        let mut expected = vec![keyed.id];
        for login in Login::ALL.into_iter().rev() {
            for issued in ["b", "a"] {
                store.add_oauth(&login_token(login, issued, Some("same@work")));
            }
        }
        for login in Login::ALL {
            for issued in ["a", "b"] {
                expected.push(Account::login(login, issued, "same@work").id);
            }
        }
        expected.push(declared.id);
        let ids = || {
            store
                .available()
                .into_iter()
                .map(|account| account.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(), expected);

        let cells: Vec<_> = expected[1..7]
            .iter()
            .map(|id| match store.get(id) {
                Some(Credential::OAuth(cell)) => cell,
                _ => panic!("expected an independent login cell"),
            })
            .collect();
        for (index, cell) in cells.iter().enumerate() {
            for other in &cells[index + 1..] {
                assert!(!cell.shares_owner(other));
            }
        }
        let roster = store.roster();
        let mut replacement = login_token(Login::Codex, "a", Some("same@work"));
        replacement.access_token = "codex-refreshed".into();
        store.add_oauth(&replacement);
        assert_eq!(
            store
                .available()
                .into_iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            expected
        );
        for (index, cell) in cells.iter().enumerate() {
            assert_eq!(
                cell.snapshot().access_token,
                if index == 2 { "codex-refreshed" } else { "at" },
            );
            let Some(Credential::OAuth(snapshot)) = roster.credential(&expected[index + 1]) else {
                panic!("the earlier roster retains its cell");
            };
            assert!(cell.shares_owner(snapshot));
        }

        replacement.email = Some("aaa@work".into());
        store.add_oauth(&replacement);
        assert_eq!(store.known().nth(3).unwrap().id, expected[3]);
        replacement.email = Some("zzz@work".into());
        store.add_oauth(&replacement);
        assert_eq!(store.known().nth(4).unwrap().id, expected[3]);
        store.retire(&expected[3]);
        assert_eq!(
            store
                .entries
                .iter()
                .filter(|entry| matches!(entry.auth, AccountAuth::OAuth(_)))
                .count(),
            5
        );
        for id in expected[1..7].iter().filter(|id| **id != expected[3]) {
            assert!(matches!(store.get(id), Some(Credential::OAuth(_))));
        }
    }
}
