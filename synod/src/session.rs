//! The conversation itself: one folder, one agent, and the exchanges the
//! window drives between them.
//!
//! Synod's session is exarch's session with the developer removed.  The
//! provider transport, the exchange driver, and the card bus are exarch's
//! ([`exarch::provider`], [`exarch::agent`], [`exarch::headless`]); what
//! differs is where the work happens (the machine's workspace, not the
//! shell's cwd), what the agent may touch (the grant, not a capability
//! base named on the command line), and what the user is told.
//!
//! synod is a library the window calls in-process, with no wire protocol
//! between the two.  [`prepare`] resolves the credential store once at
//! startup; [`menu`] turns it into a picker the window can render;
//! [`Conversation::begin`] opens a folder onto a booted machine and an
//! agent, [`Conversation::exchange`] drives one message through it, and
//! [`Conversation::end`] shuts the machine down.  Provider and model are
//! either named by the window (a menu choice) or resolved the old way —
//! whichever one account is set up on this computer, and its default
//! model.
//!
//! The store the whole module reads is held behind a [`Mutex`] because
//! [`sign_in`] can add to it: a `ChatGPT` plan signed in from the window
//! becomes available to the very next [`menu`] and conversation, with no
//! restart.  Every function here that takes it locks it only for as long
//! as it takes to read the account list — never across a network fetch or
//! a machine boot.

mod baseline;
mod menu;
mod opening;
mod signin;

use crate::grant::Grant;
use crate::workspace;
use baseline::Baseline;
use exarch::agent::{Avatar, RecordedAccount};
use exarch::provider::{
    self, Bureau, Engine, Provider,
    credential::CredentialStore,
    identity::{self, Account},
    models::{LiveSource, ModelCatalog, resolve_account},
    pricing,
};
use opening::{no_copy_line, no_room_for_copy, slow_copy_line};
use ral_core::sync::LockExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub use menu::{ModelChoice, ModelMenu, ProviderChoice, menu, refresh_menu};
pub use opening::Opening;
pub use signin::{SignInStep, SignedIn, sign_in};

/// Synod's own directories.
///
/// `$XDG_STATE_HOME/synod/<folder>/` for the run logs.  A synod run must
/// never write into an exarch run's logs, nor read its model selection.
/// The agent's working area is not among these: it is the guest's own
/// scratch tmpfs ([`crate::grant::GUEST_SCRATCH`]), no host directory.
///
/// The name is the engine's ([`exarch::bootstrap::SYNOD`]) because exarch's
/// grant composition must deny these directories, and a second spelling here
/// would let the two drift apart into a hole.
pub use exarch::bootstrap::SYNOD;

/// Resolve the credential store, once, at startup — see
/// [`crate::accounts::prepare`], which is where synod's accounts actually
/// come from.
///
/// # Errors
/// Returns `Err` if synod's own provider settings cannot be read.
///
/// # Panics
/// This function must be called while the process is still
/// single-threaded: the credential scrub mutates the environment, and
/// that is only safe before any other thread — the transport runtime, a
/// session's worker threads — has been created.
pub fn prepare() -> Result<CredentialStore, String> {
    crate::accounts::prepare()
}

/// One provider, model, and (optionally) reasoning effort, chosen from
/// [`menu`] or [`refresh_menu`]'s listing and handed to
/// [`Conversation::begin`].
///
/// `effort`'s absence and `effort: Some("auto")` are deliberately distinct:
/// leaving it unset carries [`provider::Tuning::initial`]'s thinking-on
/// default forward untouched, exactly as an unspecified choice always has;
/// naming `"auto"` is a request to send no reasoning option on the wire at
/// all, landing on `effort: None` the same way, but *chosen* rather than
/// defaulted.
#[derive(serde::Deserialize)]
pub struct Choice {
    /// An [`AccountId`](exarch::provider::identity::AccountId) rendering, as
    /// handed out in a [`ProviderChoice::account`] — resolved back to an
    /// [`Account`] by [`resolve_account`], id only, never a name: two
    /// accounts can share a display label, and starting the wrong one's
    /// conversation is exactly the bug this type exists to prevent.
    pub account: String,
    pub model: String,
    pub effort: Option<String>,
}

