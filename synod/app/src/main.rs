//! Synod's desktop shell.
//!
//! One window, three states: choose a folder and describe a job; watch
//! the assistant work; then read what it did and put back anything that
//! isn't wanted.  The window is deliberately spare — a secretary, not a
//! programmer, sits in front of it, so nothing here says "agent",
//! "session", "model", or "machine".
//!
//! The assistant changes the files in the folder directly; the safety net
//! is undo, not a gate before the work becomes real.  So the last screen
//! is a retrospective — the assistant's own account of what it did, and a
//! way to put any file (or the whole job) back the way it was.
//!
//! This crate is the *host* half of that shell.  The frontend is
//! hand-written HTML/CSS/JS under [`ui/`](../ui), embedded at build time
//! (there is no bundler and no web server); the Rust side is a thin set of
//! commands the window calls:
//!
//! - the folder picker, the child-process run, and opening files with the
//!   user's own applications, in [`commands`];
//! - the job report and the undo actions, in [`review`], over the
//!   workspace vocabulary in `synod::workspace` — the checkpoint, the
//!   change set, and the conflict-checked restore.
//!
//! The real run is driven by spawning the `synod` command-line program as
//! a child and streaming its output into the window; the shell never links
//! the agent in-process.

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

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(commands::RunningJob::default())
        .manage(review::Review::default())
        .invoke_handler(tauri::generate_handler![
            commands::choose_folder,
            commands::start_job,
            commands::stop_job,
            commands::open_file,
            review::open_earlier,
            review::job_report,
            review::undo_file,
            review::undo_all,
        ])
        .run(tauri::generate_context!())
        .expect("synod's window could not start");
}
