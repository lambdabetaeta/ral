//! The real commands the window calls: pick a folder, hold the in-process
//! conversation open, send it messages, start again, and open a file with
//! the application the user already uses for it.
//!
//! None of this is stubbed.  The folder picker is the platform's own; the
//! conversation is a [`synod::session::Conversation`] driven on its own
//! worker thread for as long as the window wants it, with its narration
//! streamed into the window as [`super::sink::SynodEvent`]s and messages
//! handed across an in-process channel; opening a file hands a document the
//! shell has checked to the user's default application through the opener
//! plugin, and shows any other file in its folder rather than running it.
//!
//! The credential store is resolved once, at startup, before this module
//! ever runs — see [`super::Accounts`] — so every command here either
//! finds it already settled or surfaces its one failure as a plain
//! sentence.

use ral_core::path::shown;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use exarch::provider::Holdings;
use ral_core::sync::LockExt;
use serde::Serialize;
use synod::session::{Choice, Conversation};
use tauri::{AppHandle, Emitter as _, Manager, State};

/// The live conversation, if one is running — reached by [`send_message`],
/// [`start_conversation`]'s own supersession, and the window's own close
/// handler.
#[derive(Default)]
pub struct Running(Arc<Mutex<Owner>>);

enum Owner {
    Open(Option<Handle>),
    Closing,
}

impl Default for Owner {
    fn default() -> Self {
        Self::Open(None)
    }
}

impl Owner {
    fn current(&self) -> Option<&Handle> {
        match self {
            Self::Open(handle) => handle.as_ref(),
            Self::Closing => None,
        }
    }

    fn open(&mut self) -> Option<&mut Option<Handle>> {
        match self {
            Self::Open(handle) => Some(handle),
            Self::Closing => None,
        }
    }

    /// The handle, if it is still the one registered under `generation`.
    fn owned_by(&self, generation: u64) -> Option<&Handle> {
        self.current().filter(|h| h.generation == generation)
    }
}

/// The first close request's shutdown job; consumed outside the owner lock.
#[must_use]
pub(crate) struct Close(Option<Handle>);

impl Close {
    /// Stop the conversation and give its machine a few seconds to shut
    /// down, so a wedged one can never keep the window from closing.
    pub(crate) fn wait(self) {
        let Some(handle) = self.0 else { return };
        let join = handle.stop();
        let (done, wait) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = join.join();
            let _ = done.send(());
        });
        let _ = wait.recv_timeout(Duration::from_secs(5));
    }
}

impl Running {
    /// Atomically forbid registration and hand shutdown to the first caller.
    pub(crate) fn begin_close(&self) -> Option<Close> {
        let previous = std::mem::replace(&mut *self.0.lock_ignore_poison(), Owner::Closing);
        match previous {
            Owner::Open(handle) => Some(Close(handle)),
            Owner::Closing => None,
        }
    }
}

/// One running conversation's worker thread and the channel that feeds it.
///
/// `generation` is this handle's own identity, unique for the app's whole
/// life: the worker's [`Emitter`] compares it against whatever handle
/// currently occupies the slot before every emit, so a conversation a
/// restart has already superseded stays silent instead of reporting over
/// the fresh one that replaced it.  `attempt` is the window's own name for
/// the conversation: it stamps every update, and [`send_message`] is
/// refused unless it names this handle.
struct Handle {
    generation: u64,
    attempt: String,
    sender: mpsc::Sender<String>,
    join: JoinHandle<()>,
    /// Reaches into this generation's baseline walk, still stat-ing the
    /// folder on a thread of its own, from outside it: a restart
    /// superseding this handle and the window's own close both flip it
    /// before waiting on `join`, so neither is held hostage behind a slow
    /// share's walk finishing on its own.
    baseline_stop: synod::workspace::manifest::Stop,
}

impl Handle {
    /// End the message stream — the worker's signal to shut its machine
    /// down — and the baseline walk, leaving only the thread to join.
    fn stop(self) -> JoinHandle<()> {
        drop(self.sender);
        self.baseline_stop.stop();
        self.join
    }
}

