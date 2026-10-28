//! Workers join their predecessors before opening; publication requires generation ownership.

use ral_core::path::shown;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::state::{Accounts, Handle, Held, Inlet, Owner, Review, Running};
use crate::assistant::conversation::Conversation;
use crate::folder::manifest::Stop;
use crate::wire::{
    AgentStateDto, Choice, ConversationEvent, ConversationUpdate, ExchangeId, ModelMenu, Opening,
    PickedFolder, Preparation, SynodEvent, WindowReport,
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
            publish(&self.app, handle, update);
        }
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "generation lookup, report replacement and publication must be atomic"
    )]
    fn exchange_done(
        &self,
        exchange: ExchangeId,
        stopped: bool,
        folder: &Path,
        report: Option<WindowReport>,
    ) {
        let held = self.slot.lock_ignore_poison();
        if let Some(handle) = held.owned_by(self.generation) {
            *self.app.state::<Review>().0.lock_ignore_poison() =
                report.as_ref().map(|report| Held {
                    folder: folder.to_path_buf(),
                    report: report.clone(),
                });
            publish(
                &self.app,
                handle,
                ConversationUpdate::ExchangeDone {
                    exchange,
                    stopped,
                    report,
                },
            );
            publish(
                &self.app,
                handle,
                ConversationUpdate::Queue {
                    messages: handle.inlet.queued(),
                },
            );
        }
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "reservation and publication must be atomic"
    )]
    fn exchange_began(&self) -> Option<(ExchangeId, Arc<super::exchange::Exchanges>)> {
        let held = self.slot.lock_ignore_poison();
        let handle = held.owned_by(self.generation)?;
        let exchange = handle.exchanges.begin();
        publish(
            &self.app,
            handle,
            ConversationUpdate::ExchangeBegan { exchange },
        );
        Some((exchange, handle.exchanges.clone()))
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "queue snapshots and publication must stay ordered"
    )]
    fn queue(&self) {
        let held = self.slot.lock_ignore_poison();
        if let Some(handle) = held.owned_by(self.generation) {
            publish(
                &self.app,
                handle,
                ConversationUpdate::Queue {
                    messages: handle.inlet.queued(),
                },
            );
        }
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "installation and queue publication must be atomic"
    )]
    fn install(&self, helm: exarch::agent::Helm) -> bool {
        let mut held = self.slot.lock_ignore_poison();
        let Some(handle) = held.owned_by_mut(self.generation) else {
            return false;
        };
        for message in handle.inlet.install(helm.clone()) {
            let _ = helm.steer(message);
        }
        publish(
            &self.app,
            handle,
            ConversationUpdate::Queue {
                messages: handle.inlet.queued(),
            },
        );
        true
    }

    fn preparing(&self, stage: Preparation) {
        self.event(SynodEvent::Preparing { stage });
    }

    /// Clears registration and publishes its end in one critical section.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the slot must stay held across the emit, not just the take, so a successor can never claim it between clearing and announcing"
    )]
    fn ended(&self, explained: bool) {
        let mut held = self.slot.lock_ignore_poison();
        if let Some(handle) = held.owned_by(self.generation) {
            publish(&self.app, handle, ConversationUpdate::Ended { explained });
            *held = Owner::default();
        }
    }
}

// Handle borrows the owner lock throughout publication.
fn publish(app: &AppHandle, handle: &Handle, update: ConversationUpdate) {
    let _ = app.emit(
        crate::seam::ipc::CONVERSATION_EVENT,
        ConversationEvent {
            attempt: handle.attempt.clone(),
            update,
        },
    );
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
    reason = "queue mutation and publication must be atomic"
)]
pub(crate) fn send_message(
    app: AppHandle,
    state: State<'_, Running>,
    attempt: String,
    message: String,
) -> Result<(), String> {
    let mut held = state.0.lock_ignore_poison();
    let handle = registered(&mut held, &attempt)?;
    let outcome = handle.inlet.steer(message);
    publish(
        &app,
        handle,
        ConversationUpdate::Queue {
            messages: handle.inlet.queued(),
        },
    );
    outcome
}

