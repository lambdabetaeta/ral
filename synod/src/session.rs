//! The conversation itself: one folder, one agent, and the exchanges the
//! window drives between them.
//!
//! Synod's session is exarch's session with the developer removed.  The
//! provider transport, the turn driver, and the card bus are exarch's
//! ([`exarch::provider`], [`exarch::agent`], [`exarch::headless`]); what
//! differs is where the work happens (the machine's workspace, not the
//! shell's cwd), what the agent may touch (the grant, not a capability
//! base named on the command line), and what the user is told.
//!
//! There is no wire protocol here any more: synod is a library the window
//! calls in-process.  [`prepare`] resolves the credential store once at
//! startup; [`menu`] turns it into a picker the window can render;
//! [`Conversation::begin`] opens a folder onto a booted machine and an
//! agent, [`Conversation::exchange`] drives one message through it, and
//! [`Conversation::end`] shuts the machine down.  Provider and model are
//! either named by the window (a menu choice) or resolved the old way —
//! whichever one account is set up on this computer, and its default
//! model.

use crate::grant::Grant;
use crate::workspace;
use exarch::agent::Agent;
use exarch::bootstrap::{self, Scratch};
use exarch::provider::{
    self, Engine, Provider, ProviderId, credential::CredentialStore,
    models::resolve_pinned_provider,
};
use std::io;
use std::path::Path;
use std::sync::Arc;
use vm_manager::Machine;

/// Synod's own directories.
///
/// `$XDG_STATE_HOME/synod/<folder>/` for the run logs,
/// `synod-scratch-<pid>` for the working area.  A synod run must never
/// write into an exarch run's logs, nor read its model selection.
pub const SYNOD: bootstrap::App = bootstrap::App::new("synod");

/// "Protection: {this}." — true of every machine synod boots, since the
/// only backend left is a real virtual machine.  Carried here, verbatim,
/// from what was vm-manager's `Boundary::Hardware` sentence, before that
/// type lost every other inhabitant and stopped being worth keeping.
const HARDWARE_PROTECTION: &str = "a separate virtual machine: the agent can reach the folder \
                                    you granted and nothing else on this computer";

/// Resolve the credential store, once, at startup.
///
/// # Errors
/// Returns `Err` if the user's config cannot be read.
///
/// # Panics
/// This function must be called while the process is still
/// single-threaded: the credential scrub mutates the environment, and
/// that is only safe before any other thread — the transport runtime, a
/// session's worker threads — has been created.
pub fn prepare() -> Result<CredentialStore, String> {
    let custom = exarch::config::load()?;
    Ok(CredentialStore::resolve_and_scrub(custom))
}

/// One provider the window can offer, and the models known for it.
#[derive(serde::Serialize, Clone)]
pub struct ProviderChoice {
    /// A stable identifier that round-trips through
    /// [`Conversation::begin`]'s `choice`.
    pub key: String,
    /// [`ProviderId::label`].
    pub label: String,
    pub default_model: Option<String>,
    /// Whatever the static catalog honestly knows for this provider — at
    /// minimum the default, never a network fetch.
    pub models: Vec<String>,
}

/// The provider picker: one entry per available account.
#[derive(serde::Serialize, Clone)]
pub struct ModelMenu {
    pub providers: Vec<ProviderChoice>,
}

/// List the providers `store` has credentials for.
pub fn menu(store: &CredentialStore) -> ModelMenu {
    let providers = store
        .available()
        .into_iter()
        .map(|id| {
            let default_model = id.famous().map(|kind| kind.info().1.to_string());
            ProviderChoice {
                key: id.label().to_string(),
                label: id.label().to_string(),
                models: default_model.iter().cloned().collect(),
                default_model,
            }
        })
        .collect();
    ModelMenu { providers }
}

/// What the window says before the first message: the folder, the
/// boundary, and who is answering.
pub struct Opening {
    /// The granted folder, display form.
    pub folder: String,
    /// "Protection: {boundary}." — always [`HARDWARE_PROTECTION`].
    pub boundary_line: String,
    /// "Assistant: {label} ({model})."
    pub assistant_line: String,
    /// The ~2GiB warning sentence, when the folder is that large.
    pub large_folder_line: Option<String>,
}

