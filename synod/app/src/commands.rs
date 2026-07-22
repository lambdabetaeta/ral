//! The real commands the window calls: pick a folder, run the job, stop
//! it, and open a file with the application the user already uses for it.
//!
//! None of this is stubbed.  The folder picker is the platform's own; the
//! run is the `synod` command-line program spawned as a child, with its
//! output streamed into the window line by line; opening a file hands the
//! path to the user's default application through the opener plugin.
//!
//! The one thing that is *not* here is the agent itself.  Synod's shell
//! never links the engine in-process — it drives a child process, so a
//! crash in the run cannot take the window down with it, and the window
//! stays answerable (the stop button works) while the job runs.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

/// The child process for the running job, if one is running.
///
/// Held behind a shared lock so the stop button and the process monitor
/// can both reach it: the monitor watches for the run to finish on its
/// own, [`stop_job`] ends it early, and whichever happens first takes the
/// child and leaves `None` behind.
#[derive(Default, Clone)]
pub struct RunningJob(Arc<Mutex<Option<Child>>>);

/// One line of the run's output, sent to the window as it arrives.  The
/// text is developer-flavoured for now — the window tucks it into a
/// "details" fold under a calm headline — so `stream` marks which pipe it
/// came from without asking the window to interpret it.
#[derive(Clone, Serialize)]
struct JobLine {
    /// `"out"` for the assistant's own narration, `"err"` for the run's
    /// progress and tool trace.
    stream: &'static str,
    line: String,
}

/// The run has ended, one way or another.
#[derive(Clone, Serialize)]
struct JobFinished {
    /// True only when the child exited successfully on its own.
    success: bool,
    /// True when [`stop_job`] ended it rather than the work completing.
    stopped: bool,
}

/// Open the native folder picker and return the chosen folder, or `None`
/// if the user closed the picker without choosing.
///
/// The title is the user's question, not ours: "Choose the folder for
/// this job" is the whole of what synod is about to be given.
#[tauri::command]
pub fn choose_folder(app: AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    app.dialog()
        .file()
        .set_title("Choose the folder for this job")
        .blocking_pick_folder()
        .and_then(|picked| picked.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
}

/// Start the job: spawn `synod <folder> <task>` and stream its output to
/// the window as `job-log` events, then a single `job-finished` event when
/// it ends.
///
/// The run happens in a child process for the reasons in the module note.
/// This returns as soon as the child is spawned; the run's life plays out
/// over the events.
///
/// # Errors
/// Returns a plain sentence if the `synod` program cannot be found beside
/// this one, or if the child cannot be spawned at all.
#[tauri::command]
pub fn start_job(
    app: AppHandle,
    job: State<'_, RunningJob>,
    folder: String,
    task: String,
) -> Result<(), String> {
    let program = locate_synod()
        .ok_or_else(|| "Could not find the synod program on this computer.".to_string())?;

    let mut child = Command::new(&program)
        .arg(&folder)
        .arg(&task)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start the job: {e}"))?;

    // The two pipes are drained on their own threads so neither can wedge
    // the run by filling its buffer, and so the window sees output as it
    // happens rather than in one lump at the end.
    let stdout = child.stdout.take().expect("stdout was requested as a pipe");
    let stderr = child.stderr.take().expect("stderr was requested as a pipe");
    stream_lines(app.clone(), stdout, "out");
    stream_lines(app.clone(), stderr, "err");

    let handle = job.0.clone();
    *guard(&handle) = Some(child);
    watch_for_end(app, handle);
    Ok(())
}

/// Stop the running job, if there is one.  A stopped run is not a failed
/// run: the monitor reports it as stopped, and the window keeps whatever
/// the assistant managed to change for the report screen.
#[tauri::command]
pub fn stop_job(job: State<'_, RunningJob>) {
    // Take the child out from under the lock first, so the guard is
    // released before the kill/wait rather than held across it.
    let running = guard(&job.0).take();
    if let Some(mut child) = running {
        let _ = child.kill();
        let _ = child.wait();
    }
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

/// Lock the child slot, recovering the guard even if a thread panicked
/// while holding it — a poisoned lock here means only that a reader thread
/// unwound, which must not wedge the stop button.
fn guard(slot: &Arc<Mutex<Option<Child>>>) -> std::sync::MutexGuard<'_, Option<Child>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Drain one of the child's pipes on its own thread, emitting each line to
/// the window.  The thread ends when the pipe closes, which is when the
/// child exits.
fn stream_lines(app: AppHandle, stream: impl Read + Send + 'static, which: &'static str) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            let _ = app.emit(
                "job-log",
                JobLine {
                    stream: which,
                    line,
                },
            );
        }
    });
}

/// Watch the child until it ends and announce how it ended.
///
/// The monitor polls rather than blocking on `wait`, so the shared slot
/// stays free for the stop button to take the child between ticks.  If the
/// slot is emptied out from under it, the run was stopped by hand; if the
/// child exits on its own, its status decides success.
fn watch_for_end(app: AppHandle, slot: Arc<Mutex<Option<Child>>>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(150));
            let mut held = guard(&slot);
            let finished = match held.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => Some(JobFinished {
                        success: status.success(),
                        stopped: false,
                    }),
                    Ok(None) => None,
                    Err(_) => Some(JobFinished {
                        success: false,
                        stopped: false,
                    }),
                },
                None => Some(JobFinished {
                    success: false,
                    stopped: true,
                }),
            };
            if let Some(outcome) = finished {
                *held = None;
                drop(held);
                let _ = app.emit("job-finished", outcome);
                break;
            }
        }
    });
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

/// Find the `synod` command-line program.
///
/// It ships beside this one, so the common case is a sibling of the
/// running binary — and because a development `cargo build` of the app and
/// of the CLI land in the same profile directory, that same look also
/// finds it during development.  The walk up to the workspace `target`
/// directory afterwards covers only the odd case where the app was built
/// but the CLI has not been yet, and tries both profiles.
fn locate_synod() -> Option<PathBuf> {
    let name = if cfg!(windows) { "synod.exe" } else { "synod" };
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;

    let sibling = dir.join(name);
    if sibling.is_file() {
        return Some(sibling);
    }

    let mut cursor = dir;
    while let Some(parent) = cursor.parent() {
        if parent.file_name().is_some_and(|n| n == "target") {
            for profile in ["debug", "release"] {
                let candidate = parent.join(profile).join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        cursor = parent;
    }
    None
}
