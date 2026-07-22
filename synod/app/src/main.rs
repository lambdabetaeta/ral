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
//! This crate is the *host* half of that shell.  The frontend is
//! hand-written HTML/CSS/JS under [`ui/`](../ui), embedded at build time
//! (there is no bundler and no web server); the Rust side is a thin set of
//! commands the window calls:
//!
//! - the folder picker, the conversation's child process, sending it
//!   messages, starting again, and opening files with the user's own
//!   applications, in [`commands`];
//! - the change report and the undo actions, in [`review`], over the
//!   workspace vocabulary in `synod::workspace` — the checkpoint, the
//!   change set, and the conflict-checked restore.
//!
//! The conversation is driven by spawning the `synod` command-line program
//! once as a child, holding it open, and streaming its output into the
//! window; the shell never links the agent in-process.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "synod-app is a desktop application, not the ral shell; the clippy.toml invariants target ral-core's Shell path/cwd/fs discipline, which this crate is nowhere near"
)]
#![allow(
    clippy::needless_pass_by_value,
    reason = "a `#[tauri::command]` handler receives its deserialized arguments and its injected State/AppHandle by value — the command ABI does not admit borrowed parameters — so the pass-by-value is the framework's shape, not ours to tighten"
)]

mod commands;
mod review;

use tauri::Manager;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(commands::RunningChild::default())
        .manage(review::Review::default())
        .invoke_handler(tauri::generate_handler![
            commands::choose_folder,
            commands::start_conversation,
            commands::send_message,
            commands::restart_conversation,
            commands::open_file,
            review::open_earlier,
            review::job_report,
            review::undo_file,
            review::undo_all,
        ])
        // Closing the window ends the conversation cleanly rather than
        // orphaning the child: hold the close, end the child on its own
        // thread (stdin's EOF is its own final path — the closing report,
        // then exit), and only then let the window actually close.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let slot = window.state::<commands::RunningChild>().slot();
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