/// The net wire's stream type, adopted from [`vm_manager::Wires::net`].
///
/// An `OwnedFd` wearing an `AF_VSOCK` connection on Unix, an `OwnedSocket`
/// wearing an `AF_HYPERV` one on Windows — [`guest_net::device::Wire`] is
/// implemented for both, the same pretence [`ral_core::wire::WireStream`]
/// makes for the control plane.
#[cfg(unix)]
pub type NetWire = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type NetWire = std::net::TcpStream;

/// One folder, held open over a booted machine and an agent, from the
/// first message to the last.
pub struct Conversation {
    grant: Grant,
    /// The machine, held in trust by the dialler: [`Conversation::end`]
    /// recovers it via [`crate::machine_dial::MachineDial::into_machine`],
    /// the only other owner being the [`Avatar`] this conversation also
    /// holds, which drops first.
    dial: Arc<crate::machine_dial::MachineDial>,
    agent: Avatar,
    baseline: Baseline,
    /// The guest's whole network, running on its own threads since before
    /// the first exchange — see [`net_seat`].
    net: guest_net::Session<NetWire>,
}

impl Conversation {
    /// Open `folder`, boot the best machine this computer can hold it in,
    /// and start the agent over it.
    ///
    /// `choice` names a provider, model, and effort from [`menu`]'s or
    /// [`refresh_menu`]'s listing; `None` takes what this computer offers —
    /// whichever one account is set up on it, that account's default model,
    /// and [`provider::Tuning::initial`]'s thinking-on effort. A chosen
    /// effort that [`provider::pricing::caps_or_default`] positively knows
    /// the model does not take is masked to `None` regardless of what was
    /// asked for — the model would otherwise refuse the request outright.
    ///
    /// # Errors
    /// Returns `Err` if this computer cannot start a virtual machine at all
    /// — the wrong platform, missing boot media, or an unsigned build — if
    /// the folder cannot be granted, if no model account is set up (or a
    /// named one has vanished), if the chosen effort names no rung on
    /// [`provider::EFFORT_LADDER`], if the scratch or log directories cannot
    /// be made, if the system prompt cannot be assembled, if the agent
    /// cannot be started, or if guest networking cannot start. Guest
    /// networking does not start degraded; a conversation with no network
    /// it can trust is refused outright rather than opened quietly without
    /// one. The before-checkpoint itself is not among these: its capture
    /// runs on past `begin`'s return, and [`Conversation::exchange`] reports
    /// its failure instead.
    ///
    /// # Panics
    /// Panics if the resolved tuning's effort is one
    /// [`provider::EFFORT_LADDER`] does not name, which [`resolve_tuning`]
    /// cannot produce.
    pub fn begin(
        folder: &Path,
        store: &Arc<Mutex<CredentialStore>>,
        catalog: &Arc<Mutex<ModelCatalog<LiveSource>>>,
        choice: Option<Choice>,
    ) -> Result<(Self, Opening), String> {
        let grant = Grant::open(folder)?;
        // Handed as the *means* of readying the media rather than the media
        // itself.  On a computer where synod was installed, none of it is needed:
        // the machine service boots from its own copy, inflated once into a cache
        // every session on the computer shares, and readying it here would put
        // two and a half gigabytes into this user's cache for nothing.  Which of
        // those is the case is `vm_manager::detect`'s to know, so this hands over
        // the closure and lets it decide.
        let boot = crate::boot::boot_media()
            .map(|plan| -> vm_manager::BootMedia { Box::new(move || plan.realise()) });
        let hypervisor = vm_manager::detect(boot)?;

        let disk_warn_bytes = exarch::config::disk_warn_bytes()?;
        // The IT-set network policy, audit ledger, and rate budget — one
        // file regardless of which front-end is running, opened once here.
        let egress = exarch::egress::Egress::open(SYNOD)?;

        let Selected {
            account,
            label,
            model,
            effort,
        } = select_account(store, choice)?;
        let announced_model = model.clone();
        let tuning = resolve_tuning(effort, &model)?;
        let announced_effort = provider::effort_label(&tuning.effort)
            .expect("resolve_tuning yields only efforts the ladder names")
            .to_string();

        let run_dir = SYNOD
            .log_run_dir(&grant.root().to_string_lossy())
            .map_err(|e| format!("could not make a log folder: {e}"))?;
        let config_dir = SYNOD.xdg_dir(ral_core::path::basedir::XdgKind::Config);

        // The store is fresh at every `begin` — it never outlives its own
        // conversation — so this folder's one full read is paid here every
        // time, and the lines below fire purely on what the folder holds.
        let measure = workspace::manifest::measure(grant.root())?;

        // A copy with nowhere to go is not attempted.  Filling the disk to
        // reach a safety net leaves the user worse off than opening without
        // one, and it is their own folder either way: the conversation runs,
        // the opening says there is no undo, and no store is even made.
        let (baseline, folder_line) = if let Some(free) = no_room_for_copy(measure, grant.root()) {
            (Baseline::Untaken, Some(no_copy_line(measure, free)))
        } else {
            let history = workspace::HistoryStore::open_for(grant.root())?;
            // The two slow arms of an opening wait on different things
            // entirely: the boot on a guest kernel coming up, the
            // before-checkpoint on every byte of the folder being read
            // and kept.  Neither needs the other, and nothing touches
            // the folder until the first exchange, so the capture starts
            // now, before the boot, and the conversation opens the moment
            // the boot is done — the capture goes on running, joined only
            // once an exchange or `end` needs the settled store. Every
            // error path below still joins it before reporting, so no
            // walk is ever left running past the conversation that
            // started it.
            let root = grant.root().to_path_buf();
            (Baseline::spawn(root, history), slow_copy_line(measure))
        };

        let machine = hypervisor.boot(&grant.machine_spec()).map_err(|e| {
            format!(
                "could not start a machine for {}: {e}",
                grant.root().display()
            )
        })?;

        // Everything the agent is told, and everything it is allowed, is in
        // the guest's namespace: the engine lives there, and the host path
        // of the folder names nothing inside it.
        let caps = grant.capabilities();
        let system =
            crate::prompt::assemble(&caps, machine.workspace_path(), &grant.name(), &config_dir)?;

        // Synod's engine is per-conversation; the credentials and catalog it
        // draws on are the application's, held for its whole life.
        let bureau = Arc::new(Bureau::Live {
            engine: Engine::new(),
            store: Arc::clone(store),
            catalog: Arc::clone(catalog),
        });
        let provider = bureau.build(&account, model.clone(), &tuning, None, None)?;

        let config = exarch::agent::RootConfig {
            system,
            caps: ral_core::types::GrantStack::of(caps),
            run_dir,
            resume: None,
            no_logs: false,
            run_lock: None,
            model,
            account: RecordedAccount {
                label: label.clone(),
                service: account.service.name.as_str().to_string(),
                id: account.id.as_str().to_string(),
            },
            // Synod's agent may not schedule its own wakeups: a
            // conversing office assistant still runs on nothing but
            // the messages it is handed, never on its own authority.
            allow_schedule: false,
            // A conversation, not a job: the agent converses,
            // withholding `reply` and parking between messages rather
            // than returning once — [`exarch::headless::converse_sink`]
            // drives one exchange at a time over this same session.
            interactive: true,
            chat: false,
            disk_warn_bytes,
            // Every agent may delegate: the office assistant hatches
            // helpers that run concurrently in the same guest, and
            // the exchange ends only once the whole tree does
            // (`converse_settled`'s Law B), not merely the trunk.
            fuel: exarch::agent::SPAWN_FUEL,
            egress: egress.clone(),
            // `seat_machine` overwrites this: the dialler cannot exist
            // before the machine it wraps.
            dial: None,
            bureau,
        };
        let (dial, agent, net_wire) = seat_machine(machine, config, provider)?;
        let net = net_seat(net_wire, egress)?;

        Ok((
            Self {
                grant,
                dial,
                agent,
                baseline,
                net,
            },
            Opening {
                label,
                model: announced_model,
                effort: announced_effort,
                folder_line,
            },
        ))
    }