/// A conversation that ended on its own, not at the window's request.
#[derive(Clone, Serialize, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
struct Failed {
    /// Whether a `failure` event has already told the window why.
    explained: bool,
}

#[derive(Clone, Serialize, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
struct ConversationEvent {
    attempt: String,
    update: ConversationUpdate,
}

#[derive(Clone, Serialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export, export_to = "../ui/js/bindings/")]
enum ConversationUpdate {
    Opening { opening: synod::session::Opening },
    Event { event: super::sink::SynodEvent },
    ExchangeDone,
    Ended { failed: Failed },
}

/// Publishes only while this generation owns the conversation slot.
#[derive(Clone)]
pub(crate) struct Emitter {
    app: AppHandle,
    slot: Arc<Mutex<Owner>>,
    generation: u64,
}

impl Emitter {
    /// Whether this generation still occupies the slot.
    fn current(&self) -> bool {
        self.slot
            .lock_ignore_poison()
            .owned_by(self.generation)
            .is_some()
    }

    fn opening(&self, opening: synod::session::Opening) {
        self.emit(ConversationUpdate::Opening { opening });
    }

    pub(crate) fn event(&self, event: super::sink::SynodEvent) {
        self.emit(ConversationUpdate::Event { event });
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "generation lookup and publication must be atomic"
    )]
    fn emit(&self, update: ConversationUpdate) {
        let held = self.slot.lock_ignore_poison();
        if let Some(handle) = held.owned_by(self.generation) {
            self.publish(handle, update);
        }
    }

    // The handle borrows the owner lock, so it is held throughout.
    fn publish(&self, handle: &Handle, update: ConversationUpdate) {
        let _ = self.app.emit(
            "conversation-event",
            ConversationEvent {
                attempt: handle.attempt.clone(),
                update,
            },
        );
    }

    /// Name what the shell itself is doing in the status bar's one station,
    /// before there is an agent whose states to relay.  The shell's own
    /// narration runs from the worker's first breath to the opening, where
    /// `ready` hands the station over.
    fn state(&self, label: impl Into<String>, pending: bool) {
        self.event(super::sink::SynodEvent::State {
            label: label.into(),
            pending,
        });
    }

    /// Clears registration and publishes its end in one critical section.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the slot must stay held across the emit, not just the take, so a successor can never claim it between clearing and announcing"
    )]
    fn ended(&self, failed: Failed) {
        let mut held = self.slot.lock_ignore_poison();
        if let Some(handle) = held.owned_by(self.generation) {
            self.publish(handle, ConversationUpdate::Ended { failed });
            *held = Owner::default();
        }
    }
}

/// A folder the user picked, as the window holds it.
#[derive(Serialize, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub struct PickedFolder {
    /// The native path, which the window hands back as it came, to
    /// `start_conversation` and `job_report`, and never shows.
    path: PathBuf,
    /// The `/`-spelled name the window shows.
    label: String,
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
pub async fn choose_folder(app: AppHandle) -> Option<PickedFolder> {
    use tauri_plugin_dialog::DialogExt;
    app.dialog()
        .file()
        .set_title("Choose the folder for this job")
        .blocking_pick_folder()
        .and_then(|picked| picked.into_path().ok())
        .map(|path| PickedFolder {
            label: shown(&path),
            path,
        })
}

/// The provider/model menu the window offers before starting: one entry
/// per account this computer has credentials for.
///
/// Answers instantly from whatever [`super::Accounts`] already has cached —
/// a fresh disk entry carried over from an earlier run, or nothing at all,
/// which the menu shows as loading — and kicks off
/// [`super::refresh_menu_async`], which emits the result as
/// `models-refreshed` unconditionally, so every list shown loading has a
/// listing on its way.
///
/// # Errors
/// Returns the credential scrub's own failure, if startup could not
/// resolve one.
#[tauri::command]
pub fn list_models(
    app: AppHandle,
    accounts: State<'_, super::Accounts>,
) -> Result<synod::session::ModelMenu, String> {
    let Holdings { store, catalog, .. } = accounts.resolved()?;
    let instant = synod::session::menu(store, catalog);
    super::refresh_menu_async(&app);
    Ok(instant)
}

