//! The conversation itself: one folder, one agent, and a message loop that
//! runs until the window ends it.
//!
//! Synod's session is exarch's session with the developer removed.  The
//! provider transport, the turn driver, and the card bus are exarch's
//! ([`exarch::provider`], [`exarch::agent`], [`exarch::headless`]); what
//! differs is where the work happens (the machine's workspace, not the
//! shell's cwd), what the agent may touch (the grant, not a capability
//! base named on the command line), and what the user is told.
//!
//! This binary is never typed at.  Its whole surface is the window's own
//! spawn contract: the folder on the command line once, at spawn
//! ([`crate::cli`]); every message after that framed down stdin
//! ([`read_message`]); the same events [`exarch::headless::run`] always
//! streamed, on stdout and stderr, once per exchange; stdin's end ending
//! the process.  Provider and model are never a flag — they resolve from
//! whichever one account is set up on this computer, and its default
//! model.

use crate::grant::Grant;
use crate::workspace;
use exarch::agent::Agent;
use exarch::bootstrap::{self, Scratch};
use exarch::provider::{self, Engine, Provider, ProviderId, credential::CredentialStore};
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use vm_manager::Machine;

/// Synod's own directories.
///
/// `$XDG_STATE_HOME/synod/<folder>/` for the run logs,
/// `synod-scratch-<pid>` for the working area.  A synod run must never
/// write into an exarch run's logs, nor read its model selection.
pub const SYNOD: bootstrap::App = bootstrap::App::new("synod");

/// The exchange-settled signal.
///
/// synod writes this line to stderr the moment an exchange has settled and
/// its report is ready to read — bracketed in a NUL byte on each side, one
/// no live narration or tool trace is ever built out of, so the window's
/// line reader can tell this line apart from everything else on the same
/// stream without guessing. It carries no report of its own: the window
/// already has a structured reader for that ([`crate::workspace::job_report`]),
/// so this is purely the "go read it again" signal, not a second copy of
/// the report in a second format. Part of the same internal spawn contract
/// [`read_message`] documents the other half of.
pub const EXCHANGE_DONE: &str = "\0synod:exchange-done\0";

/// What the boundary *means for the user*, said after
/// [`vm_manager::Boundary`]'s own sentence when there is no wall: their
/// real documents change as the agent works, everything can be put back
/// afterwards, and a document left open in Word can be overwritten under
/// them — said plainly, because that risk is accepted, not hidden.
///
/// Deliberately jargon-free.  A person who cannot evaluate the words
/// "virtual machine" is exactly the person this sentence is for.
const NO_BOUNDARY: &str = "\
The assistant changes the files in your folder directly as it works.
Synod keeps a copy of everything first, and when the job is done you
will see a list of what changed — everything can be put back, one file
at a time or all at once.  Please close any documents from this folder
before starting: a document still open in Word or Excel can be
overwritten while you have it on screen.";