    /// Drive one message through the conversation, streaming the bus's
    /// events into the caller's `sink` in order — the same events
    /// [`exarch::headless::converse_settled`] drives through, ending only
    /// once the whole fleet quiesces: the trunk parked, no live helpers
    /// left, their results drained.
    ///
    /// Settles the baseline first — joining its capture thread if this is
    /// the first exchange — before the guest touches anything, since the
    /// folder must never be read and written at once. Checkpoints what this
    /// exchange left behind, cumulatively from the baseline — taken even
    /// after a failed exchange, since whatever changed before the failure is
    /// still undoable. Taking it only after quiescence is what keeps a
    /// helper's late write from ever landing after the checkpoint and being
    /// blamed on the user. Renders no report: the window reads one back
    /// through [`workspace::job_report`].
    ///
    /// # Errors
    /// Returns `Err` if the baseline capture failed or its thread panicked;
    /// otherwise if the exchange itself fails; otherwise if the exchange
    /// succeeded but the after-checkpoint could not be taken, that error is
    /// returned instead.
    pub fn exchange<S: exarch::bus::Sink>(
        &mut self,
        message: String,
        sink: &mut S,
    ) -> Result<(), String> {
        let history = self.baseline.store()?;
        let outcome = exarch::headless::converse_settled(&mut self.agent, message, sink);
        // No baseline, no after-checkpoint: a checkpoint with nothing to be
        // judged against is a record no report can read.
        let after = history
            .map(|history| history.capture(self.grant.root(), workspace::Moment::After))
            .transpose();
        outcome.and_then(|()| after.map(drop))
    }

