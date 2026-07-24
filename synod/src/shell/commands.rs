//! The real commands the window calls: pick a folder, hold the in-process
//! conversation open, send it messages, start again, and open a file with
//! the application the user already uses for it.
//!
//! None of this is stubbed.  The folder picker is the platform's own; the
//! conversation is a [`synod::session::Conversation`] driven on its own
//! worker thread for as long as the window wants it, with its narration
//! streamed into the window as [`super::sink::SynodEvent`]s and messages
//! handed across an in-process channel; opening a file hands the path to
//! the user's default application through the opener plugin.
//!
//! The credential store is resolved once, at startup, before this module
//! ever runs — see [`crate::Accounts`] — so every command here either
//! finds it already settled or surfaces its one failure as a plain
//! sentence.

use std::panic;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;
use synod::session::{Choice, Conversation};
use tauri::{AppHandle, Emitter as _, Manager, State};

/// The live conversation, if one is running — reached by [`send_message`],
/// [`start_conversation`]'s own supersession, and the window's own close
/// handler.
#[derive(Default)]
pub struct Running(Arc<Mutex<Option<Handle>>>);

impl Running {
    /// The shared slot, for the window's own close handler to end the
    /// conversation with — the one reach into this module from outside it.
    pub(crate) fn slot(&self) -> Arc<Mutex<Option<Handle>>> {
        self.0.clone()
    }
}

/// One running conversation's worker thread and the channel that feeds it.
///
/// `generation` is this handle's own identity, unique for the app's whole
/// life: the worker's [`Emitter`] compares it against whatever handle
/// currently occupies the slot before every emit, so a conversation a
/// restart has already superseded stays silent instead of reporting over
/// the fresh one that replaced it.
pub(crate) struct Handle {
    generation: u64,
    sender: mpsc::Sender<String>,
    join: JoinHandle<()>,
}

/// The conversation has ended, one way or another — a failure, or a
/// deliberate stop (a restart, or the window closing).
#[derive(Clone, Serialize)]
struct ConversationEnded {
    /// True when the shell ended it on purpose rather than it failing on
    /// its own.
    stopped: bool,
}

/// The worker thread's own gated seam onto the window: [`Self::emit`] reaches
/// `app.emit` only while this generation still occupies the conversation
/// slot, so a superseded worker's narration — tokens, cards, its own
/// `synod-opening`, a failure — can never land in a transcript a successor
/// has already begun.
#[derive(Clone)]
pub(crate) struct Emitter {
    app: AppHandle,
    slot: Arc<Mutex<Option<Handle>>>,
    generation: u64,
}

impl Emitter {
    pub(crate) fn emit<T: Serialize + Clone>(&self, event: &str, payload: T) {
        if guard(&self.slot).as_ref().is_some_and(|h| h.generation == self.generation) {
            let _ = self.app.emit(event, payload);
        }
    }

    /// The one terminal emit: takes the slot itself and emits `synod-ended`
    /// while still holding the lock, so clearing the slot and announcing
    /// the end are atomic with respect to a successor claiming it — a stale
    /// end can never land after a successor's opening.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the slot must stay held across the emit, not just the take, so a successor can never claim it between clearing and announcing"
    )]
    fn ended(&self, payload: ConversationEnded) {
        let mut held = guard(&self.slot);
        if held.as_ref().is_some_and(|h| h.generation == self.generation) {
            held.take();
            let _ = self.app.emit("synod-ended", payload);
        }
    }
}