/// Read one framed message from `stdin`.
///
/// Messages are multi-line free text, so the frame is a plain one: every
/// line up to a line holding exactly `.` (a lone dot, the classic
/// SMTP-`DATA` terminator) belongs to the message, and the dot itself does
/// not — no ordinary sentence is a line on its own, so the window writes
/// the frame trivially: the message's lines, then a line that is just `.`.
///
/// Returns `Ok(None)` only when the stream ends with nothing read at all —
/// the window closed stdin between messages, which ends the session. A
/// message the stream happens to end inside of, with no closing dot, is
/// still returned in full: the window only ever closes stdin between
/// messages, never mid-one, so a bare end-of-stream there is not a framing
/// failure to reject.
///
/// # Errors
/// Returns `Err` if reading a line from `stdin` fails.
pub(crate) fn read_message(stdin: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut body = String::new();
    let mut any = false;
    loop {
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        any = true;
        let line = line.strip_suffix('\n').unwrap_or(&line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == "." {
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
    }
    Ok(any.then_some(body))
}

/// Hold the conversation over `machine`'s workspace under `grant` until
/// stdin closes.
///
/// # Errors
/// Returns `Err` if the workspace cannot be entered, if no model account is
/// set up on this computer, if the account's default model cannot be
/// resolved, if the scratch or log directories cannot be made, if the
/// system prompt cannot be assembled, if a message cannot be read back from
/// stdin, or if an exchange itself fails.
///
/// # Panics
/// Panics if the chosen provider is absent from the credential store — an
/// invariant [`choose`] upholds by choosing only among available ones.
pub fn start(grant: &Grant, machine: &mut dyn Machine) -> Result<(), String> {
    // Said before any work happens, because it is about what is *about* to
    // happen to the user's own documents.  The boundary describes itself in
    // its own words; synod adds only what it means for the person reading.
    let boundary = machine.boundary();
    eprintln!("Working in {}", grant.root().display());
    eprintln!("Protection: {boundary}.");
    if !boundary.is_hardware() {
        eprintln!("{NO_BOUNDARY}");
    }
    eprintln!();

    // The agent works where the machine put the folder: the granted folder
    // itself under a host machine, the guest's `/work` under a real one.
    let workspace = machine.workspace_path().to_path_buf();
    let cwd = workspace.to_string_lossy().into_owned();

    // The provider config and credentials are exarch's, read from the
    // user's own config home — the model is called from the host, never
    // from inside the machine, so no key ever crosses the boundary.
    let custom = exarch::config::load()?;
    let disk_warn_bytes = exarch::config::disk_warn_bytes()?;
    // The IT-set fetch-url policy, audit ledger, and rate budget — one file
    // regardless of which front-end is running, opened once here.
    let egress = exarch::fleet::egress::Egress::open(SYNOD)?;
    // SAFETY: startup is still single-threaded here — the transport runtime
    // and the session's worker threads are created below — so no other
    // thread can race this env mutation.  This is the only credential scrub;
    // every spawned child therefore inherits an environment free of keys.
    let store = CredentialStore::resolve_and_scrub(custom);
    let available = store.available();
    if available.is_empty() {
        return Err(
            "no model account is set up on this computer — ask your IT department \
                    to set a provider API key (ANTHROPIC_API_KEY, OPENAI_API_KEY, …)"
                .into(),
        );
    }
    let (id, model) = choose(&available)?;
    let cred = store
        .get(&id)
        .expect("the chosen provider is one of the available ones")
        .clone();
    eprintln!("Assistant: {} ({model}).", id.label());

    let scratch =
        Arc::new(Scratch::new(SYNOD).map_err(|e| format!("could not make a working area: {e}"))?);
    let run_dir = SYNOD
        .log_run_dir(&cwd)
        .map_err(|e| format!("could not make a log folder: {e}"))?;
    let config_dir = SYNOD.xdg_dir(ral_core::path::basedir::XdgKind::Config);

    let caps = grant.capabilities(scratch.path());
    let system = crate::prompt::assemble(&caps, &scratch, grant.root(), &config_dir)?;

    // A machine that hands back a control-plane stream is a real VM: the
    // agent's engine dials in from inside the guest, so the workspace is a
    // guest path — never a directory this host process could `chdir` into.
    // A machine with nothing to hand back (today's only backend) runs the
    // engine right here, in the folder itself. Decided here, right before
    // the transport runtime starts, while startup is still single-threaded
    // — the same window the `chdir` this replaces always needed.
    #[cfg(unix)]
    let root_seat = if let Some(fd) = machine.take_control() {
        exarch::agent::RootSeat::Wire {
            transport: Box::new(
                ral_core::transport::WireTransport::adopt(
                    std::os::unix::net::UnixStream::from(fd),
                    ral_core::transport::Liveness::default(),
                )
                .map_err(|e| format!("could not take control of the machine: {e}"))?,
            ),
            cwd: workspace.clone(),
            home: workspace,
        }
    } else {
        std::env::set_current_dir(&workspace)
            .map_err(|e| format!("could not open {} to work in: {e}", workspace.display()))?;
        exarch::agent::RootSeat::Identity {
            scratch: Arc::clone(&scratch),
        }
    };
    #[cfg(not(unix))]
    let root_seat = {
        std::env::set_current_dir(&workspace)
            .map_err(|e| format!("could not open {} to work in: {e}", workspace.display()))?;
        exarch::agent::RootSeat::Identity {
            scratch: Arc::clone(&scratch),
        }
    };

    let engine = Engine::new();
    let provider = Arc::new(Provider::build(
        engine.clone(),
        &id,
        model.clone(),
        &cred,
        None,
        provider::Tuning::initial(),
        None,
    ));
    let mut session = Agent::root(
        exarch::agent::RootConfig {
            system,
            caps,
            run_dir,
            model,
            provider_label: id.label().to_string(),
            // Synod's agent may not schedule its own wakeups: a conversing
            // office assistant still runs on nothing but the messages it
            // is handed, never on its own authority.
            allow_schedule: false,
            // A conversation, not a job: the agent converses, withholding
            // `reply` and parking between messages rather than returning
            // once — [`exarch::headless::converse`] drives one exchange at
            // a time over this same session.
            interactive: true,
            chat: false,
            disk_warn_bytes,
            // A conversation is asked and answered, one message at a time,
            // never a fleet: no sub-agent may ever start from it.
            fuel: 0,
            egress,
        },
        root_seat,
        Arc::clone(&provider),
    )
    .map_err(|e| format!("could not start the assistant: {e}"))?;

    // Checkpoint the folder before any work: the baseline every exchange's
    // report is judged against, and the state every undo returns to. Taken
    // once, for the whole conversation — this late so a setup failure above
    // costs no folder walk.
    let folder = workspace::measure(grant.root())?;
    if folder.bytes > workspace::LARGE_FOLDER_BYTES {
        let tenths = folder.bytes / 100_000_000;
        eprintln!(
            "This folder holds about {}.{} GB across {} files.  Synod keeps a \
             copy of everything before starting, which can take a while — \
             possibly minutes on a shared drive.",
            tenths / 10,
            tenths % 10,
            folder.files
        );
    }
    let store = workspace::HistoryStore::open_for(grant.root())?;
    let baseline = store.capture(grant.root(), workspace::Moment::Before)?;

    let mut stdin = io::stdin().lock();
    while let Some(message) =
        read_message(&mut stdin).map_err(|e| format!("could not read the next message: {e}"))?
    {
        let outcome = exarch::headless::converse(&mut session, message, engine.clone());
        // Checkpoint what this exchange left behind and report the
        // cumulative difference from the baseline, in plain language.
        // Taken even after a failed exchange — whatever changed before the
        // failure is still undoable.  If the exchange itself erred, its
        // error outranks a capture error here.
        match store.capture(grant.root(), workspace::Moment::After) {
            Ok(after) => {
                let report = workspace::JobReport {
                    folder: grant.root().to_string_lossy().into_owned(),
                    finished_at_ms: Some(after.taken_at_ms),
                    changes: workspace::ChangeSet::between(&baseline.manifest, &after.manifest),
                };
                eprintln!();
                eprint!("{}", workspace::report::render(&report));
                eprintln!("{EXCHANGE_DONE}");
                let _ = io::stderr().flush();
            }
            Err(e) => {
                if outcome.is_ok() {
                    return Err(e);
                }
            }
        }
        outcome?;
    }
    Ok(())
}

/// The provider and model for this run: whichever one account is set up on
/// this computer, and its default model.
///
/// Synod has no model picker and remembers no choice, so an account that
/// names no default model is a question for the user, refused in the same
/// plain register as having no account at all — there is no flag left to
/// answer it with.
fn choose(available: &[ProviderId]) -> Result<(ProviderId, String), String> {
    let id = available[0].clone();
    let model = id.famous().map(|kind| kind.info().1.to_string());
    model
        .map(|model| (id.clone(), model))
        .ok_or_else(|| {
            format!(
                "the account set up on this computer ('{}') does not say which model to \
                 use — ask your IT department to set one up.",
                id.label()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A single dot ends the message; the dot itself is not part of it.
    #[test]
    fn a_lone_dot_ends_the_message() {
        let mut stdin = Cursor::new(b"do the filing\n.\n".as_slice());
        let message = read_message(&mut stdin).expect("reads").expect("a message arrived");
        assert_eq!(message, "do the filing");
    }

    /// A multi-line body joins with newlines, exactly as typed — the whole
    /// reason the frame is a terminator line rather than a blank line,
    /// which ordinary prose uses between paragraphs.
    #[test]
    fn a_multi_line_body_keeps_its_blank_lines() {
        let mut stdin = Cursor::new(b"Dear all,\n\nPlease find attached.\n.\n".as_slice());
        let message = read_message(&mut stdin).expect("reads").expect("a message arrived");
        assert_eq!(message, "Dear all,\n\nPlease find attached.");
    }

    /// The stream ending with nothing read at all is the session's own
    /// end, not a message — the window closed stdin between messages.
    #[test]
    fn eof_with_nothing_read_ends_the_session() {
        let mut stdin = Cursor::new(b"".as_slice());
        assert_eq!(read_message(&mut stdin).expect("reads"), None);
    }

    /// A stream that ends mid-message, with no closing dot, still yields
    /// the message read so far — the window only closes stdin between
    /// messages, never inside one.
    #[test]
    fn eof_mid_message_still_returns_what_was_read() {
        let mut stdin = Cursor::new(b"unfinished thought".as_slice());
        let message = read_message(&mut stdin).expect("reads").expect("a message arrived");
        assert_eq!(message, "unfinished thought");
    }

    /// Two messages read in sequence off one stream, then the session's
    /// own end — the shape the loop in [`start`] actually relies on.
    #[test]
    fn messages_read_in_sequence_then_the_session_ends() {
        let mut stdin = Cursor::new(b"first\n.\nsecond\n.\n".as_slice());
        assert_eq!(
            read_message(&mut stdin).expect("reads"),
            Some("first".to_string())
        );
        assert_eq!(
            read_message(&mut stdin).expect("reads"),
            Some("second".to_string())
        );
        assert_eq!(read_message(&mut stdin).expect("reads"), None);
    }

    /// The notice exists to be understood by someone who has never heard
    /// of a virtual machine.  If it ever acquires the vocabulary of the
    /// design record, it has stopped doing its job.
    #[test]
    fn the_no_boundary_notice_speaks_english() {
        for jargon in [
            "VM",
            "virtual machine",
            "hypervisor",
            "sandbox",
            "boundary",
            "guest",
            "host",
            "capability",
            "checkpoint",
            "manifest",
            "hash",
        ] {
            assert!(
                !NO_BOUNDARY.to_lowercase().contains(&jargon.to_lowercase()),
                "the warning must not say '{jargon}'",
            );
        }
    }

    /// What it must say instead: that their own files change directly,
    /// that everything can be put back, and that open documents must be
    /// closed first because they can be overwritten mid-edit.
    #[test]
    fn the_no_boundary_notice_says_what_is_at_stake() {
        for point in [
            "your folder",
            "put back",
            "close any documents",
            "overwritten",
        ] {
            assert!(
                NO_BOUNDARY.contains(point),
                "the warning must make the point '{point}'",
            );
        }
    }
}