/// Start the conversation: open `folder` on its own worker thread and hold
/// it open, ready for [`send_message`], publishing correlated opening,
/// narration, exchange completion and end through `conversation-event`.
///
/// This returns as soon as the thread is spawned; the conversation plays
/// out over the events from here on.  Calling this again while a
/// conversation is already running supersedes it, by generation, rather
/// than refusing — see [`Handle`].
///
/// `attempt` correlates every update; it grants no authority.
///
/// # Errors
/// Returns a plain sentence if credentials failed or the window is closing.
#[tauri::command]
pub fn start_conversation(
    app: AppHandle,
    state: State<'_, Running>,
    accounts: State<'_, super::Accounts>,
    folder: PathBuf,
    choice: Choice,
    attempt: String,
) -> Result<(), String> {
    accounts.resolved()?;
    spawn_conversation(app, state.0.clone(), folder, choice, attempt)
}

/// Send one message into the conversation `attempt` started.
///
/// # Errors
/// Returns a plain sentence if there is no conversation running, if the one
/// running is not `attempt`'s (a restart replaced it), or if it has already
/// finished and cannot take this message.
#[tauri::command]
#[allow(
    clippy::significant_drop_tightening,
    reason = "the lock must stay held across the send, not just the lookup, so a restart cannot swap the handle out from under it"
)]
pub fn send_message(
    state: State<'_, Running>,
    attempt: String,
    message: String,
) -> Result<(), String> {
    let held = state.0.lock_ignore_poison();
    let handle = held
        .current()
        .ok_or_else(|| "There is no conversation running to send this to.".to_string())?;
    if handle.attempt != attempt {
        return Err(
            "This message was written for a conversation that a newer one has replaced, \
             so nothing was sent."
                .to_string(),
        );
    }
    handle
        .sender
        .send(message)
        .map_err(|_| "The assistant has already finished; nothing was sent.".to_string())
}

/// Act on the file a change card names — the card's one button.
///
/// The window names the card by its place in the report it was given and
/// the name it showed, never by a path: the guest wrote every file a card
/// names, so the shell finds the file itself, inside the granted folder, and
/// refuses a link (see [`super::review::file_to_open`]).  A document of a
/// kind its application reads as data is opened with the user's own
/// application for it; anything else is shown selected in the file manager,
/// and nothing is run.  Either way the file is first marked as having come
/// from outside this computer (see [`mark_from_elsewhere`]).
///
/// # Errors
/// Returns a sentence naming the file if the card no longer matches the
/// report, the file is not a plain file inside the folder, or the system
/// cannot open or show it.
#[tauri::command]
pub fn open_file(
    app: AppHandle,
    review: State<'_, super::review::Review>,
    index: usize,
    path: String,
) -> Result<(), String> {
    use super::review::OpenAs;
    use tauri_plugin_opener::OpenerExt;
    let target = super::review::file_to_open(&review, index, &path)?;
    mark_from_elsewhere(&target.path);
    match target.how {
        OpenAs::Open => open_with_default(&target.path),
        OpenAs::Reveal => app
            .opener()
            .reveal_item_in_dir(&target.path)
            .map_err(|e| format!("Could not show {path} in its folder: {e}")),
    }
}

/// Mark a file the guest wrote as having come from the internet, so the
/// protections Windows gives a downloaded file apply to it too: the system
/// warns before running it, and Office opens it in Protected View.  The mark
/// is the `Zone.Identifier` stream a browser writes beside a download.
///
/// A failure is only logged, and the file is still opened: a folder on a
/// file system without alternate streams (FAT, many network shares) cannot
/// carry the mark at all, and refusing every file there would leave the
/// card's button dead for no protection gained.
#[cfg(windows)]
#[allow(
    clippy::disallowed_macros,
    reason = "[silent:synod-gui] synod carries no ral shell to print through"
)]
fn mark_from_elsewhere(path: &Path) {
    use crate::site::{Silent, site};

    let mut stream = path.as_os_str().to_os_string();
    stream.push(":Zone.Identifier");
    let written = site!(
        Silent::MarkOfTheWeb,
        std::fs::write(&stream, "[ZoneTransfer]\r\nZoneId=3\r\n")
    );
    if let Err(e) = written {
        eprintln!(
            "synod: could not mark {} as from elsewhere: {e}",
            shown(path)
        );
    }
}