#[tauri::command]
#[allow(
    clippy::significant_drop_tightening,
    reason = "stop reservation and queue publication require generation ownership"
)]
pub(crate) async fn stop_exchange(
    app: AppHandle,
    state: State<'_, Running>,
    attempt: String,
    exchange: ExchangeId,
) -> Result<Vec<String>, String> {
    let (generation, helm, delivery) = {
        let held = state.0.lock_ignore_poison();
        let Some(handle) = held.current().filter(|handle| handle.attempt == attempt) else {
            return Ok(Vec::new());
        };
        let Inlet::Live(helm) = &handle.inlet else {
            return Ok(Vec::new());
        };
        let Some(delivery) = handle.exchanges.stop(exchange) else {
            return Ok(Vec::new());
        };
        (handle.generation, helm.clone(), delivery)
    };
    let recalled = helm.stop();
    drop(delivery);
    let held = state.0.lock_ignore_poison();
    if let Some(handle) = held.owned_by(generation) {
        publish(
            &app,
            handle,
            ConversationUpdate::Queue {
                messages: handle.inlet.queued(),
            },
        );
    }
    Ok(recalled)
}

#[tauri::command]
#[allow(
    clippy::significant_drop_tightening,
    reason = "queue mutation and publication must be atomic"
)]
pub(crate) fn recall_queued(
    app: AppHandle,
    state: State<'_, Running>,
    attempt: String,
) -> Result<Vec<String>, String> {
    let mut held = state.0.lock_ignore_poison();
    let handle = registered(&mut held, &attempt)?;
    let recalled = handle.inlet.recall();
    publish(
        &app,
        handle,
        ConversationUpdate::Queue {
            messages: handle.inlet.queued(),
        },
    );
    Ok(recalled)
}

fn registered<'a>(held: &'a mut Owner, attempt: &str) -> Result<&'a mut Handle, String> {
    let handle = held
        .open()
        .and_then(Option::as_mut)
        .ok_or_else(|| "There is no conversation running to send this to.".to_string())?;
    if handle.attempt != attempt {
        return Err(
            "This message was written for a conversation that a newer one has replaced, \
             so nothing was sent."
                .to_string(),
        );
    }
    Ok(handle)
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
    let superseded = registration.take();
    *app.state::<Review>().0.lock_ignore_poison() = None;

    let baseline_stop = Stop::default();
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
            thread_baseline_stop,
        );
    });
    *registration = Some(Handle {
        generation,
        attempt,
        inlet: Inlet::Booting(Vec::new()),
        exchanges: Arc::default(),
        join,
        baseline_stop,
    });
    drop(held);
    Ok(())
}

/// Join before the new baseline, then stand down if superseded; narrate a panic as an end.
fn run_conversation(
    emitter: Emitter,
    superseded: Option<Handle>,
    holdings: Holdings,
    folder: PathBuf,
    choice: Choice,
    baseline_stop: Stop,
) {
    if let Some(old) = superseded {
        emitter.preparing(Preparation::FinishingPrevious);
        let _ = old.stop().join();
    }
    if !emitter.current() {
        return;
    }
    emitter.preparing(Preparation::Starting);

    let ended = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        drive(&emitter, &holdings, &folder, choice, &baseline_stop)
    }))
    .unwrap_or(Err(false));

    // Ok means restart or close already vacated the slot; nothing remains to announce.
    if let Err(explained) = ended {
        emitter.ended(explained);
    }
}

fn drive(
    emitter: &Emitter,
    holdings: &Holdings,
    picked: &Path,
    choice: Choice,
    baseline_stop: &Stop,
) -> Result<(), bool> {
    let mut sink = crate::seam::narration::Narrator::new(|event| {
        let human = matches!(&event, SynodEvent::Human { .. });
        emitter.event(event);
        if human {
            emitter.queue();
        }
    });

    let walk_emitter = emitter.clone();
    let report_walk = Box::new(move |files: u64| {
        if files == 1 || files.is_multiple_of(500) {
            walk_emitter.preparing(Preparation::ReadingFolder { files });
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
    if !emitter.install(conversation.helm()) {
        let _ = conversation.end();
        return Ok(());
    }
    if let Err(e) = conversation.settle_baseline() {
        if !emitter.current() {
            let _ = conversation.end();
            return Ok(());
        }
        emitter.event(SynodEvent::Failure { message: e });
        let _ = conversation.end();
        return Err(true);
    }
    emitter.event(SynodEvent::State {
        state: AgentStateDto::Ready,
    });

    while conversation.await_message() {
        let Some((exchange, exchanges)) = emitter.exchange_began() else {
            break;
        };
        let outcome = conversation.exchange(&mut sink);
        let stopped = exchanges.finish(exchange);
        if !emitter.current() {
            break;
        }
        if let Err(e) = outcome {
            emitter.event(SynodEvent::Failure { message: e });
        }
        emitter.exchange_done(
            exchange,
            stopped,
            picked,
            conversation.report().map(WindowReport::of),
        );
        if conversation.lost() {
            let _ = conversation.end();
            return Err(true);
        }
    }

    let _ = conversation.end();
    Ok(())
}
