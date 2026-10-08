//! The accounts screen's commands.
//!
//! Every command here ends as a sign-in does — with a `models-refreshed`
//! event carrying the live picker — so the window has exactly one path by
//! which the assistant menu ever changes.  The reply is the fresh account
//! list itself, so the screen redraws from what the store now holds rather
//! than from what it hoped the command did.
//!
//! Keys travel one way only: from the window into [`synod::accounts`] and
//! the credential manager.  What comes back is a four-character tail and
//! where it is kept, so a window left open holds no secret.
//!
//! Every command naming an *existing* account takes its id, never its
//! label: two accounts can share a display label, and resolving one by name
//! is exactly the ambiguity [`exarch::provider::identity::AccountId`] exists
//! to rule out. Declaring a *new* endpoint is the one exception — there is
//! no account yet, only the name it is to be given.

use exarch::provider::Holdings;
use synod::accounts::{self, AccountList};
use tauri::{AppHandle, State};

/// Every service synod knows, with or without a key.
///
/// # Errors
/// Returns the credential resolution's own failure, if startup could not
/// read this computer's accounts at all.
#[tauri::command]
pub fn list_accounts(accounts_state: State<'_, super::Accounts>) -> Result<AccountList, String> {
    let Holdings { store, .. } = accounts_state.resolved()?;
    Ok(accounts::list(store))
}

/// Keep `key` for the account named `account`.
///
/// # Errors
/// Returns a plain sentence if the account is unknown, the key is
/// malformed, or the credential manager would not keep it.
#[tauri::command]
pub fn save_key(
    app: AppHandle,
    accounts_state: State<'_, super::Accounts>,
    account: String,
    key: String,
) -> Result<AccountList, String> {
    let holdings = accounts_state.resolved()?;
    accounts::wallet().set_key(holdings, &account, &key)?;
    Ok(settled(&app, holdings))
}

/// Forget the key saved for `account`, leaving it known, on the
/// environment's key if it has one.
///
/// # Errors
/// Returns a plain sentence if the account is unknown, or if the
/// credential manager would not give the key up.
#[tauri::command]
pub fn forget_key(
    app: AppHandle,
    accounts_state: State<'_, super::Accounts>,
    account: String,
) -> Result<AccountList, String> {
    let holdings = accounts_state.resolved()?;
    accounts::wallet().forget_key(holdings, &account)?;
    Ok(settled(&app, holdings))
}

/// Declare another service to talk to; `key` is `None` for a server that
/// checks none.
///
/// # Errors
/// Returns a plain sentence if the name is taken, the address is not one,
/// the protocol is unknown, or the settings file or credential manager
/// refused the write.
#[tauri::command]
pub fn save_endpoint(
    app: AppHandle,
    accounts_state: State<'_, super::Accounts>,
    name: String,
    endpoint: String,
    protocol: String,
    key: Option<String>,
) -> Result<AccountList, String> {
    let holdings = accounts_state.resolved()?;
    accounts::wallet().add_endpoint(holdings, &name, &endpoint, &protocol, key.as_deref())?;
    Ok(settled(&app, holdings))
}

/// Withdraw a declared endpoint entirely.
///
/// # Errors
/// Returns a plain sentence if `account` names a built-in service rather
/// than a declared endpoint, or if the settings file or credential manager
/// refused the write.
#[tauri::command]
pub fn forget_endpoint(
    app: AppHandle,
    accounts_state: State<'_, super::Accounts>,
    account: String,
) -> Result<AccountList, String> {
    let holdings = accounts_state.resolved()?;
    accounts::wallet().forget_endpoint(holdings, &account)?;
    Ok(settled(&app, holdings))
}

/// The account list to answer with, and a background refresh of the
/// picker on its way.
///
/// The listing is a network call per newly usable provider, so it happens
/// on a thread of its own and arrives as the same `models-refreshed` event
/// a sign-in emits; the screen redraws immediately from the list returned
/// here, which needs no network at all.
fn settled(app: &AppHandle, holdings: &Holdings) -> AccountList {
    let list = accounts::list(&holdings.store);
    super::refresh_menu_async(app);
    list
}
