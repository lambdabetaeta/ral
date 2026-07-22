//! The real commands the window calls: pick a folder, hold the assistant
//! child open for the conversation, send it messages, start again, and
//! open a file with the application the user already uses for it.
//!
//! None of this is stubbed.  The folder picker is the platform's own; the
//! conversation is the `synod` command-line program spawned once as a
//! child and held open for as long as the window is, with its output
//! streamed into the window line by line and messages written down its
//! stdin; opening a file hands the path to the user's default application
//! through the opener plugin.
//!
//! The one thing that is *not* here is the agent itself.  Synod's shell
//! never links the engine in-process — it drives a child process, so a
//! crash in the run cannot take the window down with it.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

/// The child process for the running conversation, if one is running.
///
/// Held behind a shared lock so [`send_message`], [`restart_conversation`],
/// and the window's own close handler can all reach it, and so the process
/// monitor can clear it out from under them the moment it notices the
/// child is gone on its own.
#[derive(Default, Clone)]
pub struct RunningChild(Arc<Mutex<Option<Child>>>);

impl RunningChild {
    /// The shared slot, for the window's own close handler to end the
    /// conversation with — the one reach into this type from outside the
    /// module.
    pub(crate) fn slot(&self) -> Arc<Mutex<Option<Child>>> {
        self.0.clone()
    }
}

/// One line of the child's output, sent to the window as it arrives.  The
/// text is developer-flavoured for now — the window tucks it into a
/// "details" fold under the assistant's own narration — so `stream` marks
/// which pipe it came from without asking the window to interpret it.
#[derive(Clone, Serialize)]
struct OutputLine {
    /// `"out"` for the assistant's own narration, `"err"` for the run's
    /// progress and tool trace.
    stream: &'static str,
    line: String,
}

/// The child has ended, one way or another — a crash, or a deliberate
/// stop (a restart, or the window closing).
#[derive(Clone, Serialize)]
struct ChildEnded {
    /// True only when the child exited on its own with a success status.
    success: bool,
    /// True when synod-app ended it on purpose rather than the process
    /// dying by itself.
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

/// Start the conversation: spawn `synod <folder>` and hold it open, its
/// stdin ready for [`send_message`], its output streamed to the window as
/// `synod-log` events, and (should it ever end on its own) a single
/// `synod-ended` event.
///
/// This returns as soon as the child is spawned; the conversation plays
/// out over the events from here on.
///
/// # Errors
/// Returns a plain sentence if the `synod` program cannot be found beside
/// this one, or if the child cannot be spawned at all.
#[tauri::command]
pub fn start_conversation(
    app: AppHandle,
    child: State<'_, RunningChild>,
    folder: String,
) -> Result<(), String> {
    let mut spawned = spawn_child(&folder)?;
    let pid = spawned.id();

    // The two pipes are drained on their own threads so neither can wedge
    // the conversation by filling its buffer, and so the window sees
    // output as it happens rather than in one lump at the end.
    let stdout = spawned.stdout.take().expect("stdout was requested as a pipe");
    let stderr = spawned.stderr.take().expect("stderr was requested as a pipe");
    stream_out(app.clone(), stdout);
    stream_err(app.clone(), stderr);

    let slot = child.0.clone();
    *guard(&slot) = Some(spawned);
    watch_for_end(app, slot, pid);
    Ok(())
}

/// Send one message down the running child's stdin, framed exactly as
/// [`synod::session::read_message`] expects: the message's own lines,
/// then a line holding just `.`.
///
/// # Errors
/// Returns a plain sentence if there is no conversation running, or if the
/// message could not be written.
#[tauri::command]
#[allow(
    clippy::significant_drop_tightening,
    reason = "the lock must stay held for `stdin`'s whole write-then-flush, not just the lookup"
)]
pub fn send_message(child: State<'_, RunningChild>, message: String) -> Result<(), String> {
    let mut held = guard(&child.0);
    let running = held
        .as_mut()
        .ok_or_else(|| "There is no conversation running to send this to.".to_string())?;
    let stdin = running
        .stdin
        .as_mut()
        .ok_or_else(|| "The assistant is not ready for a message yet.".to_string())?;
    let could_not = |e| format!("Could not send the message: {e}");
    for line in message.lines() {
        writeln!(stdin, "{line}").map_err(could_not)?;
    }
    writeln!(stdin, ".").map_err(could_not)?;
    stdin.flush().map_err(could_not)
}