    /// Shut the machine down, ending the conversation — and, its store
    /// having no life left to serve, wipe it: closing the window is
    /// accepting the folder as it stands, so undo ends here too.
    ///
    /// Recovers the machine through [`unseat_machine`], which drops the
    /// agent before reclaiming it from the dialler — see its own doc for
    /// why that order is load-bearing. The net wire follows, never before
    /// the control wire — a session with its control plane gone but its
    /// network still live has nothing left to police what that network is
    /// used for. The wipe comes last, and runs whatever state the baseline
    /// settled into — a `Failed` baseline may still have left partial
    /// objects behind, and those are exactly what a wipe is for. A wipe
    /// cut short by the window's own close timeout is not a leak: the
    /// directory it left unlocked is exactly what
    /// [`workspace::history::sweep_stale`] collects at the next start.
    ///
    /// # Errors
    /// Returns `Err` if the machine does not stop cleanly, or if a
    /// guest-net worker panicked — failures this never swallows. A baseline
    /// still `Pending` (the user closed before ever sending a message) is
    /// stopped and joined, and its own outcome goes unreported: a copy on
    /// its way to being wiped has no error left worth raising, least of all
    /// the one this stop just caused. A wipe failure surfaces only once
    /// everything above it has succeeded.
    ///
    /// # Panics
    /// Panics if [`unseat_machine`] does — see its own doc.
    pub fn end(self) -> Result<(), String> {
        let Self {
            dial,
            agent,
            net,
            mut baseline,
            ..
        } = self;
        let machine = unseat_machine(agent, dial);
        // Joined, not merely stopped: ending a conversation must leave no
        // guest-net thread behind, and `join` is what reports a worker panic.
        net.stop();
        let net_end = net.join();
        let shutdown = machine
            .shutdown()
            .map_err(|e| format!("the machine did not stop cleanly: {e}"))
            .and(net_end);
        // Only after the machine is down: a conversation can end before its
        // first exchange, and joining a still-running walk must not hold a
        // dead-to-the-user window open ahead of the shutdown that ends it.
        // Halted first, so what is joined is a walk on its way out rather
        // than one still reading a folder whose copy is about to be wiped.
        baseline.halt();
        let wipe_err = baseline.into_store().and_then(|store| store.wipe().err());
        shutdown?;
        wipe_err.map_or(Ok(()), Err)
    }
}

