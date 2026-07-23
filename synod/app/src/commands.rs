//! The real commands the window calls: pick a folder, hold the in-process
//! conversation open, send it messages, start again, and open a file with
//! the application the user already uses for it.
//!
//! None of this is stubbed.  The folder picker is the platform's own; the
//! conversation is a [`synod::session::Conversation`] driven on its own
//! worker thread for as long as the window wants it, with its narration
//! streamed into the window line by line and messages handed across an
//! in-process channel; opening a file hands the path to the user's default
//! application through the opener plugin.
//!
//! The credential store is resolved once, at startup, before this module
//! ever runs — see [`crate::Credentials`] — so every command here either
//! finds it already settled or surfaces its one failure as a plain
//! sentence.

use std::io::{self, Write};
use std::panic;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;
use synod::session::Conversation;
use tauri::{AppHandle, Emitter, Manager, State};

/// The live conversation, if one is running — reached by [`send_message`],
/// [`restart_conversation`], and the window's own close handler.
#[derive(Default, Clone)]
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
/// life: a worker thread compares it against whatever handle currently
/// occupies the slot before it announces its own end, so a conversation a
/// restart has already superseded stays silent instead of reporting over
/// the fresh one that replaced it.
pub(crate) struct Handle {
    generation: u64,
    sender: mpsc::Sender<String>,
    /// Attached once the thread exists.  Only [`end_conversation`] reaches
    /// for it, to bound how long the window's close waits.
    join: Option<JoinHandle<()>>,
}

/// One line of the conversation's narration, sent to the window as it
/// arrives.  The text is developer-flavoured for now — the window tucks it
/// into a "details" fold under the assistant's own narration — so `stream`
/// marks which channel it came from without asking the window to interpret
/// it.
#[derive(Clone, Serialize)]
struct OutputLine {
    /// `"out"` for the assistant's own narration, `"err"` for the run's
    /// progress and tool trace.
    stream: &'static str,
    line: String,
}

/// The conversation has ended, one way or another — a failure, or a
/// deliberate stop (a restart, or the window closing).
#[derive(Clone, Serialize)]
struct ConversationEnded {
    /// True only when the conversation ran its whole life and shut its
    /// machine down cleanly.
    success: bool,
    /// True when synod-app ended it on purpose rather than it failing on
    /// its own.
    stopped: bool,
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
/// # Errors
/// Returns the credential scrub's own failure, if startup could not
/// resolve one.
#[tauri::command]
pub fn list_models(
    credentials: State<'_, crate::Credentials>,
) -> Result<synod::session::ModelMenu, String> {
    match &credentials.0 {
        Ok(store) => Ok(synod::session::menu(store)),
        Err(e) => Err(e.clone()),
    }
}

/// Start the conversation: open `folder` on its own worker thread and hold
/// it open, ready for [`send_message`], its narration streamed to the
/// window as `synod-log` events, and — however it ends — a single
/// `synod-ended` event.
///
/// This returns as soon as the thread is spawned; the conversation plays
/// out over the events from here on.
///
/// # Errors
/// Returns a plain sentence if the credential scrub failed at startup.
#[tauri::command]
pub fn start_conversation(
    app: AppHandle,
    state: State<'_, Running>,
    credentials: State<'_, crate::Credentials>,
    folder: String,
    choice: Option<(String, String)>,
) -> Result<(), String> {
    if let Err(e) = &credentials.0 {
        return Err(e.clone());
    }
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

/// "Start again": end the running conversation, if any, and start a fresh
/// one over the same folder — a new conversation is a new machine, never a
/// cleared one.
///
/// Dropping the old handle's sender lets its thread finish the exchange it
/// is mid-way through and shut its own machine down in its own time; there
/// is no way to reach into an in-flight exchange and cancel it from here
/// (the cancellation token [`exarch::agent::cancel`] threads through a
/// turn is reached through the fleet registry, which
/// [`synod::session::Conversation`] never hands out), so the old
/// conversation is superseded rather than interrupted.  Its own
/// `synod-ended`, whenever it eventually comes, is recognised as stale by
/// generation and never reaches the window.
///
/// # Errors
/// Returns a plain sentence if the credential scrub failed at startup.
#[tauri::command]
pub fn restart_conversation(
    app: AppHandle,
    state: State<'_, Running>,
    credentials: State<'_, crate::Credentials>,
    folder: String,
    choice: Option<(String, String)>,
) -> Result<(), String> {
    if let Err(e) = &credentials.0 {
        return Err(e.clone());
    }
    let slot = state.slot();
    let superseded = guard(&slot).take();
    if let Some(handle) = superseded {
        drop(handle.sender);
    }
    spawn_conversation(app, slot, folder, choice);
    Ok(())
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
    let Some(join) = handle.join else { return };
    let (done, wait) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = join.join();
        let _ = done.send(());
    });
    let _ = wait.recv_timeout(Duration::from_secs(5));
}

