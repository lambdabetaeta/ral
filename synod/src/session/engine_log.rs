//! Keeping what the engine said, so a failed start has an explanation
//! somewhere.
//!
//! # The hole this fills
//!
//! exarch's TUI has always captured the engine's output: `TerminalGuard::enter`
//! redirects file descriptor 2 into `stderr.log` in the run directory, and a
//! session that dies leaves its reason there.  synod never did.  A synod run
//! directory held `run.lock` and `sessions/<id>/record.jsonl` and nothing
//! else, so a conversation whose engine died a second after it started left
//! behind a record with one line in it — the session's own opening bookend —
//! and no account at all of what happened next.  The failure the user saw was
//! assembled entirely out of the *host's* observation that a socket had
//! closed, which is the one fact that explains nothing.
//!
//! # What is actually reachable, and what is not
//!
//! Under synod the engine is not a child of this process: it is a child of
//! `ral-daemon`, inside a virtual machine, and the only descriptor this
//! process holds onto it is the protocol socket itself.  There is no stderr
//! to redirect here, and nothing can be added to the wire — a dying engine is
//! not in a position to describe itself over a protocol, which is why it died
//! silently in the first place.
//!
//! What *is* reachable is the guest's console.  `ral-daemon` gives the engine
//! its own standard descriptors, which the kernel has connected to
//! `/dev/console`, so a panic or a refusal from the engine goes to the console
//! beside the daemon's own lines.  The machine layer has pumped that console
//! since before the guest started, keeping it in a capped file and a short
//! ring ([`vm_manager::GuestConsole`]) — until now used only to explain a boot
//! that never dialled.  This module asks the same question at the other end of
//! a machine's life and writes the answer into the run directory, where a
//! person looking for it will actually be.
//!
//! On the macOS backend the console is attached to this process's standard
//! output and no copy is kept, so there is genuinely nothing to fetch; the
//! capture says so in the file rather than leaving an empty one that reads as
//! a silent guest.  That is the deliberate rule here: **never write a file
//! that is silent about its own emptiness.**

#![allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: writes the run directory's engine.log while a conversation is \
              failing to start — host-side diagnostic plumbing beside a session that has no \
              shell to route through and no run to raise a card in. See the module docs."
)]

use std::io::Write;
use std::path::{Path, PathBuf};

/// What the capture is called inside the run directory.
///
/// Named for what it holds rather than for the descriptor it came from: it is
/// not this process's `stderr.log`, and calling it that would promise a
/// developer an fd-2 redirect that does not exist here.
const FILE: &str = "engine.log";

/// Write everything this host can still say about an engine that has gone,
/// into `run_dir`, and give back the file's path if it was written.
///
/// `why` is the sentence the user is about to be shown — repeated here on
/// purpose, because the file has to stand on its own: whoever opens it a week
/// later has the window's text nowhere in front of them.
///
/// Returns `None` when the file could not be written at all.  A caller has
/// nothing better to do with that than carry on reporting the failure it was
/// already reporting, which is why this hands back an `Option` rather than an
/// error to be wrapped in another error.
pub(super) fn capture(
    run_dir: &Path,
    why: &str,
    console: &vm_manager::GuestConsole,
) -> Option<PathBuf> {
    let path = run_dir.join(FILE);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    file.write_all(render(why, console).as_bytes()).ok()?;
    file.flush().ok()?;
    Some(path)
}

/// The capture's text, split out so it can be read by a test without a
/// directory to write into.
fn render(why: &str, console: &vm_manager::GuestConsole) -> String {
    let mut out = String::new();
    out.push_str("---- the engine behind this conversation went ----\n");
    out.push_str(why);
    out.push_str("\n\n");
    match console.log.as_ref() {
        Some(path) => {
            out.push_str(&format!(
                "The guest's whole console is at {}. It belongs to whichever process ran the \
                 machine, which under an installed synod is a service running as LocalSystem, so \
                 reading it may take an administrator.\n\n",
                path.display()
            ));
        }
        None => out.push_str(
            "No console log was kept for this machine. On this backend the guest's console goes \
             to the standard output of whatever process owns the machine and no copy is made, so \
             there is no file to name — the silence below is synod's, not the guest's.\n\n",
        ),
    }
    if console.tail.is_empty() {
        out.push_str(
            "The guest's console offered no last words. Either it never got as far as a first \
             message, or this host could not reach the console at all; the line above says \
             which.\n",
        );
    } else {
        out.push_str("The guest's last words on its console, oldest first:\n");
        for line in &console.tail {
            out.push_str("  ");
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule the module docs set: an empty capture must say why it is
    /// empty, because a reader who cannot tell "the guest said nothing" from
    /// "synod could not hear it" will diagnose the wrong machine.
    #[test]
    fn an_empty_console_says_whose_silence_it_is() {
        let nothing = render(
            "The assistant could not be started.",
            &vm_manager::GuestConsole::default(),
        );
        assert!(
            nothing.contains("no copy is made"),
            "a backend that keeps no log must say so: {nothing}"
        );
        assert!(
            nothing.contains("no last words"),
            "an empty ring must be named as empty: {nothing}"
        );

        let kept = render(
            "The assistant could not be started.",
            &vm_manager::GuestConsole {
                log: Some(PathBuf::from("C:/cache/synod-console-1.log")),
                tail: Vec::new(),
            },
        );
        assert!(
            kept.contains("synod-console-1.log"),
            "a log that exists is named so a reader can open it: {kept}"
        );
        assert!(
            !kept.contains("no copy is made"),
            "the two silences must not be described in the same words: {kept}"
        );
    }

    /// The whole point of the file: the guest's own account of itself, which
    /// is the one thing the wire could never carry.
    #[test]
    fn the_guests_words_and_the_users_sentence_are_both_in_the_file() {
        let text = render(
            "The assistant could not be started — try again. (engine-closed; details in /run)",
            &vm_manager::GuestConsole {
                log: None,
                tail: vec![
                    "ral-daemon: mounting /work".to_string(),
                    "engine: panicked at 'no workspace'".to_string(),
                ],
            },
        );
        assert!(
            text.contains("engine-closed"),
            "the file stands alone, so it repeats the sentence the user saw: {text}"
        );
        assert!(
            text.contains("panicked at 'no workspace'"),
            "the guest's own words are the reason this file exists: {text}"
        );
        assert!(
            text.lines().any(|line| line.contains("mounting /work")),
            "every retained line is kept, oldest first: {text}"
        );
    }
}
