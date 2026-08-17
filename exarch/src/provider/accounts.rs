//! Provider knowledge an embedding product needs about the accounts a
//! [`CredentialStore`] holds.
//!
//! Everything a product wants beyond what exarch's own CLI and TUI do:
//! declaring another endpoint, taking a key back, and finding one account
//! among the rest by its [`AccountId`]'s rendering.
//!
//! Nothing here is exarch- or synod-specific. What *is* product-specific —
//! where a declaration file lives, how a row is drawn, whether one is
//! offered for withdrawal — stays with the product that asked; this module
//! only ever answers "is this name available", "is this account a built-in",
//! and "which of the known accounts is this".

use super::credential::{CredentialStore, well_formed_key};
use super::identity::{self, Account, Auth, Billing, Service, ServiceName};

/// The endpoints declared beyond the built-in table.
///
/// Read off the live store rather than a file: the store is what the running
/// session believes, and a file edited by hand behind the product's back
/// should not be silently re-adopted by an unrelated save.
pub fn declared_endpoints(store: &CredentialStore) -> Vec<Service> {
    store
        .known()
        .iter()
        .filter(|account| identity::built_in(&account.service.name).is_none())
        .map(|account| account.service.clone())
        .collect()
}

/// Declare another endpoint to talk to: a name, an address, and the wire
/// protocol it speaks.
///
/// `label` opens the complaints, naming the settings file the declaration
/// came from, as it does throughout [`crate::config`].
///
/// Checked, but not yet persisted or admitted: where the declaration is
/// written down and how its key (if any) is kept are each the embedding
/// product's own business.
///
/// # Errors
/// Returns a plain sentence if the name is empty, colon-bearing, or
/// control-bearing ([`ServiceName::declared`]), if it is already taken by a
/// built-in or another declared endpoint, if the address does not look like
/// one, or if `protocol` names no adapter.
pub fn declare_endpoint(
    store: &CredentialStore,
    name: &str,
    endpoint: &str,
    protocol: &str,
    label: &str,
) -> Result<Service, String> {
    let name = ServiceName::declared(name.trim())?;
    if taken(store.known(), &name) {
        return Err(format!("There is already a service called {name}."));
    }
    let endpoint = well_formed_endpoint(endpoint)?;
    let adapter = crate::config::adapter_for_protocol(protocol, label)?;
    Ok(Service {
        name,
        endpoint: Some(endpoint),
        adapter,
        default_model: None,
        auth: Auth::Unnamed,
        billing: Billing::Metered,
        routes: false,
    })
}

/// Whether `name` is already spoken for — by the built-in table or by an
/// account already known under it — and so refused to a fresh declaration.
fn taken(known: &[Account], name: &ServiceName) -> bool {
    identity::built_in(name).is_some() || known.iter().any(|account| &account.service.name == name)
}

/// `endpoint`, trimmed and required to name a scheme, with the trailing
/// slash `genai` needs to join a service path onto it — supplying one is
/// kinder than refusing a typed-in address for lacking it.
fn well_formed_endpoint(endpoint: &str) -> Result<String, String> {
    let endpoint = endpoint.trim();
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err(format!(
            "'{endpoint}' does not look like an address — should it begin with https://?"
        ));
    }
    Ok(if endpoint.ends_with('/') {
        endpoint.to_string()
    } else {
        format!("{endpoint}/")
    })
}

/// Withdraw a declared endpoint from the live store entirely, out of every
/// map keyed by its account.
///
/// The declarations file and the vault are each the caller's to clear, before
/// or after; a built-in service refuses outright, since only its key can be
/// taken back, never the service.
///
/// # Errors
/// Returns a plain sentence if `id` names no known account, or one whose
/// service is a built-in rather than a declared endpoint.
pub fn withdraw_endpoint(store: &mut CredentialStore, id: &str) -> Result<(), String> {
    let account = find(store, id)?;
    refuse_built_in(&account)?;
    store.retire(&account.id);
    Ok(())
}

fn refuse_built_in(account: &Account) -> Result<(), String> {
    if identity::built_in(&account.service.name).is_some() {
        return Err(format!(
            "{} is a built-in service — its key can be taken back, but the \
             service itself cannot be removed.",
            account.service.name
        ));
    }
    Ok(())
}