/// The half of [`Conversation::begin`] that has nothing to do with which
/// account or folder is asking for it.
///
/// Adopts `machine`'s control wire, hands the machine itself to a fresh
/// [`crate::machine_dial::MachineDial`], and starts an [`Avatar`] over the
/// seated wire with `config` and `provider`.
///
/// `boot-run` calls this too, with a scripted `provider` and its own
/// `config` — the reason this exists at all is so that example has no
/// bracket of its own left to assemble by hand.
///
/// `config.dial` is overwritten with the fresh dialler regardless of what
/// was passed in: the dialler cannot exist before the machine it wraps is
/// known, so no caller can hand in the real one ahead of this call.
///
/// The net wire comes back unclaimed rather than seated here — taken once
/// alongside the control wire, since [`vm_manager::Machine::take_wires`]
/// panics on a second call, but what happens to a conversation's network is
/// this crate's business (see [`net_seat`]), not this constructor's: an
/// example with no guest network of its own has nowhere to put it.
///
/// # Errors
/// Returns `Err` if the control plane cannot be adopted as a wire, or if
/// the agent cannot be started.
pub fn seat_machine(
    mut machine: Box<dyn vm_manager::Machine>,
    mut config: exarch::agent::RootConfig,
    provider: Arc<Provider>,
) -> Result<(Arc<crate::machine_dial::MachineDial>, Avatar, NetWire), String> {
    let workspace = machine.workspace_path().to_path_buf();
    let wires = machine.take_wires();
    let root_seat = control_seat(wires.control, workspace)?;
    let dial = Arc::new(crate::machine_dial::MachineDial::new(machine));
    config.dial = Some(dial.clone());
    let agent = Avatar::root(config, root_seat, provider)
        .map_err(|e| format!("could not start the assistant: {e}"))?;
    Ok((dial, agent, wires.net.into()))
}

/// Recover `agent`'s machine from `dial`, ending the agent first.
///
/// [`Conversation::end`]'s own machine-recovery step, and the same one
/// `boot-run` now reaches through this function instead of reproducing.
///
/// Drops `agent` first: under a real VM its seat owns the wire, and closing
/// that end is what makes the guest's engine see EOF and power the machine
/// off from the inside, so the grace window a caller's `machine.shutdown()`
/// normally observes a stop already under way rather than forcing one. The
/// machine comes back from the dialler next, which has held it since
/// [`seat_machine`] — its only other owner was the agent just dropped,
/// whose own helpers (if any) are gone too by the time an exchange has
/// settled, so this is always the dialler's last reference.
///
/// # Panics
/// Panics if `dial`'s `Arc` has another owner left once `agent` is gone —
/// a construction bug, since nothing else in this crate clones it.
pub fn unseat_machine(
    agent: Avatar,
    dial: Arc<crate::machine_dial::MachineDial>,
) -> Box<dyn vm_manager::Machine> {
    drop(agent);
    Arc::try_unwrap(dial)
        .unwrap_or_else(|_| {
            panic!(
                "the guest dialler outlived the agent that was its only other owner — a helper \
                 must have leaked past the exchange that was supposed to settle it"
            )
        })
        .into_machine()
}

/// The whole of what a conversation needs from the credential store — the
/// account it runs on, the label it goes by among its fellows, the model,
/// and the effort asked for — read under one brief lock.  The credential is
/// not among them: the [`Bureau`] resolves that when it mints the provider.
struct Selected {
    account: Account,
    label: String,
    model: String,
    effort: Option<String>,
}

