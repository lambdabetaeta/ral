//! The desktop shell — the binary's own half of the crate, private to it.
//!
//! Where the library modules ([`grant`](crate::grant),
//! [`prompt`](crate::prompt), [`session`](crate::session),
//! [`workspace`](crate::workspace)) are the engine anyone could drive, these
//! are the Tauri window that drives it: the commands the window calls
//! ([`commands`]), the accounts screen where keys are entered ([`keys`]),
//! the `ChatGPT` sign-in the opening screen offers
//! ([`signin`]), the change report and undo actions ([`review`]), and the
//! bridge that streams the conversation's narration into the window
//! ([`sink`]).  Nothing here is part of `synod`'s public surface.

pub mod commands;
pub mod keys;
pub mod review;
pub mod signin;
pub mod sink;

use exarch::provider::credential::CredentialStore;
use exarch::provider::models::{LiveSource, ModelCatalog};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter as _, Manager};

/// The credential scrub's outcome, resolved once at startup — while the
/// process is still single-threaded, [`synod::session::prepare`] requires —
/// paired with the model catalog built from it, and held as Tauri state for
/// the app's whole life.  Every command that needs a working model account
/// surfaces the `Err` as its own plain-sentence failure rather than
/// re-running or second-guessing it; the pairing makes "no catalog without
/// credentials" a type fact instead of a runtime invariant the two halves
/// could drift out of sync on.
///
/// Both halves are behind a [`Mutex`] because both grow: a sign-in from the
/// window ([`synod::session::sign_in`]) admits a fresh `ChatGPT` account to
/// the store and its credential to the catalog, and the scrub that built
/// them cannot be re-run in a process that is no longer single-threaded.
/// Every holder in `synod::session` takes them locked only briefly — for an
/// account list, a cached model list, an admission — never across a network
/// call or a machine boot.
pub struct Accounts(Result<(Mutex<CredentialStore>, Mutex<ModelCatalog<LiveSource>>), String>);

impl Accounts {
    /// Wrap the credential scrub's outcome, composed in `main`, as Tauri
    /// state.  The field stays private; every reach from here on goes
    /// through [`Self::resolved`].
    pub(crate) fn new(
        resolved: Result<(Mutex<CredentialStore>, Mutex<ModelCatalog<LiveSource>>), String>,
    ) -> Self {
        Self(resolved)
    }

    /// The store and catalog, or a fresh copy of the startup failure that
    /// left this run with neither — every command answers with the same
    /// sentence rather than each restating how to unwrap it.
    pub(crate) fn resolved(
        &self,
    ) -> Result<&(Mutex<CredentialStore>, Mutex<ModelCatalog<LiveSource>>), String> {
        self.0.as_ref().map_err(Clone::clone)
    }
}

/// The one refresh: reacquire [`Accounts`] through `app`, run the live
/// listing, and emit it as `models-refreshed`.  A run that finds no store
/// stands down silently — there is no menu to announce and nothing to mend.
///
/// Synchronous, so a caller that must order a later event *after* the window
/// has the menu can have that guarantee by calling it directly.
pub(crate) fn refresh_menu_now(app: &AppHandle) {
    let accounts = app.state::<Accounts>();
    let Ok((store, catalog)) = accounts.resolved() else {
        return;
    };
    let menu = synod::session::refresh_menu(store, catalog);
    let _ = app.emit("models-refreshed", menu);
}

/// [`refresh_menu_now`] off the calling thread, for a caller with nothing to
/// order after it.  The live listing blocks, and these callers are answering
/// the window meanwhile.
pub(crate) fn refresh_menu_async(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || refresh_menu_now(&app));
}