/// macOS's counterpart is the `com.apple.quarantine` extended attribute,
/// which Gatekeeper reads; it is not written yet.
#[cfg(not(windows))]
fn mark_from_elsewhere(_path: &Path) {}

/// The link a click in an assistant message may hand to the system, or a
/// plain sentence refusing it.
///
/// The assistant's words are untrusted, and the system would run whatever a
/// bare name or a share path named, so only a web page (`http`, `https`,
/// with a host) or an email address (`mailto`) is let through.  A relative
/// name, a `file:` link, a Windows path read as a scheme (`C:\…`), and every
/// other scheme are refused.
///
/// # Errors
/// A sentence naming the link when it is not one of those.
pub(crate) fn link_to_follow(raw: &str) -> Result<url::Url, String> {
    let refuse = || {
        format!("Synod opens only web and email links, and {raw} is neither, so it was not opened.")
    };
    let url = url::Url::parse(raw).map_err(|_| refuse())?;
    match url.scheme() {
        "http" | "https" if url.host().is_some() => Ok(url),
        "mailto" => Ok(url),
        _ => Err(refuse()),
    }
}

/// Open a link the assistant wrote in the user's own browser — a hyperlink
/// in a rendered assistant message, handed here rather than followed inside
/// the window, whose one document is the conversation and must never
/// navigate away from it.  Only web and email links are opened; see
/// [`link_to_follow`].
///
/// # Errors
/// Returns a sentence naming the link if it is not a web or email link, or
/// if the system cannot open it.
#[tauri::command]
pub fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let link = link_to_follow(&url)?;
    app.opener()
        .open_url(link.as_str(), None::<&str>)
        .map_err(|e| format!("Could not open {url}: {e}"))
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
    slot: Arc<Mutex<Owner>>,
    folder: PathBuf,
    choice: Choice,
    attempt: String,
) -> Result<(), String> {
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);

    let mut held = slot.lock_ignore_poison();
    let registration = held
        .open()
        .ok_or_else(|| "The window is closing; a new conversation cannot start.".to_string())?;
    let superseded = registration.take().map(Handle::stop);

    let baseline_stop = synod::workspace::manifest::Stop::default();
    let (sender, receiver) = mpsc::channel();
    let emitter = Emitter {
        app,
        slot: slot.clone(),
        generation,
    };
    let thread_baseline_stop = baseline_stop.clone();
    let join = std::thread::spawn(move || {
        run_conversation(
            emitter,
            superseded,
            folder,
            choice,
            receiver,
            thread_baseline_stop,
        );
    });
    *registration = Some(Handle {
        generation,
        attempt,
        sender,
        join,
        baseline_stop,
    });
    drop(held);
    Ok(())
}

/// The worker thread's whole body: first join whatever conversation this
/// one superseded, so its closing walk is finished before this one
/// walks the folder's baseline; then, if a later generation has since
/// claimed the slot out from under this one (restart-spam), stand down
/// quietly rather than boot a machine nobody is waiting on.  Otherwise run
/// the conversation, and announce its failure through the gated [`Emitter`].
/// A panicking conversation is reported the same as any other failure to
/// start or run.
///
/// The join is narrated because it can outlast a person's patience: the
/// superseded generation finishes booting its machine and shuts it down
/// before it returns, and the window has already reset its transcript and
/// is waiting on this one.
fn run_conversation(
    emitter: Emitter,
    superseded: Option<JoinHandle<()>>,
    folder: PathBuf,
    choice: Choice,
    receiver: mpsc::Receiver<String>,
    baseline_stop: synod::workspace::manifest::Stop,
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
        converse(&emitter, &folder, choice, &receiver, &baseline_stop)
    }))
    .unwrap_or(Err(Failed { explained: false }));

    // `Ok` means the sender was dropped, which only a restart or the close
    // does, and either has already vacated the slot: nothing to announce.
    if let Err(failed) = ended {
        emitter.ended(failed);
    }
}