/// Everything slow in [`Conversation::begin`] (the machine's boot, the
/// folder's safety copy) happens after this returns, so a sign-in in the
/// window is never held up behind a conversation opening, nor the other way
/// round.
///
/// `choice.account` resolves by id alone, through [`resolve_account`] —
/// never by the label a human reads, which two accounts can share; naming
/// one by its display label is the CLI's business (`--provider`), not the
/// window's, whose menu only ever hands back what it was given.
///
/// # Errors
/// Returns `Err` if this computer has no account set up, if `choice` names
/// one that has since gone, or if the sole account names no default model.
fn select_account(
    store: &Mutex<CredentialStore>,
    choice: Option<Choice>,
) -> Result<Selected, String> {
    let store = store.lock_ignore_poison();
    let available = store.available();
    if available.is_empty() {
        return Err(
            "no assistant account is set up on this computer — sign in with ChatGPT on the \
             opening screen, or ask your IT department to set a provider API key \
             (ANTHROPIC_API_KEY, OPENAI_API_KEY, …)"
                .into(),
        );
    }
    let (account, model, effort) = if let Some(Choice {
        account,
        model,
        effort,
    }) = choice
    {
        let account = resolve_account(&account, &available)
            .ok_or("the chosen account is no longer available on this computer")?;
        (account, model, effort)
    } else {
        let (account, model) = choose(&available)?;
        (account, model, None)
    };
    let label = identity::label(&account, &available);
    // Everything the caller does next is slow, and none of it is the
    // store's business.
    drop(store);
    Ok(Selected {
        account,
        label,
        model,
        effort,
    })
}

/// The seat the trunk drives the guest's engine from: `control`, the
/// machine's own control plane, adopted as a wire, working at `cwd`.
///
/// No `#[cfg]` appears below, though what
/// [`vm_manager::Machine::take_wires`] hands over differs by platform — an
/// `AF_VSOCK` descriptor under Virtualization.framework, an `AF_HYPERV`
/// socket under Hyper-V. [`ral_core::protocol::WireTransport::adopt`] takes
/// whatever converts into its own
/// [`WireStream`](ral_core::wire::WireStream), and each platform's owned
/// handle does, so the frame protocol never learns which hypervisor it is
/// talking through.
///
/// # Errors
/// Returns `Err` if the control plane cannot be adopted as a wire.
fn control_seat(
    control: impl Into<ral_core::wire::WireStream>,
    cwd: std::path::PathBuf,
) -> Result<exarch::agent::RootSeat, String> {
    Ok(exarch::agent::RootSeat::Wire {
        transport: Box::new(
            ral_core::protocol::WireTransport::adopt(
                control,
                ral_core::protocol::Liveness::default(),
            )
            .map_err(|e| format!("could not take control of the machine: {e}"))?,
        ),
        cwd,
        // Home is the guest scratch, not the workspace: `$HOME` is where
        // XDG-defaulting tools drop caches and dotfiles, and pointed at
        // `/work` that litter would land among the user's own documents —
        // and in every change report.
        home: std::path::PathBuf::from(crate::grant::GUEST_SCRATCH),
    })
}

/// Hand the net wire to [`guest_net::run`], which owns it from here on.
///
/// # Errors
/// Returns `Err` if `guest_net::run` cannot start guest networking.
fn net_seat(
    net: impl Into<NetWire>,
    egress: exarch::egress::Egress,
) -> Result<guest_net::Session<NetWire>, String> {
    guest_net::run(
        net.into(),
        guest_net::Config {
            egress,
            gateway: vm_manager::GUEST_LINK.gateway,
            dialer: Arc::new(guest_net::vet::System),
        },
    )
    .map_err(|e| format!("could not start guest networking: {e}"))
}