/// Open the native folder picker and return the chosen folder, or `None`
/// if the user closed the picker without choosing.
///
/// The title is the user's question, not ours: "Choose the folder for
/// this job" is the whole of what synod is about to be given.
///
/// `async` so it runs off the main thread: the dialog blocks until the
/// user answers, and the main thread is the one that must stay free to
/// show it.
#[tauri::command]
pub async fn choose_folder(app: AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    app.dialog()
        .file()
        .set_title("Choose the folder for this job")
        .blocking_pick_folder()
        .and_then(|picked| picked.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
}

/// The provider/model menu the window offers before starting: one entry
/// per account this computer has credentials for.
///
/// Answers instantly from whatever [`crate::Accounts`] already has cached —
/// a fresh disk entry carried over from an earlier run, or nothing at all
/// — and, the first time this run calls it, kicks off a background fetch
/// of the complete listing: a `std::thread` that reaches the same managed
/// state through `app` (the pattern [`converse`] already uses to reach
/// [`crate::Accounts`] from its own worker thread), calls
/// [`synod::session::refresh_menu`], and emits the result as
/// `models-refreshed` unconditionally — even when it turns out to equal
/// this instant menu — so the window is never left waiting on a refresh
/// that silently agreed with what it already showed.  [`crate::RefreshGate`]
/// makes sure at most one such fetch is ever in flight for the run.
///
/// # Errors
/// Returns the credential scrub's own failure, if startup could not
/// resolve one.
#[tauri::command]
pub fn list_models(
    app: AppHandle,
    accounts: State<'_, crate::Accounts>,
    gate: State<'_, crate::RefreshGate>,
) -> Result<synod::session::ModelMenu, String> {
    let (store, catalog_mutex) = accounts.0.as_ref().map_err(Clone::clone)?;
    let instant = synod::session::menu(store, catalog_mutex);

    if !gate.0.swap(true, Ordering::SeqCst) {
        std::thread::spawn(move || {
            let accounts = app.state::<crate::Accounts>();
            let Ok((store, catalog_mutex)) = &accounts.0 else {
                return;
            };
            let menu = synod::session::refresh_menu(store, catalog_mutex);
            let _ = app.emit("models-refreshed", menu);
        });
    }

    Ok(instant)
}

/// Start the conversation: open `folder` on its own worker thread and hold
/// it open, ready for [`send_message`], its opening announced as one
/// `synod-opening` event, its narration streamed to the window as
/// `synod-event` events, and — however it ends — a single `synod-ended`
/// event.
///
/// This returns as soon as the thread is spawned; the conversation plays
/// out over the events from here on.  Calling this again while a
/// conversation is already running supersedes it, by generation, rather
/// than refusing — see [`Handle`].
///
/// # Errors
/// Returns a plain sentence if the credential scrub failed at startup.
#[tauri::command]
pub fn start_conversation(
    app: AppHandle,
    state: State<'_, Running>,
    accounts: State<'_, crate::Accounts>,
    folder: String,
    choice: Option<Choice>,
) -> Result<(), String> {
    accounts.0.as_ref().map_err(Clone::clone)?;
    spawn_conversation(app, state.slot(), folder, choice);
    Ok(())
}

/// Send one message into the running conversation.
///
/// # Errors
/// Returns a plain sentence if there is no conversation running, or if it
/// has already finished and cannot take this message.
#[tauri::command]
#[allow(
    clippy::significant_drop_tightening,
    reason = "the lock must stay held across the send, not just the lookup, so a restart cannot swap the handle out from under it"
)]
pub fn send_message(state: State<'_, Running>, message: String) -> Result<(), String> {
    let slot = state.slot();
    let held = guard(&slot);
    let handle = held
        .as_ref()
        .ok_or_else(|| "There is no conversation running to send this to.".to_string())?;
    handle
        .sender
        .send(message)
        .map_err(|_| "The assistant has already finished; nothing was sent.".to_string())
}

/// Open a file as it is now, with the user's own application for that kind
/// of file — the "Open" button on a change card.
///
/// # Errors
/// Returns a sentence naming the file if the system cannot open it.
#[tauri::command]
pub fn open_file(app: AppHandle, path: String) -> Result<(), String> {
    open_with_default(&app, &path)
}

/// Open a link the assistant wrote in the user's own browser — a hyperlink
/// in a rendered assistant message, handed here rather than followed inside
/// the window, whose one document is the conversation and must never
/// navigate away from it.
///
/// # Errors
/// Returns a sentence naming the link if the system cannot open it.
#[tauri::command]
pub fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url.clone(), None::<&str>)
        .map_err(|e| format!("Could not open {url}: {e}"))
}

/// End the conversation cleanly: drop the message sender — the worker
/// thread's own end-of-conversation signal — and give it a few seconds to
/// take its final path (shutting its machine down) before giving up on the
/// wait, so a wedged conversation can never keep the window from closing.
/// Called from the window's own close handler, never the frontend.
pub(crate) fn end_conversation(slot: &Arc<Mutex<Option<Handle>>>) {
    let Some(handle) = guard(slot).take() else {
        return;
    };
    drop(handle.sender);
    let join = handle.join;
    let (done, wait) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = join.join();
        let _ = done.send(());
    });
    let _ = wait.recv_timeout(Duration::from_secs(5));
}

