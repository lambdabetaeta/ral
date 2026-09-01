//! Signing a `ChatGPT` account into this computer, in the window's own
//! words — see [`crate::session::sign_in`].

use exarch::provider::{
    self,
    credential::CredentialStore,
    models::{LiveSource, ModelCatalog},
    oauth,
};
use ral_core::sync::LockExt;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// One step of a sign-in in progress, in the words the window says out
/// loud.
#[derive(Clone, serde::Serialize)]
pub struct SignInStep {
    /// What the window should say while this is the step in hand.
    pub say: String,
    /// The sign-in link, for the window to offer alongside its prose.
    pub link: Option<String>,
}

impl From<oauth::LoginPhase> for SignInStep {
    fn from(phase: oauth::LoginPhase) -> Self {
        match phase {
            oauth::LoginPhase::AwaitingBrowser { url } => Self {
                say: "Finish signing in, in your browser.  If no window opened, \
                      follow this link:"
                    .to_string(),
                link: Some(url),
            },
            // The window never runs the device flow — a sign-in in a window
            // is a sign-in on the machine the browser is on — so this arm
            // completes the phase vocabulary rather than describing
            // anything synod shows.
            oauth::LoginPhase::AwaitingDevice {
                user_code,
                url,
                expires_in,
            } => Self {
                say: format!(
                    "Follow this link and enter the code {user_code} to sign in.  \
                     The code expires in {expires_in}."
                ),
                link: Some(url),
            },
            oauth::LoginPhase::ExchangingCode => Self {
                say: "Signing you in…".to_string(),
                link: None,
            },
        }
    }
}

/// A finished sign-in, as the window reports it.
#[derive(Clone, serde::Serialize)]
pub struct SignedIn {
    /// The signed-in account's [`identity::label`](provider::identity::label),
    /// the name [`crate::session::menu`] lists it under. A display string —
    /// the wire says so, so no window is ever tempted to hand it back as a
    /// [`crate::session::Choice::account`].
    pub label: String,
    /// Whether this refreshed the login for an account already set up here,
    /// rather than adding a new one.
    pub replaced: bool,
}

/// Sign in to a `ChatGPT` plan and admit the account to this run.
///
/// The flow is exarch's ([`oauth::login_flow`]): it opens the user's
/// browser, waits on the loopback callback, exchanges the code, and stores
/// the token where `exarch login` stores it, so a computer signed in here
/// is signed in for both.  It blocks — for as long as the user takes at
/// their browser — so the caller runs it on a thread of its own,
/// `on_phase` carrying each step to the window and `cancel` the abandon
/// flag the flow's waits poll.
///
/// The last step is synod's own: the fresh token goes into the live store
/// and into the catalog built from it, so the account appears in the very
/// next [`crate::session::menu`] and can open the very next conversation.
/// Nothing here re-runs [`crate::session::prepare`] — its scrub is only
/// safe on a single-threaded process, and this one has long since stopped
/// being one — which is why the account is admitted rather than
/// re-resolved.
///
/// # Errors
/// Returns the flow's own sentence: a refused or abandoned sign-in, a
/// browser that never came back, a network that would not carry the
/// exchange.
pub fn sign_in(
    store: &Mutex<CredentialStore>,
    catalog: &Mutex<ModelCatalog<LiveSource>>,
    on_phase: impl Fn(SignInStep),
    cancel: &Arc<AtomicBool>,
) -> Result<SignedIn, String> {
    let (token, replaced) = oauth::login_flow(
        oauth::LoginMethod::Browser,
        |phase| on_phase(SignInStep::from(phase)),
        cancel,
    )?;
    let (_, label) = provider::admit_login(
        &mut store.lock_ignore_poison(),
        &mut catalog.lock_ignore_poison(),
        &token,
    );
    Ok(SignedIn { label, replaced })
}
