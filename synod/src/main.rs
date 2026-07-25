//! Synod's desktop shell.
//!
//! It opens with nothing to choose but a folder.  Pick one, and the window
//! becomes a conversation with the assistant that runs in it — a message
//! in, the assistant's narration streaming back, and a change report
//! beside it that refreshes after every exchange, with undo a click away.
//! Closing the window ends the conversation.  The window is deliberately
//! spare — a secretary, not a programmer, sits in front of it, so nothing
//! here says "agent", "session", "model", or "machine".
//!
//! The assistant changes the files in the folder directly; the safety net
//! is undo, not a gate before the work becomes real.  So the change report
//! is not a final screen but a standing one, open beside the conversation
//! for its whole life — the assistant's own account of what it has done so
//! far, and a way to put any file (or everything) back the way it was.
//!
//! This binary is the *host* half of that shell — and, unlike a shell over
//! a separate program, it IS the agent: it hosts the conversation
//! in-process, starts a machine over the chosen folder, and talks to it
//! directly.  The frontend is hand-written HTML/CSS/JS under
//! [`ui/`](../ui), embedded at build time (there is no bundler and no web
//! server); the Rust side is a thin set of commands the window calls, all
//! under [`shell`]:
//!
//! - the folder picker, the model menu, the conversation itself, sending it
//!   messages, starting again, and opening files with the user's own
//!   applications, in [`shell::commands`];
//! - signing in to a `ChatGPT` plan from the opening screen, in
//!   [`shell::signin`];
//! - the change report and the undo actions, in [`shell::review`], over the
//!   workspace vocabulary in `synod::workspace` — the checkpoint, the
//!   change set, and the conflict-checked restore.
//!
//! The conversation is a [`synod::session::Conversation`], held open on its
//! own worker thread for as long as the window wants it, its narration
//! streamed into the window as it works.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "synod's binary is a desktop application, not the ral shell; the clippy.toml invariants target ral-core's Shell path/cwd/fs discipline, which this crate is nowhere near"
)]
#![allow(
    clippy::needless_pass_by_value,
    reason = "a `#[tauri::command]` handler receives its deserialized arguments and its injected State/AppHandle by value — the command ABI does not admit borrowed parameters — so the pass-by-value is the framework's shape, not ours to tighten"
)]

mod shell;

use shell::{commands, review, signin};

use exarch::provider::credential::CredentialStore;
use exarch::provider::models::{LiveSource, ModelCatalog};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use tauri::Manager;

/// The credential scrub's outcome, resolved once here — while the process
/// is still single-threaded, [`synod::session::prepare`] requires — paired
/// with the model catalog built from it, and held as Tauri state for the
/// app's whole life.  Every command that needs a working model account
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
struct Accounts(Result<(Mutex<CredentialStore>, Mutex<ModelCatalog<LiveSource>>), String>);

/// Whether this run has already started its one background model refresh.
///
/// [`commands::list_models`] swaps this `false` → `true` and spawns the
/// refresh only on the swap that lands it `true`, so calling the command
/// again — a second picker open, a restart's own menu — can never race a
/// second fetch against the first.  One refresh per run is all the catalog
/// is worth: its disk cache already carries its own day-long freshness
/// window, so a run that has fetched once has nothing left to gain from
/// fetching again.
struct RefreshGate(AtomicBool);

fn main() {
    if let Some(code) = exarch::dispatch_pre_main() {
        std::process::exit(i32::from(code));
    }
    let accounts = Accounts(synod::session::prepare().map(|store| {
        let catalog = Mutex::new(ModelCatalog::new(
            LiveSource::new(&store),
            synod::session::SYNOD,
        ));
        (Mutex::new(store), catalog)
    }));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(commands::Running::default())
        .manage(accounts)
        .manage(RefreshGate(AtomicBool::new(false)))
        .manage(review::Review::default())
        .manage(signin::SignIn::default())
        .invoke_handler(tauri::generate_handler![
            commands::choose_folder,
            commands::list_models,
            commands::start_conversation,
            commands::send_message,
            commands::open_file,
            commands::open_url,
            signin::sign_in,
            signin::cancel_sign_in,
            review::open_earlier,
            review::job_report,
            review::undo_file,
            review::undo_all,
        ])
        // Closing the window ends the conversation cleanly rather than
        // orphaning its machine: hold the close, end the conversation on
        // its own thread (dropping its sender is its own final path — the
        // worker finishes its exchange, shuts its machine down, then
        // exits), and only then let the window actually close.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let slot = window.state::<commands::Running>().slot();
                let window = window.clone();
                std::thread::spawn(move || {
                    commands::end_conversation(&slot);
                    let _ = window.destroy();
                });
            }
        })
        .run(tauri::generate_context!())
        .expect("synod's window could not start");
}