/// One folder, held open over a booted machine and an agent, from the
/// first message to the last.
pub struct Conversation {
    grant: Grant,
    machine: Box<dyn Machine>,
    agent: Agent,
    engine: Arc<Engine>,
    history: workspace::HistoryStore,
    /// The working area, held for Drop: no seat holds a clone of it, and it
    /// must outlive the conversation it was assembled for.
    _scratch: Arc<Scratch>,
}

impl Conversation {
    /// Open `folder`, boot the best machine this computer can hold it in,
    /// and start the agent over it.
    ///
    /// `choice` names a provider and model from [`menu`]'s listing; `None`
    /// reproduces the old behaviour — whichever one account is set up on
    /// this computer, and its default model.
    ///
    /// # Errors
    /// Returns `Err` if this computer cannot start a virtual machine at all
    /// — the wrong platform, missing boot media, or an unsigned build — if
    /// the folder cannot be granted, if no model account is set up (or a
    /// named one has vanished), if the scratch or log directories cannot be
    /// made, if the system prompt cannot be assembled, or if the agent
    /// cannot be started.
    ///
    /// # Panics
    /// Panics if the chosen provider is absent from `store` — an invariant
    /// [`choose`] upholds by choosing only among available ones, and
    /// [`resolve_pinned_provider`] upholds by resolving only against the
    /// same list.
    pub fn begin(
        folder: &Path,
        store: &CredentialStore,
        choice: Option<(String, String)>,
    ) -> Result<(Self, Opening), String> {
        let grant = Grant::open(folder)?;
        let hypervisor = vm_manager::detect(crate::boot_media())?;
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut machine = hypervisor.boot(&grant.machine_spec()).map_err(|e| {
            format!(
                "could not start a machine for {}: {e}",
                grant.root().display()
            )
        })?;

        // Said before any work happens, because it is about what is
        // *about* to happen to the user's own documents.
        let boundary_line = format!("Protection: {HARDWARE_PROTECTION}.");

        // The agent works where the machine put the folder: the guest's
        // `/work`.
        let workspace = machine.workspace_path().to_path_buf();
        let cwd = workspace.to_string_lossy().into_owned();

        let disk_warn_bytes = exarch::config::disk_warn_bytes()?;
        // The IT-set fetch-url policy, audit ledger, and rate budget — one
        // file regardless of which front-end is running, opened once here.
        let egress = exarch::fleet::egress::Egress::open(SYNOD)?;

        let available = store.available();
        if available.is_empty() {
            return Err(
                "no model account is set up on this computer — ask your IT department \
                        to set a provider API key (ANTHROPIC_API_KEY, OPENAI_API_KEY, …)"
                    .into(),
            );
        }
        let (id, model) = match choice {
            Some((key, model)) => (resolve_pinned_provider(&key, &available)?, model),
            None => choose(&available)?,
        };
        let cred = store
            .get(&id)
            .expect("the chosen provider is one of the available ones")
            .clone();
        let assistant_line = format!("Assistant: {} ({model}).", id.label());

        let scratch = Arc::new(
            Scratch::new(SYNOD).map_err(|e| format!("could not make a working area: {e}"))?,
        );
        let run_dir = SYNOD
            .log_run_dir(&cwd)
            .map_err(|e| format!("could not make a log folder: {e}"))?;
        let config_dir = SYNOD.xdg_dir(ral_core::path::basedir::XdgKind::Config);

        let caps = grant.capabilities(scratch.path());
        let system = crate::prompt::assemble(&caps, &scratch, grant.root(), &config_dir)?;

        // The agent's engine dials in from inside the guest, so the
        // workspace is a guest path — never a directory this host process
        // could `chdir` into; the trunk drives it over the wire the
        // machine hands back.
        #[cfg(unix)]
        let root_seat = {
            let fd = machine.take_control();
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
        };
        // `WireTransport` is unix-only, and so is every seat that can drive
        // one: there is no non-unix way to reach the machine's control
        // plane, so a build for this platform can only refuse honestly.
        #[cfg(not(unix))]
        let root_seat: exarch::agent::RootSeat = {
            return Err("synod reaches its virtual machine over a socket this operating \
                         system does not provide — synod cannot run here"
                .to_string());
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
        let agent = Agent::root(
            exarch::agent::RootConfig {
                system,
                caps,
                run_dir,
                model,
                provider_label: id.label().to_string(),
                // Synod's agent may not schedule its own wakeups: a
                // conversing office assistant still runs on nothing but
                // the messages it is handed, never on its own authority.
                allow_schedule: false,
                // A conversation, not a job: the agent converses,
                // withholding `reply` and parking between messages rather
                // than returning once — [`exarch::headless::converse_on`]
                // drives one exchange at a time over this same session.
                interactive: true,
                chat: false,
                disk_warn_bytes,
                // A conversation is asked and answered, one message at a
                // time, never a fleet: no sub-agent may ever start from it.
                fuel: 0,
                egress,
            },
            root_seat,
            Arc::clone(&provider),
        )
        .map_err(|e| format!("could not start the assistant: {e}"))?;

        // Checkpoint the folder before any work: the baseline every
        // exchange's report is judged against, and the state every undo
        // returns to.  Taken once, for the whole conversation — this late
        // so a setup failure above costs no folder walk.
        let folder_measure = workspace::measure(grant.root())?;
        let large_folder_line = (folder_measure.bytes > workspace::LARGE_FOLDER_BYTES).then(|| {
            let tenths = folder_measure.bytes / 100_000_000;
            format!(
                "This folder holds about {}.{} GB across {} files.  Synod keeps a \
                 copy of everything before starting, which can take a while — \
                 possibly minutes on a shared drive.",
                tenths / 10,
                tenths % 10,
                folder_measure.files
            )
        });
        let history = workspace::HistoryStore::open_for(grant.root())?;
        history.capture(grant.root(), workspace::Moment::Before)?;

        let opening = Opening {
            folder: grant.root().to_string_lossy().into_owned(),
            boundary_line,
            assistant_line,
            large_folder_line,
        };

        Ok((
            Self {
                grant,
                machine,
                agent,
                engine,
                history,
                _scratch: scratch,
            },
            opening,
        ))
    }

    /// Drive one message through the conversation, streaming the same
    /// events [`exarch::headless::converse_on`] always does to `out` and
    /// `err`.
    ///
    /// Checkpoints what this exchange left behind, cumulatively from the
    /// baseline — taken even after a failed exchange, since whatever
    /// changed before the failure is still undoable.  Renders no report:
    /// the window reads one back through [`workspace::job_report`].
    ///
    /// # Errors
    /// Returns `Err` if the exchange itself fails; if the exchange
    /// succeeded but the after-checkpoint could not be taken, that error
    /// is returned instead.
    pub fn exchange(
        &mut self,
        message: String,
        out: &mut (dyn io::Write + Send),
        err: &mut (dyn io::Write + Send),
    ) -> Result<(), String> {
        let outcome =
            exarch::headless::converse_on(&mut self.agent, message, self.engine.clone(), out, err);
        match self
            .history
            .capture(self.grant.root(), workspace::Moment::After)
        {
            Ok(_) => {}
            Err(e) => {
                if outcome.is_ok() {
                    return Err(e);
                }
            }
        }
        outcome
    }

    /// Shut the machine down, ending the conversation.
    ///
    /// Drops the agent first: under a real VM its seat owns the wire, and
    /// closing that end is what makes the guest's engine see EOF and power
    /// the machine off from the inside — the same inside-out shutdown
    /// `boot-turn`'s own drop-then-stop performs, so the grace window
    /// `machine.shutdown` waits on below normally observes a stop already
    /// under way rather than forcing one.
    ///
    /// # Errors
    /// Returns `Err` if the machine does not stop cleanly — a failure
    /// this never swallows.
    pub fn end(self) -> Result<(), String> {
        let Self { machine, agent, .. } = self;
        drop(agent);
        machine
            .shutdown()
            .map_err(|e| format!("the machine did not stop cleanly: {e}"))
    }
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
    model.map(|model| (id.clone(), model)).ok_or_else(|| {
        format!(
            "the account set up on this computer ('{}') does not say which model to \
             use — ask your IT department to set one up.",
            id.label()
        )
    })
}