/// A typed-in key as it will be kept, or a question about what was pasted.
///
/// Every door a key is typed at asks this — one screen, one rule — and it is
/// exarch's own well-formedness rule ([`well_formed_key`]), so a key refused
/// here is not one that would have been accepted from the environment.
///
/// # Errors
/// Returns a plain sentence, phrased as a question, naming what is wrong
/// with what was pasted. `label` names the account in that sentence — a
/// display label, not an id, since a human reads this.
pub fn checked_key(label: &str, key: &str) -> Result<String, String> {
    if key.trim().is_empty() {
        return Err(format!("No key was typed for {label} — paste it first?"));
    }
    well_formed_key(key).ok_or_else(|| {
        format!("That {label} key carries a line break — was more than the key copied?")
    })
}

/// The account whose id renders as `id`, whether or not it currently has a
/// credential.
///
/// Resolves by [`AccountId`](super::identity::AccountId)'s rendering alone —
/// never by a label a human might type, which two accounts can share. Every
/// caller reads `id` fresh off a store it just consulted, so a miss here
/// means the account vanished between that read and this one, not a typo to
/// explain kindly.
///
/// # Errors
/// Returns a plain sentence if no known account's id renders as `id`.
pub fn find(store: &CredentialStore, id: &str) -> Result<Account, String> {
    find_in(store.known(), id)
}

fn find_in(known: &[Account], id: &str) -> Result<Account, String> {
    known
        .iter()
        .find(|account| account.id.as_str() == id)
        .cloned()
        .ok_or_else(|| format!("no account '{id}' is known on this computer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::adapter::AdapterKind;

    fn declared(name: &str) -> Account {
        Account::of_service(Service {
            name: ServiceName::declared(name).unwrap(),
            endpoint: Some(format!("https://{name}.example/v1/")),
            adapter: AdapterKind::OpenAI,
            default_model: None,
            auth: Auth::Unnamed,
            billing: Billing::Metered,
            routes: false,
        })
    }

    #[test]
    fn a_built_in_name_is_already_taken() {
        let anthropic = ServiceName::declared("anthropic").unwrap();
        assert!(taken(&[], &anthropic));
    }

    #[test]
    fn a_declared_name_is_taken_only_once_declared() {
        let house = ServiceName::declared("house-llm").unwrap();
        assert!(!taken(&[], &house));
        assert!(taken(std::slice::from_ref(&declared("house-llm")), &house));
    }

    #[test]
    fn an_address_without_a_scheme_is_refused_as_a_question() {
        let err = well_formed_endpoint("llm.example/v1").unwrap_err();
        assert!(err.contains("https://"), "{err}");
    }

    #[test]
    fn an_address_gains_the_trailing_slash_genai_needs() {
        assert_eq!(
            well_formed_endpoint("https://llm.example/v1").unwrap(),
            "https://llm.example/v1/"
        );
        assert_eq!(
            well_formed_endpoint("https://llm.example/v1/").unwrap(),
            "https://llm.example/v1/"
        );
    }

    #[test]
    fn a_built_in_service_refuses_withdrawal() {
        let anthropic = Account::of_service(identity::built_in_services().remove(0));
        let err = refuse_built_in(&anthropic).unwrap_err();
        assert!(err.contains("built-in"), "{err}");
    }

    #[test]
    fn a_declared_endpoint_may_be_withdrawn() {
        refuse_built_in(&declared("house-llm")).expect("a declared endpoint is not built in");
    }

    #[test]
    fn find_in_resolves_by_id_rendering_alone() {
        let house = declared("house-llm");
        let found = find_in(std::slice::from_ref(&house), "house-llm").unwrap();
        assert_eq!(found.id, house.id);
        let err = find_in(std::slice::from_ref(&house), "missing").unwrap_err();
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn checked_key_asks_a_question_about_what_was_pasted() {
        assert!(
            checked_key("house-llm", "  ")
                .unwrap_err()
                .contains("paste it")
        );
        assert!(
            checked_key("house-llm", "sk-real\nGET /")
                .unwrap_err()
                .contains("line break")
        );
        assert_eq!(checked_key("house-llm", " sk-real ").unwrap(), "sk-real");
    }
}