/// Bump the generation, supersede whatever handle currently occupies the
/// slot, and register the new one — all in one critical section, so the
/// window's close handler can never observe a half-registered handle.
///
/// Superseding drops the old sender immediately, ending that worker's
/// message stream right here, and hands its `JoinHandle` into the new
/// worker's closure rather than joining it under this lock: the new
/// worker joins it itself, before it opens anything, well off the slot
/// lock.  Holding the guard across `thread::spawn` cannot deadlock — the
/// spawned thread's first slot access comes only after that join, by
/// which point this function has long since written the complete
/// [`Handle`] and dropped the guard.
fn spawn_conversation(
    app: AppHandle,
    slot: Arc<Mutex<Option<Handle>>>,
    folder: String,
    choice: Option<Choice>,
) {
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::SeqCst);

    let mut held = guard(&slot);
    let superseded = held.take().map(|handle| {
        drop(handle.sender);
        handle.join
    });

    let (sender, receiver) = mpsc::channel();
    let thread_slot = slot.clone();
    let join = std::thread::spawn(move || {
        run_conversation(app, thread_slot, generation, superseded, folder, choice, receiver);
    });
    *held = Some(Handle {
        generation,
        sender,
        join,
    });
}

/// The worker thread's whole body: first join whatever conversation this
/// one superseded, so its final checkpoint is written before this one
/// walks the folder's baseline; then, if a later generation has since
/// claimed the slot out from under this one (restart-spam), stand down
/// quietly rather than boot a machine nobody is waiting on.  Otherwise run
/// the conversation to whatever end it reaches and announce that end
/// through the gated [`Emitter`].  A panicking conversation is reported the
/// same as any other failure to start or run.
fn run_conversation(
    app: AppHandle,
    slot: Arc<Mutex<Option<Handle>>>,
    generation: u64,
    superseded: Option<JoinHandle<()>>,
    folder: String,
    choice: Option<Choice>,
    receiver: mpsc::Receiver<String>,
) {
    if let Some(old) = superseded {
        let _ = old.join();
    }
    if guard(&slot).as_ref().is_none_or(|h| h.generation != generation) {
        return;
    }

    let emitter = Emitter {
        app,
        slot,
        generation,
    };

    let ended = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        converse(&emitter, &folder, choice, &receiver)
    }))
    .unwrap_or(ConversationEnded { stopped: false });

    emitter.ended(ended);
}

/// Open the folder, narrate the opening, then drive one exchange per
/// message until the sender is dropped — a restart or the window closing —
/// and shut the machine down.
fn converse(
    emitter: &Emitter,
    folder: &str,
    choice: Option<Choice>,
    receiver: &mpsc::Receiver<String>,
) -> ConversationEnded {
    let mut sink = super::sink::TauriSink::new(emitter.clone());

    let accounts = emitter.app.state::<crate::Accounts>();
    let (store, _) = accounts.0.as_ref().expect(
        "start_conversation refuses before spawning this thread when the credential scrub failed",
    );

    let (mut conversation, opening) = match Conversation::begin(Path::new(folder), store, choice) {
        Ok(begun) => begun,
        Err(e) => {
            emitter.emit("synod-event", super::sink::SynodEvent::Failure { message: e });
            return ConversationEnded { stopped: false };
        }
    };

    emitter.emit("synod-opening", opening);

    while let Ok(message) = receiver.recv() {
        if let Err(e) = conversation.exchange(message, &mut sink) {
            emitter.emit("synod-event", super::sink::SynodEvent::Failure { message: e });
        }
        // Announced even after a failed exchange: the after-checkpoint ran
        // regardless, and whatever changed before the failure is already in
        // the report the window will now re-read.
        emitter.emit("exchange-done", ());
    }

    let _ = conversation.end();
    ConversationEnded { stopped: true }
}

/// Lock the conversation slot, recovering the guard even if a thread
/// panicked while holding it — a poisoned lock here means only that a
/// reader thread unwound, which must not wedge the window.
fn guard(slot: &Arc<Mutex<Option<Handle>>>) -> std::sync::MutexGuard<'_, Option<Handle>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Hand `path` to the user's default application for that file.  Also the
/// way [`review::open_earlier`](super::review::open_earlier) opens the
/// version it sets out from history.
pub(crate) fn open_with_default(app: &AppHandle, path: &str) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(path.to_string(), None::<&str>)
        .map_err(|e| format!("Could not open {path}: {e}"))
}
