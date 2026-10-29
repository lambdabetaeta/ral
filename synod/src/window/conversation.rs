//! Workers join their predecessors before opening; publication requires generation ownership.

use ral_core::path::shown;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::state::{Accounts, Handle, Held, Inlet, Owner, Review, Running};
use crate::assistant::conversation::Conversation;
use crate::assistant::gate::{self, Seat, Stage};
use crate::folder::manifest::Stop;
use crate::wire::{
    AgentStateDto, Choice, ConversationEvent, ConversationUpdate, JobId, ModelMenu, Opening,
    PickedFolder, Preparation, SynodEvent, WindowReport,
};
use exarch::port::Port;
use exarch::provider::Holdings;
use exarch::record::AgentId;
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

    pub(crate) fn failure(&self, message: String) {
        publish_failure(
            &self.slot,
            &self.app.state::<Review>(),
            self.generation,
            |handle| {
                publish(
                    &self.app,
                    handle,
                    ConversationUpdate::Event {
                        event: SynodEvent::Failure { message },
                    },
                );
            },
        );
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "generation lookup, report replacement and publication must be atomic"
    )]
    fn job_done(&self, job: JobId, stopped: bool, folder: &Path, report: Option<WindowReport>) {
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
                ConversationUpdate::JobDone {
                    job,
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
        reason = "generation lookup and publication must be atomic"
    )]
    fn job_began(&self, job: JobId) -> bool {
        let held = self.slot.lock_ignore_poison();
        let Some(handle) = held.owned_by(self.generation) else {
            return false;
        };
        publish(&self.app, handle, ConversationUpdate::JobBegan { job });
        true
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
    fn install(&self, port: Port, agent: AgentId) -> bool {
        let mut held = self.slot.lock_ignore_poison();
        let Some(handle) = held.owned_by_mut(self.generation) else {
            return false;
        };
        for message in handle.inlet.install(port.clone(), agent) {
            let _ = port.submit(agent, message);
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

#[allow(
    clippy::significant_drop_tightening,
    reason = "generation lookup, report invalidation and publication must be atomic"
)]
fn publish_failure(
    slot: &Mutex<Owner>,
    review: &Review,
    generation: u64,
    publish: impl FnOnce(&Handle),
) {
    let held = slot.lock_ignore_poison();
    if let Some(handle) = held.owned_by(generation) {
        *review.0.lock_ignore_poison() = None;
        publish(handle);
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
#[allow(clippy::unnecessary_wraps, reason = "preserve the IPC result contract")]
#[allow(
    clippy::significant_drop_tightening,
    reason = "stop and queue publication require generation ownership"
)]
pub(crate) fn stop_job(
    app: AppHandle,
    state: State<'_, Running>,
    attempt: String,
    job: JobId,
) -> Result<Vec<String>, String> {
    let held = state.0.lock_ignore_poison();
    let Some(handle) = held.current().filter(|handle| handle.attempt == attempt) else {
        return Ok(Vec::new());
    };
    let recalled = match &handle.inlet {
        Inlet::Live { port, agent } => port
            .stop(*agent, job.into())
            .ok()
            .flatten()
            .unwrap_or_default(),
        Inlet::Booting(_) => Vec::new(),
    };
    publish(
        &app,
        handle,
        ConversationUpdate::Queue {
            messages: handle.inlet.queued(),
        },
    );
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

    let ended = catch_driver(
        || drive(&emitter, &holdings, &folder, choice, &baseline_stop),
        || emitter.failure("The assistant's conversation driver stopped unexpectedly.".into()),
    );

    // Ok means restart or close already vacated the slot; nothing remains to announce.
    if let Err(explained) = ended {
        emitter.ended(explained);
    }
}

fn catch_driver(
    run: impl FnOnce() -> Result<(), bool>,
    failure: impl FnOnce(),
) -> Result<(), bool> {
    panic::catch_unwind(panic::AssertUnwindSafe(run)).unwrap_or_else(|_| {
        failure();
        Err(true)
    })
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
                emitter.failure(e);
                return Err(true);
            }
        };

    emitter.opening(opening);
    if !emitter.install(conversation.port(), Seat::root(&conversation)) {
        return finish_conversation(emitter, conversation, Ok(()));
    }
    if let Err(e) = conversation.settle_baseline() {
        if !emitter.current() {
            return finish_conversation(emitter, conversation, Ok(()));
        }
        emitter.failure(e);
        conversation.invalidate();
        return finish_conversation(emitter, conversation, Err(true));
    }
    emitter.event(SynodEvent::State {
        state: AgentStateDto::Ready,
    });

    let stage = Seated {
        emitter,
        folder: picked,
    };
    let ended = gate::run(&mut conversation, &stage, &mut sink);
    finish_conversation(emitter, conversation, ended)
}

/// The emitter, bound to the folder its reports describe.
struct Seated<'a> {
    emitter: &'a Emitter,
    folder: &'a Path,
}

impl Stage for Seated<'_> {
    fn current(&self) -> bool {
        self.emitter.current()
    }

    fn failure(&self, message: String) {
        self.emitter.failure(message);
    }

    fn job_began(&self, job: JobId) -> bool {
        self.emitter.job_began(job)
    }

    fn job_done(&self, job: JobId, stopped: bool, report: Option<WindowReport>) {
        self.emitter.job_done(job, stopped, self.folder, report);
    }
}

fn finish_conversation(
    emitter: &Emitter,
    conversation: Conversation,
    result: Result<(), bool>,
) -> Result<(), bool> {
    match conversation.end() {
        Ok(()) => result,
        Err(message) => {
            emitter.failure(message);
            Err(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_withdraws_current_review_but_never_a_successors_review() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let slot = Mutex::new(Owner::Open(Some(Handle {
            generation: 2,
            attempt: "next".into(),
            inlet: Inlet::Booting(vec![]),
            join: std::thread::spawn(|| {}),
            baseline_stop: Stop::default(),
        })));
        let review = Review(Mutex::new(Some(Held {
            folder: PathBuf::from("folder"),
            report: WindowReport {
                files: vec![],
                unreadable: vec![],
            },
        })));
        let published = AtomicBool::new(false);
        publish_failure(&slot, &review, 1, |_| {
            published.store(true, Ordering::SeqCst);
        });
        assert!(!published.load(Ordering::SeqCst));
        assert!(review.0.lock_ignore_poison().is_some());
        publish_failure(&slot, &review, 2, |_| {
            assert!(review.0.lock_ignore_poison().is_none());
            published.store(true, Ordering::SeqCst);
        });
        assert!(published.load(Ordering::SeqCst));
    }

    #[test]
    fn a_driver_panic_publishes_failure_before_ending() {
        let failed = std::cell::Cell::new(false);
        let outcome = catch_driver(|| panic!("walk panicked"), || failed.set(true));
        assert_eq!(outcome, Err(true));
        assert!(failed.get());
    }
}