/// Bump the generation, register its handle in the slot before the thread
/// can possibly outrun that registration, then let the conversation run to
/// completion on its own thread.
fn spawn_conversation(
    app: AppHandle,
    slot: Arc<Mutex<Option<Handle>>>,
    folder: String,
    choice: Option<(String, String)>,
) {
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::SeqCst);
    let (sender, receiver) = mpsc::channel();
    *guard(&slot) = Some(Handle {
        generation,
        sender,
        join: None,
    });

    let thread_slot = slot.clone();
    let join = std::thread::spawn(move || {
        run_conversation(app, thread_slot, generation, folder, choice, receiver);
    });

    if let Some(handle) = guard(&slot).as_mut()
        && handle.generation == generation
    {
        handle.join = Some(join);
    }
}

/// The worker thread's whole body: run the conversation to whatever end it
/// reaches, then announce that end — unless a restart has since superseded
/// this generation, in which case the window has already moved on and
/// hears nothing.  A panicking conversation is reported the same as any
/// other failure to start or run.
fn run_conversation(
    app: AppHandle,
    slot: Arc<Mutex<Option<Handle>>>,
    generation: u64,
    folder: String,
    choice: Option<(String, String)>,
    receiver: mpsc::Receiver<String>,
) {
    let ended = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        converse(&app, &folder, choice, &receiver)
    }))
    .unwrap_or(ConversationEnded {
        success: false,
        stopped: false,
    });

    let mut held = guard(&slot);
    if held.as_ref().is_some_and(|h| h.generation == generation) {
        *held = None;
        drop(held);
        let _ = app.emit("synod-ended", ended);
    }
}

/// Open the folder, narrate the opening, then drive one exchange per
/// message until the sender is dropped — a restart or the window closing —
/// and shut the machine down.
fn converse(
    app: &AppHandle,
    folder: &str,
    choice: Option<(String, String)>,
    receiver: &mpsc::Receiver<String>,
) -> ConversationEnded {
    let mut out = LineWriter::new(app.clone(), "out");
    let mut err = LineWriter::new(app.clone(), "err");

    let credentials = app.state::<crate::Credentials>();
    let store = credentials
        .0
        .as_ref()
        .expect("start_conversation and restart_conversation refuse before spawning this thread when the credential scrub failed");

    let (mut conversation, opening) = match Conversation::begin(Path::new(folder), store, choice) {
        Ok(begun) => begun,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            return ConversationEnded {
                success: false,
                stopped: false,
            };
        }
    };

    let _ = writeln!(err, "Working in {}", opening.folder);
    let _ = writeln!(err, "{}", opening.boundary_line);
    let _ = writeln!(err);
    let _ = writeln!(err, "{}", opening.assistant_line);
    if let Some(large_folder_line) = &opening.large_folder_line {
        let _ = writeln!(err, "{large_folder_line}");
    }
    let _ = err.flush();

    while let Ok(message) = receiver.recv() {
        if let Err(e) = conversation.exchange(message, &mut out, &mut err) {
            let _ = writeln!(err, "{e}");
        }
        // Announced even after a failed exchange: the after-checkpoint ran
        // regardless, and whatever changed before the failure is already in
        // the report the window will now re-read.
        let _ = app.emit("exchange-done", ());
    }

    ConversationEnded {
        success: conversation.end().is_ok(),
        stopped: true,
    }
}

/// Lock the conversation slot, recovering the guard even if a thread
/// panicked while holding it — a poisoned lock here means only that a
/// reader thread unwound, which must not wedge the window.
fn guard(slot: &Arc<Mutex<Option<Handle>>>) -> std::sync::MutexGuard<'_, Option<Handle>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Buffers written bytes to whole lines and emits each as its own
/// `synod-log` event; a trailing partial line — no closing `\n` yet — is
/// flushed as its own event on [`Write::flush`] or drop, so nothing
/// written is ever lost between exchanges.
struct LineWriter {
    app: AppHandle,
    stream: &'static str,
    buffer: String,
}

impl LineWriter {
    fn new(app: AppHandle, stream: &'static str) -> Self {
        Self {
            app,
            stream,
            buffer: String::new(),
        }
    }
}

impl io::Write for LineWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.push_str(&String::from_utf8_lossy(buf));
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].to_string();
            let _ = self.app.emit(
                "synod-log",
                OutputLine {
                    stream: self.stream,
                    line,
                },
            );
            self.buffer.drain(..=pos);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            let _ = self.app.emit(
                "synod-log",
                OutputLine {
                    stream: self.stream,
                    line,
                },
            );
        }
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Hand `path` to the user's default application for that file.  Also the
/// way [`review::open_earlier`](crate::review::open_earlier) opens the
/// version it sets out from history.
pub(crate) fn open_with_default(app: &AppHandle, path: &str) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(path.to_string(), None::<&str>)
        .map_err(|e| format!("Could not open {path}: {e}"))
}