/// Open the folder, narrate the opening, then drive one exchange per
/// message until the sender is dropped — a restart or the window closing —
/// and shut the machine down.
///
/// # Errors
/// [`Failed`] when the conversation cannot be opened.
fn converse(
    emitter: &Emitter,
    picked: &Path,
    choice: Choice,
    receiver: &mpsc::Receiver<String>,
    baseline_stop: &synod::workspace::manifest::Stop,
) -> Result<(), Failed> {
    let mut sink = super::sink::TauriSink::new(emitter.clone());

    let accounts = emitter.app.state::<super::Accounts>();
    // start_conversation already checked this before spawning the thread;
    // a failure here means the credential scrub's outcome flipped under us,
    // which cannot happen — but a worker that finds no store stands down
    // rather than panics.
    let Ok(holdings) = accounts.resolved() else {
        return Err(Failed { explained: false });
    };

    // Narrated because it is silent otherwise: on a large folder or a
    // network share, the baseline stat-walk is the one long minute a user
    // meets before anything else has had a chance to say something.  Owned
    // rather than borrowed because the walk runs on its own thread, on past
    // `begin`'s return.
    let walk_emitter = emitter.clone();
    let report_walk = Box::new(move |files: u64| {
        if files == 1 || files.is_multiple_of(500) {
            walk_emitter.state(
                format!("reading the folder — {files} files seen so far"),
                true,
            );
        }
    });
    let (mut conversation, opening) =
        match Conversation::begin(picked, holdings, choice, baseline_stop, report_walk) {
            Ok(begun) => begun,
            Err(e) => {
                emitter.event(super::sink::SynodEvent::Failure { message: e });
                return Err(Failed { explained: true });
            }
        };

    emitter.opening(opening);
    emitter.state("ready", false);

    // A fresh conversation opens on a fresh report: whatever the last one
    // left held describes a folder this window is no longer showing.
    let review = emitter.app.state::<super::review::Review>();
    super::review::forget(&review);

    while let Ok(message) = receiver.recv() {
        if let Err(e) = conversation.exchange(message, &mut sink) {
            emitter.event(super::sink::SynodEvent::Failure { message: e });
        }
        // Held even after a failed exchange: the closing walk ran
        // regardless, and whatever changed before the failure is already in
        // the report the window will now ask for.
        if let Some(report) = conversation.report() {
            super::review::hold(&review, picked, report);
        }
        emitter.emit(ConversationUpdate::ExchangeDone);
    }

    let _ = conversation.end();
    Ok(())
}

/// Hand `path` to the user's default application for that file.  The system
/// runs whatever `path` names, so the one caller, [`open_file`], passes only
/// a document it has already checked.
fn open_with_default(path: &Path) -> Result<(), String> {
    tauri_plugin_opener::open_path(path, None::<&str>)
        .map_err(|e| format!("Could not open {}: {e}", shown(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_and_email_links_are_followed() {
        for link in [
            "https://example.com/page?q=1",
            "http://example.com",
            "HTTPS://Example.com/",
            "mailto:someone@example.com",
        ] {
            assert!(link_to_follow(link).is_ok(), "{link}");
        }
    }

    #[test]
    fn anything_that_could_run_a_program_is_refused() {
        for link in [
            "cmd.exe",
            "calc",
            "./run.bat",
            "../x.exe",
            r"\\host\share\x.exe",
            "//host/share/x.exe",
            r"C:\Windows\System32\cmd.exe",
            "C:/Windows/System32/cmd.exe",
            "file:///C:/Windows/System32/cmd.exe",
            "file://host/share/x.exe",
            "javascript:alert(1)",
            "ms-settings:",
            "search-ms:query=x",
            "vbscript:x",
            "tel:123",
            "http://",
            "",
        ] {
            assert!(link_to_follow(link).is_err(), "{link}");
        }
    }
}
