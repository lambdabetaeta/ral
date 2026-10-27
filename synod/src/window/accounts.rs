//! Account commands use ids, return no keys, and refresh the menu after writes.

use super::state::{Accounts, SignIn};
use crate::assistant::accounts;
use crate::wire::{AccountList, SignInDone, SignInStep};
use exarch::provider::{self, Holdings, oauth};
use ral_core::sync::LockExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter as _, Manager, State};

#[tauri::command]
pub(crate) fn list_accounts(accounts_state: State<'_, Accounts>) -> Result<AccountList, String> {
    let Holdings { store, .. } = accounts_state.resolved()?;
    Ok(accounts::list(store))
}

#[tauri::command]
pub(crate) fn save_key(
    app: AppHandle,
    accounts_state: State<'_, Accounts>,
    account: String,
    key: String,
) -> Result<AccountList, String> {
    let holdings = accounts_state.resolved()?;
    accounts::wallet()?.set_key(holdings, &account, &key)?;
    Ok(settled(&app, holdings))
}

#[tauri::command]
pub(crate) fn forget_key(
    app: AppHandle,
    accounts_state: State<'_, Accounts>,
    account: String,
) -> Result<AccountList, String> {
    let holdings = accounts_state.resolved()?;
    accounts::wallet()?.forget_key(holdings, &account)?;
    Ok(settled(&app, holdings))
}

#[tauri::command]
pub(crate) fn save_endpoint(
    app: AppHandle,
    accounts_state: State<'_, Accounts>,
    name: String,
    endpoint: String,
    protocol: String,
    key: Option<String>,
) -> Result<AccountList, String> {
    let holdings = accounts_state.resolved()?;
    accounts::wallet()?.add_endpoint(holdings, &name, &endpoint, &protocol, key.as_deref())?;
    Ok(settled(&app, holdings))
}

#[tauri::command]
pub(crate) fn forget_endpoint(
    app: AppHandle,
    accounts_state: State<'_, Accounts>,
    account: String,
) -> Result<AccountList, String> {
    let holdings = accounts_state.resolved()?;
    accounts::wallet()?.forget_endpoint(holdings, &account)?;
    Ok(settled(&app, holdings))
}

fn settled(app: &AppHandle, holdings: &Holdings) -> AccountList {
    let list = accounts::list(&holdings.store);
    refresh_menu_async(app, holdings.clone());
    list
}

#[tauri::command]
pub(crate) fn sign_in(
    app: AppHandle,
    state: State<'_, SignIn>,
    accounts: State<'_, Accounts>,
) -> Result<(), String> {
    let holdings = accounts.resolved()?.clone();

    let cancel = {
        let mut held = state.0.lock_ignore_poison();
        if held.is_some() {
            return Err("A sign-in is already in progress.".to_string());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *held = Some(Arc::clone(&cancel));
        cancel
    };

    std::thread::spawn(move || {
        let stepping = app.clone();
        let outcome = (|| {
            let (token, replaced) = oauth::login_flow(
                &oauth::SignIn::New(provider::identity::Login::Chatgpt, oauth::Method::Browser),
                |phase| {
                    let _ = stepping.emit(crate::seam::ipc::SIGN_IN_STEP, SignInStep::from(phase));
                },
                &cancel,
            )?;
            let (_, label) = provider::admit_login(&holdings, &token)?;
            Ok(SignInDone::SignedIn { label, replaced })
        })();

        // Release before announcing so a retry cannot collide with its predecessor's flag.
        release(&app.state::<SignIn>(), &cancel);

        let done = match outcome {
            Ok(signed_in) => {
                // Publish the live model listing before announcing sign-in completion.
                refresh_menu_now(&app, &holdings);
                signed_in
            }
            Err(reason) => SignInDone::Failed { reason },
        };
        let _ = app.emit(crate::seam::ipc::SIGN_IN_DONE, done);
    });

    Ok(())
}

#[tauri::command]
pub(crate) fn cancel_sign_in(state: State<'_, SignIn>) {
    if let Some(cancel) = state.0.lock_ignore_poison().as_ref() {
        cancel.store(true, Ordering::Release);
    }
}

/// Clear only this attempt's flag; an older flow must not clear a newer one.
fn release(state: &SignIn, cancel: &Arc<AtomicBool>) {
    let mut held = state.0.lock_ignore_poison();
    if held.as_ref().is_some_and(|held| Arc::ptr_eq(held, cancel)) {
        *held = None;
    }
}

pub(crate) fn refresh_menu_now(app: &AppHandle, holdings: &Holdings) {
    let Holdings { store, catalog, .. } = holdings;
    let menu = crate::assistant::menu::refresh_menu(store, catalog);
    let _ = app.emit(crate::seam::ipc::MODELS_REFRESHED, menu);
}

pub(crate) fn refresh_menu_async(app: &AppHandle, holdings: Holdings) {
    let app = app.clone();
    std::thread::spawn(move || refresh_menu_now(&app, &holdings));
}
