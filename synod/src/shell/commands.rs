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
//! ever runs — see [`super::Accounts`] — so every command here either
//! finds it already settled or surfaces its one failure as a plain
//! sentence.

use std::panic;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::Duration;

use ral_core::sync::LockExt;
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
    /// Reaches into this generation's `Conversation::begin`, still stat-ing
    /// the folder on the worker thread, from outside it: a restart
    /// superseding this handle and the window's own close both flip it
    /// before waiting on `join`, so neither is held hostage behind a slow
    /// share's walk finishing on its own.
    measure_stop: synod::workspace::manifest::Stop,
}

/// The conversation has ended, one way or another — a failure, or a
/// deliberate stop (a restart, or the window closing).
#[derive(Clone, Serialize)]
struct ConversationEnded {
    /// True when the shell ended it on purpose rather than it failing on
    /// its own.
    stopped: bool,
    /// True when a `synod-event` `Failure` already told the window its own
    /// sentence for why — so the window's generic "stopped unexpectedly"
    /// must stay silent instead of contradicting it.
    explained: bool,
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
    /// Whether this generation still occupies the slot: the gate every emit
    /// passes, and the worker's own test for whether to stand down.
    fn current(&self) -> bool {
        self.slot
            .lock_ignore_poison()
            .as_ref()
            .is_some_and(|h| h.generation == self.generation)
    }

    pub(crate) fn emit<T: Serialize + Clone>(&self, event: &str, payload: T) {
        if self.current() {
            let _ = self.app.emit(event, payload);
        }
    }

    /// Name what the shell itself is doing in the status bar's one station,
    /// before there is an agent whose states to relay.  The shell's own
    /// narration runs from the worker's first breath to the opening, where
    /// `ready` hands the station over.
    fn state(&self, label: impl Into<String>, pending: bool) {
        self.emit(
            "synod-event",
            super::sink::SynodEvent::State {
                label: label.into(),
                pending,
            },
        );
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
        let mut held = self.slot.lock_ignore_poison();
        if held
            .as_ref()
            .is_some_and(|h| h.generation == self.generation)
        {
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
/// Answers instantly from whatever [`super::Accounts`] already has cached —
/// a fresh disk entry carried over from an earlier run, or nothing at all
/// — and, the first time this run calls it, kicks off
/// [`super::refresh_menu_async`], which emits the result as
/// `models-refreshed` unconditionally — even when it turns out to equal
/// this instant menu — so the window is never left waiting on a refresh
/// that silently agreed with what it already showed.  The managed
/// [`Once`] makes sure at most one such fetch is ever in flight for the
/// run: one refresh per run is all the catalog is worth, since its disk
/// cache already carries its own day-long freshness window.
///
/// # Errors
/// Returns the credential scrub's own failure, if startup could not
/// resolve one.
#[tauri::command]
pub fn list_models(
    app: AppHandle,
    accounts: State<'_, super::Accounts>,
    refresh_started: State<'_, Once>,
) -> Result<synod::session::ModelMenu, String> {
    let (store, catalog) = accounts.resolved()?;
    let instant = synod::session::menu(store, catalog);

    refresh_started.call_once(|| super::refresh_menu_async(&app));

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
    accounts: State<'_, super::Accounts>,
    folder: String,
    choice: Option<Choice>,
) -> Result<(), String> {
    accounts.resolved()?;
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
    let held = slot.lock_ignore_poison();
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
    let Some(handle) = slot.lock_ignore_poison().take() else {
        return;
    };
    drop(handle.sender);
    handle.measure_stop.stop();
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

    let mut held = slot.lock_ignore_poison();
    let superseded = held.take().map(|handle| {
        drop(handle.sender);
        // Stopped here, not left for the new worker's join to wait out: a
        // restart while the superseded generation is still stat-ing a slow
        // share must not make the fresh conversation sit behind that walk
        // finishing on its own.
        handle.measure_stop.stop();
        handle.join
    });

    let measure_stop = synod::workspace::manifest::Stop::default();
    let (sender, receiver) = mpsc::channel();
    let emitter = Emitter {
        app,
        slot: slot.clone(),
        generation,
    };
    let thread_measure_stop = measure_stop.clone();
    let join = std::thread::spawn(move || {
        run_conversation(emitter, superseded, folder, choice, receiver, thread_measure_stop);
    });
    *held = Some(Handle {
        generation,
        sender,
        join,
        measure_stop,
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
///
/// The join is narrated because it can outlast a person's patience: the
/// superseded generation finishes booting its machine and shuts it down
/// before it returns, and the window has already reset its transcript and
/// is waiting on this one.
fn run_conversation(
    emitter: Emitter,
    superseded: Option<JoinHandle<()>>,
    folder: String,
    choice: Option<Choice>,
    receiver: mpsc::Receiver<String>,
    measure_stop: synod::workspace::manifest::Stop,
) {
    if let Some(old) = superseded {
        emitter.state("finishing the previous session", true);
        let _ = old.join();
    }
    if !emitter.current() {
        return;
    }
    emitter.state("starting", true);

    let ended = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        converse(&emitter, &folder, choice, &receiver, &measure_stop)
    }))
    .unwrap_or(ConversationEnded {
        stopped: false,
        explained: false,
    });

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
    measure_stop: &synod::workspace::manifest::Stop,
) -> ConversationEnded {
    let mut sink = super::sink::TauriSink::new(emitter.clone());

    let accounts = emitter.app.state::<super::Accounts>();
    // start_conversation already checked this before spawning the thread;
    // a failure here means the credential scrub's outcome flipped under us,
    // which cannot happen — but a worker that finds no store stands down
    // rather than panics.
    let Ok((store, catalog)) = accounts.resolved() else {
        return ConversationEnded {
            stopped: false,
            explained: false,
        };
    };

    #[allow(
        clippy::disallowed_methods,
        reason = "path: lifting the folder the user picked in the window's own dialog — the \
                  user-input adapter site clippy.toml admits, not a path built from parts"
    )]
    let picked = Path::new(folder);
    // Named before it is booted or hashed: on a large folder or a
    // network share, `begin`'s own stat-walk is the one silent minute
    // a user meets before anything else has had a chance to say something.
    let mut report_measure = |measure: synod::workspace::manifest::Measure| {
        if measure.files == 1 || measure.files.is_multiple_of(500) {
            emitter.state(
                format!("reading the folder — {} files seen so far", measure.files),
                true,
            );
        }
    };
    let (mut conversation, opening) = match Conversation::begin(
        picked,
        store,
        catalog,
        choice,
        measure_stop,
        &mut report_measure,
    ) {
        Ok(begun) => begun,
        Err(e) => {
            emitter.emit(
                "synod-event",
                super::sink::SynodEvent::Failure { message: e },
            );
            return ConversationEnded {
                stopped: false,
                explained: true,
            };
        }
    };

    emitter.emit("synod-opening", opening);
    emitter.state("ready", false);

    while let Ok(message) = receiver.recv() {
        if let Err(e) = conversation.exchange(message, &mut sink) {
            emitter.emit(
                "synod-event",
                super::sink::SynodEvent::Failure { message: e },
            );
        }
        // Announced even after a failed exchange: the after-checkpoint ran
        // regardless, and whatever changed before the failure is already in
        // the report the window will now re-read.
        emitter.emit("exchange-done", ());
    }

    let _ = conversation.end();
    ConversationEnded {
        stopped: true,
        explained: false,
    }
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