/// The account and model for a run whose [`Choice`] left both unnamed:
/// whichever one account is set up on this computer, and its default
/// model. An account that names no default model is a question for the
/// user, refused in the same plain register as having no account at all —
/// there is no menu entry left to answer it with.
fn choose(available: &[Account]) -> Result<(Account, String), String> {
    let account = &available[0];
    account
        .service
        .default_model
        .clone()
        .map(|model| (account.clone(), model))
        .ok_or_else(|| {
            format!(
                "the account set up on this computer ('{}') does not say which model to \
                 use — ask your IT department to set one up.",
                identity::label(account, available)
            )
        })
}

/// The tuning [`Choice::effort`] resolves to, masked against what the
/// pricing catalog positively knows `model` supports.
///
/// An absent `effort` carries [`provider::Tuning::initial`]'s thinking-on
/// default forward untouched; `Some(label)` resolves strictly against
/// [`provider::EFFORT_LADDER`] — `"auto"` lands on `effort: None`
/// deliberately, distinct from the absent case landing on
/// [`provider::Tuning::initial`]'s `Some(Medium)`. Loads the pricing
/// catalog first (best effort — see [`pricing::ensure_loaded_blocking`]), then masks
/// the resolved effort to `None` when [`pricing::caps_or_default`]
/// positively reports the model does not take reasoning at all; before the
/// catalog loads, or on a lookup miss, that call reads the model as
/// reasoning-capable and no masking happens.
///
/// # Errors
/// Returns `Err` if `effort` names no rung on [`provider::EFFORT_LADDER`].
fn resolve_tuning(effort: Option<String>, model: &str) -> Result<provider::Tuning, String> {
    let tuning = match effort {
        None => provider::Tuning::initial(),
        Some(label) => provider::Tuning {
            effort: provider::effort_by_label(&label)?,
            temperature: None,
            top_p: None,
        },
    };
    pricing::ensure_loaded_blocking();
    Ok(mask_unsupported_effort(
        tuning,
        pricing::caps_or_default(model).supports("reasoning"),
    ))
}

/// Force `tuning.effort` to `None` when `reasoning` is `false`, leaving
/// every other field untouched — the actual masking step
/// [`resolve_tuning`] applies once it has learned whether the model takes a
/// reasoning control at all. Split out from that lookup so the masking
/// itself has a seam a test can reach without needing the pricing
/// catalog's own network-fetched, process-global snapshot to have loaded a
/// model that positively lacks the parameter.
fn mask_unsupported_effort(mut tuning: provider::Tuning, reasoning: bool) -> provider::Tuning {
    if !reasoning {
        tuning.effort = None;
    }
    tuning
}

#[cfg(test)]
mod tests {
    use super::*;
    use exarch::provider::ReasoningEffort;

    #[test]
    fn resolve_tuning_with_no_effort_keeps_the_thinking_on_default() {
        let tuning = resolve_tuning(None, "claude-opus-4").unwrap();
        assert_eq!(tuning, provider::Tuning::initial());
    }

    #[test]
    fn resolve_tuning_rejects_an_unknown_effort_label() {
        let err = resolve_tuning(Some("extreme".into()), "claude-opus-4").unwrap_err();
        assert!(err.contains("extreme"), "got: {err}");
    }

    #[test]
    fn resolve_tuning_auto_is_a_deliberate_none_not_an_absence() {
        let tuning = resolve_tuning(Some("auto".into()), "claude-opus-4").unwrap();
        assert!(tuning.effort.is_none());
        assert_ne!(
            tuning,
            provider::Tuning::initial(),
            "an explicit 'auto' must not read back as the thinking-on default"
        );
    }

    #[test]
    fn mask_unsupported_effort_clears_only_the_effort() {
        let tuning = provider::Tuning {
            effort: Some(ReasoningEffort::Medium),
            temperature: Some(0.5),
            top_p: None,
        };

        let masked = mask_unsupported_effort(tuning.clone(), false);
        assert!(masked.effort.is_none());
        assert_eq!(masked.temperature, Some(0.5));

        let kept = mask_unsupported_effort(tuning, true);
        assert!(matches!(kept.effort, Some(ReasoningEffort::Medium)));
    }
}
