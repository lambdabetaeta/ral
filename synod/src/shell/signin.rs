//! Signing in to a `ChatGPT` plan from the opening screen.
//!
//! One button, one flow, and one rule: the window never waits on it.  The
//! sign-in itself takes as long as the user takes at their browser, so
//! [`sign_in`] hands it to a thread and returns at once; the window learns
//! how it is going from `sign-in-step` events, how it ended from
//! `sign-in-done`, and — on success — gets the refreshed account menu as the
//! same `models-refreshed` event a startup refresh emits, so the picker
//! repopulates through the one path it already has.
//!
//! At most one sign-in runs at a time.  [`SignIn`] holds the running one's
//! cancel flag, which is both the in-flight marker [`sign_in`] refuses a
//! second attempt on and the flag [`cancel_sign_in`] trips: an abandoned
//! sign-in lets go of its loopback listener within a moment, so the next
//! attempt finds the port free rather than inheriting a fifteen-minute
//! wait nobody is watching.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter as _, Manager, State};

/// The sign-in in flight, if there is one — its cancel flag, held so the
/// window can abandon it and so a second attempt can be refused rather
/// than raced against the first for the loopback port.
#[derive(Default)]
pub struct SignIn(Mutex<Option<Arc<AtomicBool>>>);

/// How a sign-in ended, as one event the window can act on without
/// inspecting anything else.
#[derive(Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SignInDone {
    /// The account is signed in, and already in the menu the
    /// `models-refreshed` event alongside this one carries. `label` is its
    /// display name, never an id to hand back.
    SignedIn {
        label: String,
        /// True when this refreshed an account already set up here.
        replaced: bool,
    },
    /// The sign-in did not happen — refused, abandoned, or unreachable.
    /// `reason` is the sentence to show.
    Failed { reason: String },
}

/// Start a sign-in: open the user's browser on the `ChatGPT` sign-in page and
/// carry it through on a thread of its own.
///
/// Returns as soon as that thread is running; the sign-in plays out over
/// `sign-in-step` events and ends in exactly one `sign-in-done`, preceded on
/// success by a `models-refreshed` carrying the menu the new account is now
/// in.
///
/// # Errors
/// Returns a plain sentence if a sign-in is already running, or if startup
/// could not read this computer's credentials at all — a failure no
/// sign-in can mend.
#[tauri::command]
pub fn sign_in(
    app: AppHandle,
    state: State<'_, SignIn>,
    accounts: State<'_, crate::Accounts>,
) -> Result<(), String> {
    accounts.0.as_ref().map_err(Clone::clone)?;

    let cancel = {
        let mut held = guard(&state);
        if held.is_some() {
            return Err("A sign-in is already in progress.".to_string());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *held = Some(Arc::clone(&cancel));
        cancel
    };

    std::thread::spawn(move || {
        let accounts = app.state::<crate::Accounts>();
        let Ok((store, catalog)) = &accounts.0 else {
            // Refused above, before this thread was ever spawned.
            return;
        };

        let stepping = app.clone();
        let outcome = synod::session::sign_in(
            store,
            catalog,
            |step| {
                let _ = stepping.emit("sign-in-step", step);
            },
            &cancel,
        );

        // Let go of the flag before announcing, so the window's own reaction
        // to the announcement — a second attempt after a failure — is never
        // refused by the attempt it is reacting to.
        release(&app.state::<SignIn>(), &cancel);

        let done = match outcome {
            Ok(signed_in) => {
                // The live listing, not the instant one, and emitted before
                // the sign-in is announced done: a plan account brings no
                // default model with it, so until its models have been
                // fetched there is nothing the account could answer with.
                // The window is told it is signed in once it is signed in
                // *and* has something to say it with.
                let _ = app.emit(
                    "models-refreshed",
                    synod::session::refresh_menu(store, catalog),
                );
                SignInDone::SignedIn {
                    label: signed_in.label,
                    replaced: signed_in.replaced,
                }
            }
            Err(reason) => SignInDone::Failed { reason },
        };
        let _ = app.emit("sign-in-done", done);
    });

    Ok(())
}

/// Abandon the sign-in in flight, if any.
///
/// Trips its cancel flag and returns; the flow's own waits notice and end
/// it, and the window hears about it as the `sign-in-done` failure the
/// abandoned flow reports for itself.  Called on a window with no sign-in
/// running, this does nothing — the user clicking cancel as the sign-in
/// lands is not an error.
#[tauri::command]
pub fn cancel_sign_in(state: State<'_, SignIn>) {
    if let Some(cancel) = guard(&state).as_ref() {
        cancel.store(true, Ordering::Release);
    }
}

/// Clear the in-flight marker, but only if it is still `cancel`'s own — a
/// later attempt's flag is never cleared by an earlier attempt finishing.
fn release(state: &SignIn, cancel: &Arc<AtomicBool>) {
    let mut held = guard(state);
    if held.as_ref().is_some_and(|held| Arc::ptr_eq(held, cancel)) {
        *held = None;
    }
}

/// Lock the in-flight slot, recovering the guard even if a thread panicked
/// while holding it — the same discipline the conversation slot keeps: a
/// poisoned lock here must never leave the window unable to sign in.
fn guard(state: &SignIn) -> std::sync::MutexGuard<'_, Option<Arc<AtomicBool>>> {
    state
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
