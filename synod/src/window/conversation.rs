//! Workers join their predecessors before opening; publication requires generation ownership.

use ral_core::path::shown;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use super::state::{Accounts, Handle, Held, Owner, Review, Running};
use crate::assistant::conversation::Conversation;
use crate::folder::manifest::Stop;
use crate::wire::{
    Choice, ConversationEvent, ConversationUpdate, ModelMenu, Opening, PickedFolder, SynodEvent,
    WindowReport,
};
use exarch::provider::Holdings;
use ral_core::sync::LockExt;
use tauri::{AppHandle, Emitter as _, Manager, State};

/// Publishes only while this generation owns the conversation slot.
#[derive(Clone)]
pub(crate) struct Emitter {
    app: AppHandle,
    slot: Arc<Mutex<Owner>>,
    generation: u64,
}

impl Emitter {
    fn current(&self) -> bool {
        self.slot
            .lock_ignore_poison()
            .owned_by(self.generation)
            .is_some()
    }

    fn opening(&self, opening: Opening) {
        self.emit(ConversationUpdate::Opening { opening });
    }

    pub(crate) fn event(&self, event: SynodEvent) {
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

    #[allow(
        clippy::significant_drop_tightening,
        reason = "generation lookup, report replacement and publication must be atomic"
    )]
    fn exchange_done(&self, folder: &Path, report: Option<WindowReport>) {
        let held = self.slot.lock_ignore_poison();
        if let Some(handle) = held.owned_by(self.generation) {
            *self.app.state::<Review>().0.lock_ignore_poison() =
                report.as_ref().map(|report| Held {
                    folder: folder.to_path_buf(),
                    report: report.clone(),
                });
            self.publish(handle, ConversationUpdate::ExchangeDone { report });
        }
    }

    // Handle borrows the owner lock, held throughout publication.
    fn publish(&self, handle: &Handle, update: ConversationUpdate) {
        let _ = self.app.emit(
            crate::seam::ipc::CONVERSATION_EVENT,
            ConversationEvent {
                attempt: handle.attempt.clone(),
                update,
            },
        );
    }

    fn state(&self, label: impl Into<String>, pending: bool) {
        self.event(SynodEvent::State {
            label: label.into(),
            pending,
        });
    }

    /// Clears registration and publishes its end in one critical section.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the slot must stay held across the emit, not just the take, so a successor can never claim it between clearing and announcing"
    )]
    fn ended(&self, explained: bool) {
        let mut held = self.slot.lock_ignore_poison();
        if let Some(handle) = held.owned_by(self.generation) {
            self.publish(handle, ConversationUpdate::Ended { explained });
            *held = Owner::default();
        }
    }
}

/// Async keeps the blocking native dialog off the window event loop.
#[tauri::command]
pub(crate) async fn choose_folder(app: AppHandle) -> Option<PickedFolder> {
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

#[tauri::command]
pub(crate) fn list_models(
    app: AppHandle,
    accounts: State<'_, Accounts>,
) -> Result<ModelMenu, String> {
    let holdings = accounts.resolved()?;
    let Holdings { store, catalog, .. } = holdings;
    let instant = crate::assistant::menu::menu(store, catalog);
    super::accounts::refresh_menu_async(&app, holdings.clone());
    Ok(instant)
}

/// Attempt correlates updates; it grants no authority.
#[tauri::command]
pub(crate) fn start_conversation(
    app: AppHandle,
    state: State<'_, Running>,
    accounts: State<'_, Accounts>,
    folder: PathBuf,
    choice: Choice,
    attempt: String,
) -> Result<(), String> {
    let holdings = accounts.resolved()?.clone();
    spawn_conversation(app, state.0.clone(), holdings, folder, choice, attempt)
}

#[tauri::command]
#[allow(
    clippy::significant_drop_tightening,
    reason = "the lock must stay held across the send, not just the lookup, so a restart cannot swap the handle out from under it"
)]
pub(crate) fn send_message(
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

/// Register atomically; the new worker joins its predecessor outside the owner lock.
fn spawn_conversation(
    app: AppHandle,
    slot: Arc<Mutex<Owner>>,
    holdings: Holdings,
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
    *app.state::<Review>().0.lock_ignore_poison() = None;

    let baseline_stop = Stop::default();
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
            holdings,
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

/// Join before the new baseline, then stand down if superseded; narrate a panic as an end.
fn run_conversation(
    emitter: Emitter,
    superseded: Option<JoinHandle<()>>,
    holdings: Holdings,
    folder: PathBuf,
    choice: Choice,
    receiver: mpsc::Receiver<String>,
    baseline_stop: Stop,
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
        converse(
            &emitter,
            &holdings,
            &folder,
            choice,
            &receiver,
            &baseline_stop,
        )
    }))
    .unwrap_or(Err(false));

    // Ok means restart or close already vacated the slot; nothing remains to announce.
    if let Err(explained) = ended {
        emitter.ended(explained);
    }
}

fn converse(
    emitter: &Emitter,
    holdings: &Holdings,
    picked: &Path,
    choice: Choice,
    receiver: &mpsc::Receiver<String>,
    baseline_stop: &Stop,
) -> Result<(), bool> {
    let mut sink = crate::seam::narration::Narrator::new(|event| emitter.event(event));

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
                emitter.event(SynodEvent::Failure { message: e });
                return Err(true);
            }
        };

    emitter.opening(opening);
    emitter.state("ready", false);

    while let Ok(message) = receiver.recv() {
        if let Err(e) = conversation.exchange(message, &mut sink) {
            emitter.event(SynodEvent::Failure { message: e });
        }
        emitter.exchange_done(picked, conversation.report().map(WindowReport::of));
    }

    let _ = conversation.end();
    Ok(())
}