/// "Start again": end the running child, if any, and spawn a fresh one
/// over the same folder — a new conversation is a new process, never a
/// cleared one.
///
/// # Errors
/// Returns a plain sentence if the fresh child cannot be spawned.
#[tauri::command]
pub fn restart_conversation(
    app: AppHandle,
    child: State<'_, RunningChild>,
    folder: String,
) -> Result<(), String> {
    kill(&child.0);
    start_conversation(app, child, folder)
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

/// End the conversation cleanly: close the child's stdin — its own
/// end-of-session signal — and give it a few seconds to take its final
/// path (the closing report) and exit on its own before falling back to a
/// kill, so a lost or hung child can never keep the window from closing.
/// Called from the window's own close handler, never the frontend.
pub(crate) fn end_conversation(slot: &Arc<Mutex<Option<Child>>>) {
    let Some(mut running) = guard(slot).take() else {
        return;
    };
    drop(running.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(running.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = running.kill();
    let _ = running.wait();
}

/// End the conversation at once, with no grace period — "start again"'s
/// own half; the fresh child that follows gets its own report from its
/// own fresh `Before` checkpoint, so there is nothing to wait for here.
fn kill(slot: &Arc<Mutex<Option<Child>>>) {
    // Taken out from under the lock first, so the guard is released before
    // the kill/wait rather than held across it.
    let taken = guard(slot).take();
    if let Some(mut running) = taken {
        let _ = running.kill();
        let _ = running.wait();
    }
}

/// Spawn `synod <folder>` with its stdin, stdout, and stderr all piped —
/// stdin for [`send_message`], the other two for the streamed narration.
fn spawn_child(folder: &str) -> Result<Child, String> {
    let program = locate_synod()
        .ok_or_else(|| "Could not find the synod program on this computer.".to_string())?;
    Command::new(&program)
        .arg(folder)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start the assistant: {e}"))
}

/// Lock the child slot, recovering the guard even if a thread panicked
/// while holding it — a poisoned lock here means only that a reader thread
/// unwound, which must not wedge the window.
fn guard(slot: &Arc<Mutex<Option<Child>>>) -> std::sync::MutexGuard<'_, Option<Child>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Drain the child's stdout on its own thread: pure narration, forwarded
/// to the window line by line with no interpretation.
fn stream_out(app: AppHandle, stream: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            let _ = app.emit("synod-log", OutputLine { stream: "out", line });
        }
    });
}

/// Drain the child's stderr on its own thread.  Every line is progress
/// and tool trace forwarded to the window as-is, except
/// [`synod::session::EXCHANGE_DONE`] — the internal signal that an
/// exchange has settled and its report is ready to re-read — which is
/// consumed here and turned into its own `exchange-done` event instead of
/// being shown as a line of narration.
fn stream_err(app: AppHandle, stream: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if line == synod::session::EXCHANGE_DONE {
                let _ = app.emit("exchange-done", ());
                continue;
            }
            let _ = app.emit("synod-log", OutputLine { stream: "err", line });
        }
    });
}

/// Watch the slot until the child bearing `pid` leaves it, one way or
/// another, and announce how.
///
/// Polls rather than blocking on `wait`, so the shared slot stays free for
/// [`send_message`] and a restart between ticks.  Tagged with the pid it
/// was spawned to watch: a restart can replace the slot's occupant while
/// this is still running, and a poll that finds a *different* child there
/// belongs to that child's own, newer watcher — this one retires quietly
/// rather than reporting on a process it was never asked to.
fn watch_for_end(app: AppHandle, slot: Arc<Mutex<Option<Child>>>, pid: u32) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(150));
            let mut held = guard(&slot);
            let outcome = match held.as_mut() {
                Some(running) if running.id() == pid => match running.try_wait() {
                    Ok(Some(status)) => Some(ChildEnded {
                        success: status.success(),
                        stopped: false,
                    }),
                    Ok(None) => None,
                    Err(_) => Some(ChildEnded {
                        success: false,
                        stopped: false,
                    }),
                },
                Some(_) => return, // superseded by a restart; its own watcher reports
                None => Some(ChildEnded {
                    success: false,
                    stopped: true,
                }),
            };
            let Some(outcome) = outcome else { continue };
            if held.as_ref().is_some_and(|running| running.id() == pid) {
                *held = None;
            }
            drop(held);
            let _ = app.emit("synod-ended", outcome);
            return;
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
