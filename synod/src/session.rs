//! The run itself: one folder, one job, one agent.
//!
//! Synod's session is exarch's session with the developer removed.  The
//! provider transport, the turn driver, and the card bus are exarch's
//! ([`exarch::provider`], [`exarch::agent`], [`exarch::headless`]); what
//! differs is where the work happens (the machine's workspace, not the
//! shell's cwd), what the agent may touch (the grant, not a capability
//! base named on the command line), and what the user is told.

use crate::cli::Cli;
use crate::grant::Grant;
use crate::workspace;
use exarch::agent::Agent;
use exarch::bootstrap::{self, Scratch};
use exarch::provider::{self, Engine, Provider, ProviderId, credential::CredentialStore, models};
use exarch::tui::SessionInfo;
use std::sync::Arc;
use vm_manager::Machine;

/// Synod's own directories.
///
/// `$XDG_STATE_HOME/synod/<folder>/` for the run logs,
/// `synod-scratch-<pid>` for the working area.  A synod run must never
/// write into an exarch run's logs, nor read its model selection.
pub const SYNOD: bootstrap::App = bootstrap::App::new("synod");

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

/// Run one job to completion in `machine`'s workspace under `grant`.
///
/// # Errors
/// Returns `Err` if no job was given, if the workspace cannot be entered,
/// if no model account is set up on this computer, if the provider or
/// model cannot be resolved, if the scratch or log directories cannot be
/// made, if the system prompt cannot be assembled, or if the run itself
/// fails.
///
/// # Panics
/// Panics if the chosen provider is absent from the credential store — an
/// invariant [`choose`] upholds by choosing only among available ones.
pub fn start(grant: &Grant, machine: &mut dyn Machine, cli: &Cli) -> Result<(), String> {
    let job =
        exarch::cli::load_seed(None, cli.job_file.clone(), cli.job.clone())?.ok_or_else(|| {
            "please say what you would like done — for example: \
             synod <folder> \"file every invoice under the month it was sent\""
                .to_string()
        })?;

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
    let mut catalog = models::ModelCatalog::new(models::LiveSource::new(&store));
    let (id, model) = choose(
        cli.provider.as_deref(),
        cli.model.as_deref(),
        &available,
        &mut catalog,
    )?;
    let cred = store
        .get(&id)
        .expect("the chosen provider is one of the available ones")
        .clone();

    let scratch =
        Arc::new(Scratch::new(SYNOD).map_err(|e| format!("could not make a working area: {e}"))?);
    let run_dir = SYNOD
        .log_run_dir(&cwd)
        .map_err(|e| format!("could not make a log folder: {e}"))?;
    let config_dir = SYNOD.xdg_dir(ral_core::path::basedir::XdgKind::Config);

    let caps = grant.capabilities(scratch.path());
    let system = crate::prompt::assemble(&caps, &scratch, grant.root(), &config_dir)?;
    let system_size = system.len();

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
            // Synod's agent may not schedule its own wakeups: an office job
            // is asked for and answered, never left running on its own
            // authority.
            allow_schedule: false,
            // A job is a job: the agent works until it has something to
            // report, then returns.  There is no conversation to park in.
            interactive: false,
            chat: false,
            disk_warn_bytes,
            // An office job is asked and answered, never a fleet: no
            // sub-agent may ever start from it.
            fuel: 0,
        },
        root_seat,
        Arc::clone(&provider),
    )
    .map_err(|e| format!("could not start the assistant: {e}"))?;

    // Checkpoint the folder before any work: the baseline the after-run
    // report is judged against, and the state every undo returns to.
    // Taken this late so a setup failure above costs no folder walk.
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

    // The one-shot frontend is exarch's: it is the only *public* way to
    // drive an agent to quiescence today (`Agent::seed`, `inbox`, and
    // `transcript` are crate-private), and synod's real surface is the
    // review UI of the design record, not a second terminal renderer.
    // `SessionInfo` is exarch-flavoured; only `base` is read here.
    let info = SessionInfo {
        system_size,
        system_files: &[],
        base: "granted-folder",
        extend_base: None,
        restrict_files: &[],
        scratch: scratch.path(),
        cwd: &cwd,
    };
    let outcome = exarch::headless::run(
        &mut session,
        &info,
        &provider,
        Some(job),
        exarch::headless::OutputFormat::Text,
        engine,
    );

    // Checkpoint what the run left behind and report the difference in
    // plain language.  Taken even after a failed run — whatever changed
    // before the failure is still undoable.  If the run itself erred,
    // its error outranks a capture error here.
    match store.capture(grant.root(), workspace::Moment::After) {
        Ok(after) => {
            let report = workspace::JobReport {
                folder: grant.root().to_string_lossy().into_owned(),
                finished_at_ms: Some(after.taken_at_ms),
                changes: workspace::ChangeSet::between(&baseline.manifest, &after.manifest),
            };
            eprintln!();
            eprint!("{}", workspace::report::render(&report));
        }
        Err(e) => {
            if outcome.is_ok() {
                return Err(e);
            }
        }
    }
    outcome
}

/// Choose the provider and model for this run: an explicit `--provider`,
/// else whichever available provider offers `--model`, else the one
/// account set up on this computer.
///
/// Synod has no model picker and remembers no choice, so a provider that
/// advertises no default model is a question to the user rather than a
/// blank to be filled in later.
fn choose(
    pinned: Option<&str>,
    wanted: Option<&str>,
    available: &[ProviderId],
    catalog: &mut models::ModelCatalog<models::LiveSource>,
) -> Result<(ProviderId, String), String> {
    let id = match (pinned, wanted) {
        (Some(name), _) => models::resolve_pinned_provider(name, available)?,
        (None, Some(name)) => models::resolve_model_provider(name, available, catalog)?,
        (None, None) => available[0].clone(),
    };
    let model = match wanted {
        Some(name) => name.to_string(),
        None => match id.famous() {
            Some(kind) => kind.info().1.to_string(),
            None => {
                return Err(format!(
                    "the account '{}' does not say which model to use — pass --model NAME",
                    id.label()
                ));
            }
        },
    };
    Ok((id, model))
}

#[cfg(test)]
mod tests {
    use super::NO_BOUNDARY;

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
