//! Where every [`Avatar`] comes from and how it ends: `root` and `for_test`
//! build a trunk, `fork` and `branch` a child, all four through `assemble`
//! and its [`Build`] bundle. `clear` rebuilds a node's context in place
//! rather than ending it; `Drop` is the one exit every life takes.

use crate::agent::cancel::EvalReach;
use crate::agent::dial::Dial;
use crate::agent::event::{AgentLog, ContextOp, EditAuthority};
use crate::agent::seat::{self, Seat};
use crate::agent::shell::LogCell;
use crate::agent::{Agent, Avatar, ProviderHandle, SPAWN_FUEL, cancel, nudge};
use crate::bootstrap::Scratch;
use crate::bus::{AgentId, Emitter, Inbox};
use crate::fleet::{Fleet, Unborn};
use crate::prompt::Grants;
use crate::provider::Provider;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) fn fresh_id() -> AgentId {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn seed_id_counter(sessions_root: &std::path::Path) -> io::Result<()> {
    let max = match std::fs::read_dir(sessions_root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry.file_type().ok().filter(std::fs::FileType::is_dir)?;
                entry.file_name().to_str()?.parse::<AgentId>().ok()
            })
            .max()
            .unwrap_or(0),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    NEXT_ID.fetch_max(max.saturating_add(1), Ordering::Relaxed);
    Ok(())
}

/// The trunk's fleet name, and so the label its tab carries: the frontend reads
/// the root tab's name off the agent, which is what lets `/focus` resolve the
/// trunk by the same name every other agent answers to.
const TRUNK_NAME: &str = "main";

/// What `Avatar::assemble` needs.  Fields are `pub(crate)` because a desk
/// handler builds a spawned child's literal from its captured `HostServices`,
/// the one place lawfully holding the adopted nursery shell; `fork_with` is
/// the ordinary in-thread path.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool sets an independent, orthogonal axis on the constructed agent (interactive, returns, allow_schedule, tool_enabled, search); not a candidate for a combined enum"
)]
pub(crate) struct Build {
    /// The tab-bar identity — known to every caller before construction, `/branch`'s
    /// and `` agents `start ``'s own choice reached down to here.
    pub(crate) name: String,
    /// The still-unresolved template, carrying the builtin-index placeholder
    /// so the constructed agent's own children resolve from it in turn.
    /// `Arc<str>` so a fork's inheritance is a bump, not a re-copy.
    pub(crate) system: Arc<str>,
    /// `system` resolved for this bundle's own [`Grants`] — what reaches the
    /// model. The caller resolves it because the log's `SessionStarted`
    /// bookend records the resolved length, and the log must exist before the
    /// agent it describes does.
    pub(crate) system_prompt: String,
    /// The fleet-shared builtin index, carried on so this agent's own forks
    /// resolve theirs without a live shell.
    pub(crate) index: Arc<crate::prompt::BuiltinIndex>,
    pub(crate) caps: ral_core::types::Capabilities,
    /// Already through its identity ceremony: `assemble` seats no engine of
    /// its own, so every construction site states which seat kind it builds.
    pub(crate) seat: Seat,
    pub(crate) log: AgentLog,
    pub(crate) run_lock: Option<crate::bootstrap::RunLock>,
    pub(crate) resume_summary: Option<(u64, u64)>,
    /// Whom the constructed agent reports to — `None` builds a root, the
    /// trunk or a `/branch` child, which converses and never delivers.
    pub(crate) parent: Option<Arc<Agent>>,
    pub(crate) fuel: u32,
    pub(crate) provider: ProviderHandle,
    pub(crate) interactive: bool,
    pub(crate) returns: bool,
    pub(crate) allow_schedule: bool,
    /// Whether provider requests advertise the `ral` tool at all — `false`
    /// only for a `--chat` trunk.
    pub(crate) tool_enabled: bool,
    /// Whether the agent may ride the provider's own hosted web search —
    /// bounded by the IT policy verdict, never a CLI flag or user config.
    pub(crate) search: bool,
    /// Fresh for the trunk, the parent's clone for a fork: one fleet per run.
    pub(crate) fleet: Arc<Fleet>,
    /// A host setting, not a per-agent choice: every fork inherits the trunk's
    /// ceiling verbatim.
    pub(crate) disk_warn_bytes: Option<u64>,
    /// IT's network policy, audit ledger, and rate budget — likewise a host
    /// setting, inherited verbatim.
    pub(crate) egress: crate::egress::Egress,
    /// Shared verbatim by every fork — `` agents `start ``'s wire arm reads it off
    /// its own agent, never off a fresh construction.
    pub(crate) dial: Option<Arc<dyn Dial>>,
    /// This agent's reach into its own running eval, read off `seat` before it
    /// moves into this bundle — a root states it pre-weakened
    /// ([`EvalReach::interrupt_only`]).
    pub(crate) reach: EvalReach,
}

/// Why a fork did not happen.
#[derive(Debug)]
pub(crate) enum Unforked {
    /// The child's own session log could not be opened off its parent's.
    Log(io::Error),
    /// The fleet refused the child — see [`Unborn`].
    Unborn(Unborn),
}

impl From<Unborn> for Unforked {
    fn from(why: Unborn) -> Self {
        Self::Unborn(why)
    }
}

impl std::fmt::Display for Unforked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Log(error) => write!(f, "could not fork the session log: {error}"),
            Self::Unborn(why) => write!(f, "{why}"),
        }
    }
}

/// A session's provider identity, snapshotted for the record.
///
/// What the account was called, and which service and account it was, at the
/// moment the session began. `Clone` because [`AgentLog::fork`] carries it
/// into every child's own `SessionStarted` bookend unchanged.
#[derive(Clone, Debug)]
pub struct RecordedAccount {
    pub label: String,
    pub service: String,
    pub id: String,
}

impl RecordedAccount {
    /// A snapshot for tests that only care that *something* is recorded.
    /// Not `#[cfg(test)]`: integration test binaries link the library built
    /// without it, so a fixture they share with the unit tests must be an
    /// ordinary function, as [`crate::agent::Avatar::for_test`] already is.
    #[doc(hidden)]
    pub fn for_test(name: &str) -> Self {
        Self {
            label: name.to_string(),
            service: name.to_string(),
            id: name.to_string(),
        }
    }
}

/// Everything [`Avatar::root`] needs beyond the seat choice and the provider.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool sets an independent, orthogonal axis on the trunk (allow_schedule, interactive, chat); not a candidate for a combined enum"
)]
pub struct RootConfig {
    pub system: String,
    pub caps: ral_core::types::Capabilities,
    pub run_dir: std::path::PathBuf,
    pub resume: Option<std::path::PathBuf>,
    pub no_logs: bool,
    pub run_lock: Option<crate::bootstrap::RunLock>,
    pub model: String,
    pub account: RecordedAccount,
    pub allow_schedule: bool,
    pub interactive: bool,
    pub chat: bool,
    pub disk_warn_bytes: Option<u64>,
    /// The depth budget this trunk starts with — `SPAWN_FUEL`, the one
    /// figure both products' trunks carry: every agent may delegate, and an
    /// exchange or a chain of forks ends only once the whole tree does.
    pub fuel: u32,
    /// IT's network policy, audit ledger, and rate budget, opened once at
    /// launch and threaded into every fork verbatim.
    pub egress: crate::egress::Egress,
    /// The dial-side capability a wire trunk reaches its helpers through;
    /// `None` for every identity trunk. A wire trunk built with `fuel > 0`
    /// and no dialler is refused here — never a runtime surprise reached
    /// only once a model calls `agent`.
    pub dial: Option<Arc<dyn Dial>>,
}

/// Where the trunk's engine lives — [`Avatar::root`]'s one construction-time
/// choice.
pub enum RootSeat {
    /// In-process, over an identity transport. `cwd` is stated rather than
    /// read from the process, since a GUI host has no per-conversation
    /// process directory to chdir into; `detach` says whether the host judged
    /// the verb meaningful here at all.
    Identity {
        scratch: Arc<Scratch>,
        cwd: std::path::PathBuf,
        detach: bool,
    },
    /// Out-of-process: an already-built `transport` onto an engine elsewhere,
    /// a spawned `--engine` child or synod's adopted control-plane stream into
    /// a guest VM. `cwd`/`home` come from the caller because under a VM the
    /// workspace is a guest path this process cannot resolve.
    Wire {
        transport: Box<ral_core::protocol::WireTransport>,
        cwd: std::path::PathBuf,
        home: std::path::PathBuf,
    },
}

impl Avatar {
    /// Build an agent and its avatar in one step: the agent is in its parent's
    /// subtree, holds its name, and carries a lease before this returns, so no
    /// caller can ever hold an unenrolled child.
    ///
    /// # Errors
    /// Whatever [`Fleet::enrol`] refuses.  The refusal is decided before the
    /// avatar exists, so there is nothing to unwind.
    pub(crate) fn assemble(b: Build) -> Result<Self, Unborn> {
        let Build {
            name,
            system,
            system_prompt,
            index,
            caps,
            seat,
            log,
            parent,
            fuel,
            provider,
            interactive,
            returns,
            allow_schedule,
            tool_enabled,
            search,
            fleet,
            run_lock,
            resume_summary,
            disk_warn_bytes,
            egress,
            dial,
            reach,
        } = b;
        let nudges = tool_enabled.then(nudge::Nudges::new);
        let log_dir = log.dir().to_path_buf();
        let inbox = Inbox::new();
        let mailbox = inbox.mailbox();
        // The parent's generation as it stands right now.  Its own avatar is
        // the only writer, and that avatar is the thread running this
        // construction, so nothing can bump it between here and the enrolment
        // below.
        let consumer = parent.as_ref().map_or(0, |p| p.generation());
        let agent = Arc::new(Agent {
            id: log.id(),
            name,
            log_dir,
            started: std::time::Instant::now(),
            system: system_prompt.into(),
            system_base: system,
            index,
            caps,
            parent,
            children: Mutex::new(Vec::new()),
            fuel,
            provider,
            interactive,
            tool_enabled,
            search,
            returns,
            allow_schedule,
            disk_warn_bytes,
            egress,
            dial,
            cancel: cancel::Token::new(),
            reach,
            mailbox,
            schedules: crate::fleet::schedule::ScheduleRegistry::new(),
            pins: Arc::default(),
            status: Mutex::new(super::Status {
                generation: 0,
                rest: None,
                reply: None,
                awaiting: std::collections::BTreeSet::new(),
            }),
            consumer,
        });
        fleet.enrol(&agent)?;
        Ok(Self {
            agent,
            log: LogCell::new(log),
            _run_lock: run_lock,
            resume_summary,
            seat,
            inbox,
            nudges,
            reply: None,
            fleet,
            last_input: (0, 0),
            ral_epoch: 0,
            disk_check_epoch: 0,
            disk_warn_latched: false,
        })
    }

    /// The trunk — the root of a fresh fleet.  Creates the fleet's shared
    /// index, so the frontend can build its [`Fleet`](crate::fleet::Fleet) by
    /// reading these handles back.  `cfg.interactive` makes it the *conversing*
    /// trunk; off it, a one-shot headless one.
    ///
    /// # Errors
    /// If the trunk's session directory or its event log cannot be opened.
    ///
    /// # Panics
    /// Never: the `expect` below only restates for the compiler that an
    /// identity seat's shell was built a few lines above.
    pub fn root(cfg: RootConfig, root_seat: RootSeat, provider: Arc<Provider>) -> io::Result<Self> {
        let RootConfig {
            system,
            caps,
            run_dir,
            resume,
            no_logs,
            run_lock,
            model,
            account,
            allow_schedule,
            interactive,
            chat,
            disk_warn_bytes,
            fuel,
            egress,
            dial,
        } = cfg;
        // The CLI rejects both pairings at parse; these hold the invariant for
        // every other caller that builds a root.
        if resume.is_some() && no_logs {
            return Err(io::Error::other(
                "cannot resume a logless session — it writes nothing to reopen",
            ));
        }
        if resume.is_some() && chat {
            return Err(io::Error::other(
                "cannot resume a chat session — chat keeps no resumable harness history",
            ));
        }
        if resume.is_some() && matches!(&root_seat, RootSeat::Wire { .. }) {
            return Err(io::Error::other(
                "--resume is unavailable for a wire seat — the engine process is gone; resume an identity session instead",
            ));
        }
        // Stated, not discovered by a model calling `agent`: a fuelled wire
        // trunk with no dialler to reach helpers through cannot ever answer
        // `` agents `start ``'s wire arm, so refuse the construction itself.
        if matches!(&root_seat, RootSeat::Wire { .. }) && fuel > 0 && dial.is_none() {
            return Err(io::Error::other(
                "a wire trunk with spawn fuel needs a dialler to reach helper engines through — \
                 pass one via RootConfig::dial, or build this trunk with fuel: 0",
            ));
        }
        // Read before `egress` moves into `Build` below.
        let search = egress.policy.search;
        // An identity seat resolves its builtin index off the very shell it
        // goes on to run calls through; a wire seat's real shell lives in the
        // remote engine, so `exarch_shell` dresses a throwaway with the same
        // compiled-in surface and index resolution reads that instead.
        let identity_shell = match &root_seat {
            RootSeat::Identity {
                scratch,
                cwd,
                detach,
            } => Some(seat::boot_root_shell(scratch, cwd.clone(), *detach)),
            RootSeat::Wire { .. } => None,
        };
        // A binding of its own, so the stand-in outlives the borrow below.
        let throwaway_wire_shell;
        let index = crate::prompt::BuiltinIndex::resolve(if let Some(shell) = &identity_shell {
            shell
        } else {
            throwaway_wire_shell =
                crate::bootstrap::exarch_shell(ral_core::io::TerminalState::default());
            &throwaway_wire_shell
        });
        // Chat advertises no tool, so its prompt takes no builtin surface at
        // all — no index, no taught section, the bare stand-in verbatim.
        let system_prompt = if chat {
            system.clone()
        } else {
            index.apply(
                &system,
                &Grants {
                    returns: !interactive,
                    allow_schedule,
                    spawns: fuel > 0,
                },
            )
        };
        let root_dir = resume.as_deref().unwrap_or(&run_dir);
        let sessions_root = root_dir.join("sessions");
        let run_lock = if no_logs {
            None
        } else {
            match run_lock {
                Some(lock) => Some(lock),
                None => Some(crate::bootstrap::RunLock::try_acquire(root_dir)?),
            }
        };
        let (log, resume_summary) = if resume.is_some() {
            seed_id_counter(&sessions_root)?;
            let mut log = AgentLog::resume(&sessions_root, 0)?;
            let summary = log.resumed_summary();
            let at_unix_ms = crate::bootstrap::now_unix_ms();
            log.record_resumed(&model, &account, system_prompt.len(), at_unix_ms)?;
            (log, Some(summary))
        } else {
            let id = fresh_id();
            let log = if no_logs {
                AgentLog::root_without_logs(
                    &sessions_root,
                    id,
                    &model,
                    &account,
                    system_prompt.len(),
                )?
            } else {
                AgentLog::root(&sessions_root, id, &model, &account, system_prompt.len())?
            };
            (log, None)
        };
        let seat = match root_seat {
            RootSeat::Identity {
                scratch,
                cwd,
                detach,
            } => Seat::identity(
                identity_shell.expect("built above for an identity seat"),
                scratch,
                cwd,
                detach,
                &log,
            ),
            RootSeat::Wire {
                transport,
                cwd,
                home,
            } => Seat::wire(*transport, cwd, home)
                .map_err(|s| io::Error::other(seat::engine_gone(&s)))?,
        };
        // This seat is rebuilt in place under a standing root, so a raw reach
        // captured now would go stale — see `EvalReach::interrupt_only`.
        let reach = seat.eval_reach().interrupt_only();
        let avatar = Self::assemble(Build {
            name: TRUNK_NAME.to_string(),
            system: system.into(),
            system_prompt,
            index,
            caps,
            seat,
            log,
            parent: None,
            fuel,
            provider: ProviderHandle::new(provider),
            interactive,
            returns: !interactive,
            allow_schedule,
            // Chat mode advertises no tool at all: a bare conversation.
            tool_enabled: !chat,
            search,
            fleet: Fleet::new(),
            run_lock,
            resume_summary,
            disk_warn_bytes,
            egress,
            dial,
            reach,
        })
        // A brand-new fleet, so this can never collide or find its rootless
        // self dead.
        .expect("a fresh fleet's trunk is born unrefused");
        if resume.is_some() {
            avatar
                .log
                .lock()
                .import_context(vec![genai::chat::ChatMessage::user(
                    "session resumed from disk; the shell is fresh: bindings, workers, and cwd from before are gone, the scratch dir is new ($EXARCH_SCRATCH is per-pid, so scratch paths in the old context are dead), pinned state and scheduled events are gone (pin-list and schedules `list to confirm), and any sub-agents from before have ended.",
                )])
                .map_err(io::Error::other)?;
        }
        Ok(avatar)
    }

    pub(crate) fn clear(&mut self) -> io::Result<()> {
        let at_unix_ms = crate::bootstrap::now_unix_ms();
        // The queue goes first, before the seat reboot below spends real time
        // booting a shell: what is waiting *now* is old context, but a prompt
        // typed during the reboot was typed against an already-blanked screen
        // and belongs to the new one.  Sweeping it afterwards eats it in
        // silence — the queue holds no record of what it dropped.
        self.inbox.clear();
        let record = self.log.lock().clear(self.agent.system.len(), at_unix_ms)?;
        let error = record.rotation_error;
        // Rebooting the seat drops the outgoing shell, whose teardown cancels
        // its registered workers — `/clear` outranks every lease.
        self.seat.clear(&self.log.lock());
        // The rebuilt context is empty: the next step's usage sets this afresh.
        self.last_input = (0, 0);
        // A rebuilt context has been told nothing.
        if let Some(nudges) = &mut self.nudges {
            *nudges = nudge::Nudges::new();
        }
        // Abandon the subtree — this agent itself stays live — and disarm the
        // schedules.  A straggler that composed its message before this call
        // carries its own stamp and is rejected at a consuming edge, so
        // neither order is load-bearing.
        self.agent.clear_subtree();
        self.agent.schedules.clear();
        // The frontend wipes its pin register on `/clear`, so the session's
        // mirror must follow.
        self.agent
            .pins
            .lock()
            .expect("pin register poisoned")
            .clear();
        error.map_or(Ok(()), Err)
    }

    pub(crate) fn rewind(&mut self, anchor: u64, emit: &Emitter) -> Result<(), String> {
        // Coupled first, so the edit's record — the one notification there is,
        // now that `apply_edit` authors it through the seam — publishes live.
        self.couple(emit);
        {
            let mut log = self.log.lock();
            let exchanges = log.rewind_exchanges(anchor)?;
            let _receipt = log.apply_edit(ContextOp::Drop { exchanges }, EditAuthority::User)?;
        }
        self.inbox.drop_nudges();
        if let Some(nudges) = &mut self.nudges {
            *nudges = nudge::Nudges::new();
        }
        Ok(())
    }

    /// A plain returning fork, under a minted name.  Tests only: a production
    /// spawn assembles its own `Build` through the desk's spawn spine
    /// (`ExarchDesk::launch`), and `/branch` calls `fork_with` directly with
    /// the name it already chose.
    #[cfg(test)]
    pub(crate) fn fork(&self, caps: ral_core::types::Capabilities) -> Result<Self, Unforked> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        self.fork_named(
            caps,
            &format!("fork-{}", SEQ.fetch_add(1, Ordering::Relaxed)),
        )
    }

    /// A returning fork under a chosen name — the shape a production spawn
    /// assembles, for a test that goes on to name the child in an assertion.
    #[cfg(test)]
    pub(crate) fn fork_named(
        &self,
        caps: ral_core::types::Capabilities,
        name: &str,
    ) -> Result<Self, Unforked> {
        self.fork_with(caps, true, name.to_string())
    }

    /// An independent child of this agent capped at `caps`; `returns` decides
    /// whether it holds `reply` — both the prompt's advertised builtin index
    /// and the desk's refusal read that one bit, so they cannot disagree.
    fn fork_with(
        &self,
        caps: ral_core::types::Capabilities,
        returns: bool,
        name: String,
    ) -> Result<Self, Unforked> {
        // Scope, dynamic context, and installed builtin table cross in this
        // one step, because core owns the flow matrix — no hand-copied field
        // here can drift from it.  Per-agent state starts fresh.
        let shell = self.seat.shell_mut().shell.fork_session();
        let child_id = fresh_id();
        let fuel = self.agent.fuel.saturating_sub(1);
        // Against the *child's* grants, never this agent's: a `/branch` child
        // withholds `reply` however its creator's own bit reads.
        let system_prompt = self.agent.index.apply(
            &self.agent.system_base,
            &Grants {
                returns,
                allow_schedule: self.agent.allow_schedule,
                spawns: fuel > 0,
            },
        );
        let log = self
            .log
            .lock()
            .fork(child_id, system_prompt.len())
            .map_err(Unforked::Log)?;
        let seat = match &self.seat {
            // No detach: `fork_session` carries no such policy across, so
            // granting it here would grant nothing now and conjure the verb
            // at the child's first `/clear`, which reboots from the seat.
            Seat::Identity { scratch, cwd, .. } => {
                Seat::identity(shell, scratch.clone(), cwd.clone(), false, &log)
            }
            Seat::Wire { .. } => {
                unreachable!("shell_mut already panicked above for a wire seat")
            }
        };
        let reach = seat.eval_reach();
        Ok(Self::assemble(Build {
            name,
            system: self.agent.system_base.clone(),
            system_prompt,
            index: self.agent.index.clone(),
            caps,
            seat,
            log,
            // A branch converses and never returns, so it reports to nobody and
            // roots its own tree.  Its fuel, caps, and prompt are copied from
            // its creator right here, which is the whole of what an edge to the
            // creator would have bounded.
            parent: returns.then(|| self.agent.clone()),
            fuel,
            // Seeded from the parent's *current* provider, so a later `/model`
            // on either never disturbs the other.
            provider: ProviderHandle::new(self.agent.provider.current()),
            // Human-attachment is inherited; engagement is not, being read off
            // the child's own exchange clock from its first exchange.
            interactive: self.agent.interactive,
            returns,
            allow_schedule: self.agent.allow_schedule,
            // `--chat` is trunk-only, so every fork keeps the tool.
            tool_enabled: true,
            // Never a fresh grant: a child's reach is bounded by its parent's.
            search: self.agent.search,
            fleet: self.fleet.clone(),
            run_lock: None,
            resume_summary: None,
            disk_warn_bytes: self.agent.disk_warn_bytes,
            egress: self.agent.egress.clone(),
            dial: self.agent.dial.clone(),
            reach,
        })?)
    }

    /// Fork a conversing child under `name`: the creator's context and
    /// capabilities verbatim, but `reply` withheld, so it parks for the
    /// human instead of returning a value.
    ///
    /// # Errors
    /// Whatever [`Self::fork_with`] refuses.
    pub(crate) fn branch(&self, name: String) -> Result<Self, Unforked> {
        let child = self.fork_with(self.agent.caps.clone(), false, name)?;
        self.inherit_context(&child)?;
        Ok(child)
    }

    /// Import the creator's model-visible context into `child`, mnemon-style.
    fn inherit_context(&self, child: &Self) -> Result<(), Unforked> {
        let messages = self.log.lock().inherited_context_messages();
        child
            .log
            .lock()
            .import_context(messages)
            .map_err(|why| Unforked::Log(io::Error::other(why)))
    }

    /// Seed a freshly forked child's inbox with its launch prompt — the spawn
    /// site calls this once and then drops its handle, so it is the only
    /// downward edge.
    pub(crate) fn seed(&self, prompt: String) {
        self.inbox.push_user(prompt);
    }

    /// A trunk against a throwaway sessions root under `dir`: unrestricted
    /// capabilities, a baked shell, and an empty [`Provider::scripted`] the
    /// harness fills.  Non-interactive, so it terminates at quiescence like
    /// any returning agent and `attend` never blocks.
    ///
    /// # Errors
    /// If the throwaway scratch or the session log inside it cannot be created.
    pub fn for_test(system: &str) -> io::Result<Self> {
        Self::for_test_with(TestTrunk::new(system))
    }

    /// [`Self::for_test`] with a knob turned — the bits an agent fixes at
    /// construction and no test can set afterwards, its `Arc` being shared the
    /// moment it exists.
    ///
    /// # Errors
    /// If the throwaway scratch or the session log inside it cannot be created.
    ///
    /// # Panics
    /// If the test process has no cwd.
    pub(crate) fn for_test_with(cfg: TestTrunk) -> io::Result<Self> {
        let TestTrunk {
            system,
            allow_schedule,
            egress,
            disk_warn_bytes,
            lease,
        } = cfg;
        // Derived from the policy, exactly as `root` derives it, so a fixture
        // can never claim a reach its own egress denies.
        let search = egress.policy.search;
        let mut shell = crate::bootstrap::boot_shell();
        let id = fresh_id();
        // Keyed by this agent's own fresh id, so concurrent tests never
        // contend on one dir.  Seeded into the shell exactly as a real boot
        // seeds it, and before the seat arms the ledgers: unseeded, an
        // `$EXARCH_SCRATCH` probe falls through to the host process env, and a
        // suite run from inside a live exarch session would measure *that*
        // session's scratch.
        let scratch = Arc::new(Scratch::for_test(
            crate::bootstrap::EXARCH,
            &format!("agent-{id}"),
        )?);
        scratch.install_into(&mut shell);
        let index = crate::prompt::BuiltinIndex::resolve(&shell);
        let system_prompt = index.apply(
            &system,
            &Grants {
                returns: true,
                allow_schedule,
                spawns: SPAWN_FUEL > 0,
            },
        );
        // Beside the scratch, which the seat below owns: the session's whole
        // footprint is then one directory, and it goes when the agent does.
        let log = AgentLog::root(
            &scratch.test_sibling("sessions")?,
            id,
            "test-model",
            &RecordedAccount::for_test("test"),
            system_prompt.len(),
        )?;
        let provider = ProviderHandle::new(Arc::new(Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new(),
        )));
        let cwd = std::env::current_dir().expect("test process has a cwd");
        let seat = Seat::identity(shell, scratch, cwd, false, &log);
        // Pre-weakened for the same reason a real root's is — see `root`.
        let reach = seat.eval_reach().interrupt_only();
        Ok(Self::assemble(Build {
            name: TRUNK_NAME.to_string(),
            system: system.into(),
            system_prompt,
            index,
            caps: ral_core::types::Capabilities::default(),
            seat,
            log,
            parent: None,
            fuel: SPAWN_FUEL,
            provider,
            interactive: false,
            returns: true,
            allow_schedule,
            tool_enabled: true,
            search,
            fleet: Fleet::with_lease(lease),
            run_lock: None,
            resume_summary: None,
            disk_warn_bytes,
            egress,
            dial: None,
            reach,
        })
        .expect("a fresh fleet's trunk is born unrefused"))
    }
}

/// What a unit test may vary about a [`Avatar::for_test`] trunk.
pub(crate) struct TestTrunk {
    pub(crate) system: String,
    pub(crate) allow_schedule: bool,
    /// The IT policy the trunk's `search` reach is derived from.
    pub(crate) egress: crate::egress::Egress,
    pub(crate) disk_warn_bytes: Option<u64>,
    /// The idle bound of the fleet this trunk is born into.
    pub(crate) lease: std::time::Duration,
}

impl TestTrunk {
    pub(crate) fn new(system: &str) -> Self {
        Self {
            system: system.to_string(),
            allow_schedule: false,
            egress: crate::egress::Egress::for_test(),
            disk_warn_bytes: None,
            lease: crate::fleet::AGENT_LEASE_IDLE,
        }
    }
}

impl Drop for Avatar {
    /// The one exit every life takes, and the whole of deregistration: the
    /// agent's last strong reference goes with this, so its parent's subtree
    /// and both fleet doors prune it at the next walk.  A cascade cancels only
    /// an agent's *eval root*, leaving its armed schedules for whoever drops
    /// it, so they are cleared here unconditionally and its workers die with
    /// the seat below: a settled-but-never-cancelled agent leaks neither.
    /// `/clear` never reaches this — it rebuilds in place, clearing its own.
    fn drop(&mut self) {
        self.agent.schedules.clear();
        let recorded = self.log.lock().record_session_ended();
        if let Err(error) = recorded {
            eprintln!("exarch: the session's tail bookend was not recorded: {error}");
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::testkit::*;
    use crate::bus::{Emitter, Item, Post};
    use crate::provider::scripted::Script;
    use genai::chat::ChatMessage;
    use std::fs;

    /// A forked child inherits the parent's installed builtin surface, not
    /// just the core set a bare `Shell::new` seeds.
    #[test]
    fn fork_inherits_host_builtins() {
        let session = Avatar::for_test("system").unwrap();
        assert!(
            session
                .seat
                .shell_mut()
                .shell
                .lookup_builtin("view-text")
                .is_some(),
            "the parent boot shell must carry the exarch host builtins"
        );
        let child = session
            .fork(session.caps().clone())
            .expect("fork child session");
        for name in ["view-text", "grep-files", "edit-hash", "explore-dir"] {
            assert!(
                child.seat.shell_mut().shell.lookup_builtin(name).is_some(),
                "the forked child must inherit the host builtin `{name}`"
            );
        }
    }

    /// Fuel bounds depth, not fan-out: siblings cost the parent nothing, and
    /// a chain walked all the way down bottoms out at zero rather than
    /// wrapping.
    #[test]
    fn fork_fans_out_without_spending_the_parents_fuel() {
        let parent = Avatar::for_test("system").unwrap();
        assert_eq!(parent.agent.fuel, SPAWN_FUEL);
        for _ in 0..3 {
            let child = parent.fork(parent.caps().clone()).expect("fork child");
            assert_eq!(
                child.agent.fuel,
                SPAWN_FUEL - 1,
                "each child starts one below the parent, regardless of how many siblings it has"
            );
        }
        assert_eq!(
            parent.agent.fuel, SPAWN_FUEL,
            "fork never touches the parent's own fuel — fan-out is unbounded"
        );

        let mut chain = parent;
        for expected in (0..SPAWN_FUEL).rev() {
            chain = chain.fork(chain.caps().clone()).expect("fork child");
            assert_eq!(chain.agent.fuel, expected);
        }
        assert_eq!(
            chain.agent.fuel, 0,
            "the chain must bottom out at zero, not wrap"
        );
    }

    /// A fork carries its parent's search reach verbatim, in both directions
    /// — the ceiling the desk's own clamp narrows a spawn against.
    #[test]
    fn fork_inherits_its_parents_search_reach() {
        let parent = Avatar::for_test("system").unwrap();
        assert!(parent.fork(parent.caps().clone()).unwrap().agent.search);
        let searchless = searchless_trunk();
        assert!(
            !searchless
                .fork(searchless.caps().clone())
                .unwrap()
                .agent
                .search,
            "a searchless parent can hand out no search of its own"
        );
    }

    /// The provider is per-agent: a later swap on either side never disturbs
    /// the other — what `/model` on the focused agent relies on.
    #[test]
    fn fork_seeds_its_own_provider_handle() {
        let parent = Avatar::for_test("system").unwrap();
        parent.agent.provider.swap(scripted("p-a", Script::new()));
        let child = parent.fork(parent.caps().clone()).expect("fork child");
        assert_eq!(
            child.agent.provider.current().model(),
            "p-a",
            "the child seeds its handle from the parent's current provider"
        );
        parent.agent.provider.swap(scripted("p-b", Script::new()));
        assert_eq!(parent.agent.provider.current().model(), "p-b");
        assert_eq!(
            child.agent.provider.current().model(),
            "p-a",
            "a swap on the parent never disturbs an already-forked child"
        );
    }

    /// A branch imports the creator's context and withholds `reply`, but is
    /// otherwise an ordinary fork: caps verbatim, one less fuel.
    #[test]
    fn branch_imports_context_and_withholds_reply() {
        let parent = Avatar::for_test("system").unwrap();
        parent
            .log
            .lock()
            .append_user("what did we learn?".into(), None)
            .unwrap();
        parent
            .log
            .lock()
            .append_assistant(
                genai::chat::ChatMessage::assistant("the invariant matters"),
                vec![],
                None,
            )
            .unwrap();

        let child = parent.branch("branch".into()).expect("branch child");

        let view = serde_json::to_string(
            &child
                .log
                .lock()
                .history_transcript()
                .messages()
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(view.contains("what did we learn?"));
        assert!(view.contains("the invariant matters"));
        assert!(
            view.rfind("the invariant matters") > view.rfind("what did we learn?"),
            "the creator's context is imported in mnemon order: {view}"
        );

        assert!(
            !child.returns(),
            "a branch withholds `reply` and never returns"
        );
        assert_eq!(
            child.caps(),
            parent.caps(),
            "a branch inherits the creator's capabilities verbatim"
        );
        assert_eq!(
            child.agent.fuel,
            parent.agent.fuel - 1,
            "a branch is a fork: its fuel is one less than the parent's"
        );
    }

    /// Read `system_prompt_bytes` off a session's `SessionStarted` bookend,
    /// the first record in its `record.jsonl`.
    fn recorded_system_prompt_bytes(log_dir: &std::path::Path) -> usize {
        let records = crate::record::read_records(&log_dir.join("record.jsonl")).unwrap();
        let first = records
            .into_iter()
            .next()
            .expect("record.jsonl must have at least one record");
        match first {
            crate::record::Record::Protocol(crate::record::Protocol::SessionStarted {
                system_prompt_bytes,
                ..
            }) => system_prompt_bytes,
            other => panic!("first record must be SessionStarted, got {other:?}"),
        }
    }

    /// The bookend records the child's own resolved length — never a
    /// parent's, never the raw template's — checked in both directions a
    /// `returns` flip can take.
    #[test]
    fn fork_and_branch_bookend_record_the_childs_own_resolved_length() {
        let dir = tmp("bookend-resolved-length");
        let scratch = Scratch::for_test(crate::bootstrap::EXARCH, "bookend-resolved-length")
            .expect("scratch dir");
        let template = format!(
            "persona\n\n# Builtins\n\n{}",
            crate::prompt::BUILTIN_INDEX_PLACEHOLDER
        );
        let root = Avatar::root(
            RootConfig {
                system: template,
                caps: ral_core::types::Capabilities::default(),
                run_dir: dir.path().to_owned(),
                resume: None,
                no_logs: false,
                run_lock: None,
                model: "test-model".into(),
                account: RecordedAccount::for_test("test"),
                allow_schedule: false,
                // interactive: withholds `reply`.
                interactive: true,
                chat: false,
                disk_warn_bytes: None,
                fuel: SPAWN_FUEL,
                egress: crate::egress::Egress::for_test(),
                dial: None,
            },
            RootSeat::Identity {
                scratch: Arc::new(scratch),
                cwd: std::env::current_dir().expect("test process has a cwd"),
                detach: false,
            },
            scripted("test-model", Script::new()),
        )
        .expect("root trunk");

        let child = root.fork(root.caps().clone()).expect("fork child");
        assert_ne!(
            child.agent.system.len(),
            root.agent.system.len(),
            "the fork's bits must actually differ from its parent's for \
             this test to be meaningful"
        );
        assert_eq!(
            recorded_system_prompt_bytes(&child.log_dir()),
            child.agent.system.len(),
            "an ordinary fork's bookend must record its own resolved \
             system, not its non-returning parent's"
        );

        let grandchild = child
            .branch("grandchild".into())
            .expect("branch grandchild");
        assert_ne!(
            grandchild.agent.system.len(),
            child.agent.system.len(),
            "the branch's bits must actually differ from its parent's for \
             this test to be meaningful"
        );
        assert_eq!(
            recorded_system_prompt_bytes(&grandchild.log_dir()),
            grandchild.agent.system.len(),
            "a /branch child's bookend must record its own resolved \
             system, not its returning parent's"
        );
    }

    /// `/clear` cancels every registered worker, the durable class included,
    /// and the rebuilt shell starts empty.  A worker settling *after* the
    /// clear still flushes its batch to the inbox, stamped with its birth
    /// generation, and `Avatar::admits` is the edge that rejects it.  The
    /// workers stay deaf until `CLEAR_RELEASE` so they settle past the inbox
    /// drop; settling inside the clear would have `Inbox::clear` eat the
    /// batch instead, leaving this straggler path unexercised.
    #[test]
    fn clear_cancels_registered_workers_and_drops_their_late_surface() {
        let mut session = Avatar::for_test("system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);

        // The deferred sink `run_shell` wires captures `emit`'s mailbox, which
        // must be this session's own inbox for the late-surface assertion
        // below to mean anything.
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        let _ = session.run_shell(
            "c1".into(),
            "spawn { test-clear-block-until-released }",
            30,
            &emit,
        );
        let _ = session.run_shell(
            "c2".into(),
            r#"service "clear-test" { test-clear-block-until-released }"#,
            30,
            &emit,
        );

        let entries = session.seat.shell_mut().shell.workers();
        assert_eq!(entries.len(), 2, "one ordinary worker, one service");
        let durable = entries
            .iter()
            .find(|e| e.class == ral_core::types::LeaseClass::Durable)
            .expect("the service must register under the durable class");
        assert_eq!(durable.cmd, "clear-test");
        for entry in &entries {
            assert!(
                !entry.handle.cancel.is_cancelled(),
                "freshly spawned, not yet touched by /clear"
            );
        }

        session.clear().expect("clear must succeed");

        for entry in &entries {
            assert!(
                entry.handle.cancel.is_cancelled(),
                "/clear must cancel every registered worker, the durable class included ({})",
                entry.cmd
            );
        }
        assert_eq!(
            probe_int(&session, "worker-count"),
            0,
            "the rebuilt shell's registry must start empty"
        );

        // The clear has fully returned — its inbox drop is behind us — so the
        // workers may now observe cancellation and flush.  Generous budget:
        // the suite runs oversubscribed in a VM.
        CLEAR_RELEASE.store(true, std::sync::atomic::Ordering::Release);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        for entry in &entries {
            loop {
                if *entry.handle.state.lock().unwrap() != ral_core::types::HandleState::Running {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the cancelled worker must settle within the budget"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let late = session
            .inbox
            .next_item()
            .expect("a worker settling after /clear still posts its late surface batch");
        assert!(
            matches!(late, Item::Surface { .. }),
            "expected the late post to surface as an Item::Surface, got {late:?}"
        );
        assert!(
            !session.admits(&late),
            "the late batch's birth generation must be rejected by the rebuilt session"
        );
    }

    /// An agent that ends without ever being cancelled — the ordinary settle
    /// at the end of its `attend` — has no cascade edge pointed at it, so
    /// `Drop` is the only thing that reaches its workers.
    #[test]
    fn agent_drop_cancels_its_own_unclosed_workers() {
        let mut avatar = Avatar::for_test("system").unwrap();
        avatar
            .seat
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);

        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, avatar.agent.id, avatar.inbox.mailbox());
        let _ = avatar.run_shell("c1".into(), "spawn { test-clear-block-forever }", 30, &emit);

        let entries = avatar.seat.shell_mut().shell.workers();
        assert_eq!(entries.len(), 1, "the agent's own spawn must register");
        assert!(!entries[0].handle.cancel.is_cancelled(), "freshly spawned");

        drop(avatar);

        assert!(
            entries[0].handle.cancel.is_cancelled(),
            "dropping the agent (settle or cancel — however its life ended) \
             must cancel its own still-running workers"
        );
    }

    /// A self-armed wakeup re-arms on the shared reaper for as long as its
    /// guard lives, and the registry is `Arc`-shared with the reaper's
    /// closure, so it outlives a bare drop of the `Agent`: without `Drop`'s
    /// clear, a settled agent's cron fires into an inbox nobody drains.
    #[test]
    fn agent_drop_clears_its_own_armed_schedules() {
        let agent = Avatar::for_test("system").unwrap();
        let schedules = agent.agent.schedules.clone();
        schedules
            .schedule(
                crate::fleet::schedule::Trigger::After(std::time::Duration::from_hours(1)),
                "ping".into(),
                "ping".into(),
                &agent.inbox.mailbox(),
            )
            .expect("a one-hour `after` trigger must arm");
        assert_eq!(schedules.list().len(), 1, "the schedule is armed");

        drop(agent);

        assert!(
            schedules.list().is_empty(),
            "dropping the agent must clear its own armed schedules, the same \
             law /clear already applies explicitly"
        );
    }

    /// `/clear` re-arms over a freshly booted shell, so the fresh boot's scope
    /// is the new baseline and the pre-clear binding is gone with the whole
    /// old `Shell`, ledger included.
    #[test]
    fn clear_reseals_baseline_and_forgets_ledger() {
        let mut session = Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);
        session.run_shell("c0".into(), "let pre_clear_x = 1", 5, &emit);

        session.clear().expect("clear must succeed");

        assert!(
            !scope_has(&mut session, "pre_clear_x"),
            "the pre-clear binding must not survive the rebuild"
        );
    }

    #[test]
    fn rewind_validates_the_anchor_drops_the_digest_whole_and_sheds_nudges() {
        let mut session = Avatar::for_test("system").unwrap();
        {
            let mut log = session.log.lock();
            for (prompt, answer) in [
                ("one", "answer one"),
                ("two", "answer two"),
                ("three", "answer three"),
            ] {
                log.append_user(prompt.into(), None).unwrap();
                log.append_assistant(ChatMessage::assistant(answer), Vec::new(), None)
                    .unwrap();
            }
        }
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);

        assert_eq!(
            session.rewind(9, &emit).unwrap_err(),
            "exchange 9 is not present in the current view — the last exchange is 3"
        );

        session
            .log
            .lock()
            .apply_edit(
                ContextOp::Fold {
                    through_exchange: 2,
                    digest: "the first two are complete".into(),
                },
                EditAuthority::Harness,
            )
            .unwrap();
        assert_eq!(
            session.rewind(1, &emit).unwrap_err(),
            "exchange 1 is folded into the digest through 2 — name 2 to drop the digest whole, or fold further"
        );

        session.inbox.push(Post::Nudge {
            exchange: 3,
            text: "stale continuation".into(),
        });
        session.rewind(2, &emit).expect("the digest reach is legal");
        let view = session.log.lock().view().clone();
        assert!(view.digest.is_none(), "the digest reach was dropped whole");
        assert!(view.spans.is_empty(), "the rewind removes the whole suffix");
        assert!(
            !matches!(session.inbox.next_item(), Some(Item::Nudge { .. })),
            "a queued nudge for a rewound exchange must not commit"
        );
        // The fold applied above published its own edit record once the first
        // rewind attempt coupled the seam, so the drop is asserted anywhere on
        // the channel rather than at a fixed position.
        assert!(
            crate::bus::drain_records(&rx)
                .into_iter()
                .any(|record| matches!(
                    record,
                    crate::record::Record::Protocol(crate::record::Protocol::ContextEdited {
                        op: ContextOp::Drop { exchanges },
                        by: EditAuthority::User,
                    }) if exchanges == vec![2, 3]
                )),
            "rewind must be durable on the trace"
        );
    }

    /// `assemble` arms the child over the whole scope `fork_session`
    /// snapshotted, sealing parent scratch included as its baseline: a name
    /// the parent leased is never a lease candidate in the child, however
    /// many idle calls it runs.
    #[test]
    fn fork_child_inherited_scratch_is_baseline() {
        let mut session = Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);
        session.run_shell("c0".into(), "let parent_scratch = 1", 5, &emit);

        let mut child = session
            .fork(ral_core::types::Capabilities::default())
            .expect("fork");
        assert!(
            scope_has(&mut child, "parent_scratch"),
            "fork_session snapshots the parent's whole scope"
        );

        // Re-arm with a tiny bound: `assemble` already armed this same scope
        // with the production constant, and re-arming reseals identically,
        // just fast enough to idle out inside a test.
        child
            .seat
            .shell_mut()
            .shell
            .arm_binding_lease(ral_core::types::BindingLease {
                idle_calls: 1,
                large_binding_bytes: u64::MAX,
            });
        let (child_tx, _child_rx) = crate::bus::channel();
        let child_emit = Emitter::new(child_tx, child.agent.id);
        for i in 0..3 {
            child.run_shell(format!("child{i}"), "let _child_spin = 0", 5, &child_emit);
        }
        assert!(
            scope_has(&mut child, "parent_scratch"),
            "inherited parent scratch is baseline in the child — never pruned, however \
             many boundary prunes the idle calls above ran"
        );
    }

    #[test]
    fn resumed_agent_adds_only_the_fresh_shell_note() {
        let dir = tmp("resume-agent");
        let sessions = dir.path().join("sessions");
        let mut log = AgentLog::root(
            &sessions,
            0,
            "old-model",
            &RecordedAccount::for_test("old-provider"),
            0,
        )
        .unwrap();
        log.append_user("before the crash".into(), None).unwrap();
        log.append_assistant(ChatMessage::assistant("saved answer"), vec![], None)
            .unwrap();
        let before: Vec<_> = log.history_transcript().messages().cloned().collect();
        drop(log);

        let scratch =
            Scratch::for_test(crate::bootstrap::EXARCH, "resume-agent").expect("scratch dir");
        let agent = Avatar::root(
            RootConfig {
                system: "system".into(),
                caps: ral_core::types::Capabilities::default(),
                run_dir: dir.path().to_owned(),
                resume: Some(dir.path().to_owned()),
                no_logs: false,
                run_lock: None,
                model: "new-model".into(),
                account: RecordedAccount::for_test("new-provider"),
                allow_schedule: false,
                interactive: true,
                chat: false,
                disk_warn_bytes: None,
                fuel: 0,
                egress: crate::egress::Egress::for_test(),
                dial: None,
            },
            RootSeat::Identity {
                scratch: Arc::new(scratch),
                cwd: std::env::current_dir().expect("test process has a cwd"),
                detach: false,
            },
            scripted("new-model", Script::new()),
        )
        .expect("resumed agent");

        assert!(agent.is_ready());
        let messages = agent.rendered_messages();
        assert_eq!(
            serde_json::to_vec(&messages[..before.len()]).unwrap(),
            serde_json::to_vec(&before).unwrap()
        );
        assert_eq!(messages.len(), before.len() + 1);
        let note = messages.last().expect("fresh-shell note");
        assert_eq!(note.role, genai::chat::ChatRole::User);
        let text = note.content.first_text().expect("note text");
        for loss in [
            "shell is fresh",
            "bindings",
            "workers",
            "cwd",
            "scratch",
            "pinned state",
            "scheduled events",
            "sub-agents",
        ] {
            assert!(text.contains(loss), "resume note must name {loss}: {text}");
        }
        let records =
            crate::record::read_records(&dir.path().join("sessions/0/record.jsonl")).unwrap();
        assert!(records.iter().any(|record| matches!(
            record,
            crate::record::Record::Protocol(crate::record::Protocol::SessionResumed { .. })
        )));
    }

    /// The rotation swaps the segment behind the seam, never the seam itself:
    /// a recorder handed out before the clear, and the bus coupled before it,
    /// both keep publishing afterwards.  Swapping the seam instead left the
    /// frontend dark for the whole first exchange of the cleared session.
    #[test]
    fn clear_rotates_record_jsonl_and_shared_emitters_follow_the_new_segment() {
        let mut session = Avatar::for_test("system").unwrap();
        let record = session.log_dir().join("record.jsonl");
        fs::write(record.with_file_name("record.jsonl.0"), b"reserved").unwrap();
        let (tx, rx) = crate::bus::channel();
        session.couple(&Emitter::new(tx, session.agent.id));
        let recorder = session.recorder();

        session.clear().expect("clear rotation");

        let rotated_record = record.with_file_name("record.jsonl.1");
        assert!(rotated_record.is_file());
        assert!(record.is_file());
        assert!(
            fs::read(record.with_file_name("record.jsonl.0"))
                .unwrap()
                .starts_with(b"reserved")
        );

        let _recorded = recorder
            .emit(crate::record::Display::Prompt {
                text: "after the clear".into(),
            })
            .expect("the pre-clear recorder still appends");
        recorder.transient(crate::record::Transient::Cleared);

        let current_records = crate::record::read_records(&record).unwrap();
        assert!(matches!(
            current_records.first().expect("new session head"),
            crate::record::Record::Protocol(crate::record::Protocol::SessionStarted { .. })
        ));
        assert!(
            current_records.iter().any(|r| matches!(
                r,
                crate::record::Record::Display(crate::record::Display::Prompt { text })
                    if text == "after the clear"
            )),
            "the pre-clear recorder must write into the new segment, not the rotated one"
        );

        let published: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            published.iter().any(|s| matches!(
                s,
                crate::bus::Signal::Fact(_, rec)
                    if matches!(rec.value(), crate::record::Record::Display(
                        crate::record::Display::Prompt { text }) if text == "after the clear")
            )),
            "and the bus coupled before the clear must still see it"
        );
        assert!(
            published.iter().any(|s| matches!(
                s,
                crate::bus::Signal::Transient(_, crate::record::Transient::Cleared)
            )),
            "including the `Cleared` acknowledgement the frontend waits on"
        );
    }

    #[test]
    fn no_logs_is_process_wide_and_never_mints_durable_files() {
        let dir = tmp("no-logs-agent");
        let scratch =
            Scratch::for_test(crate::bootstrap::EXARCH, "no-logs-agent").expect("scratch dir");
        let agent = Avatar::root(
            RootConfig {
                system: "system".into(),
                caps: ral_core::types::Capabilities::default(),
                run_dir: dir.path().to_owned(),
                resume: None,
                no_logs: true,
                run_lock: None,
                model: "test-model".into(),
                account: RecordedAccount::for_test("test"),
                allow_schedule: false,
                interactive: true,
                chat: false,
                disk_warn_bytes: None,
                fuel: 1,
                egress: crate::egress::Egress::for_test(),
                dial: None,
            },
            RootSeat::Identity {
                scratch: Arc::new(scratch),
                cwd: std::env::current_dir().expect("test process has a cwd"),
                detach: false,
            },
            scripted("test-model", Script::new()),
        )
        .expect("mirror-only agent");
        let root_log = agent.log_dir();
        let child = agent
            .fork(ral_core::types::Capabilities::default())
            .expect("mirror-only child");
        let child_log = child.log_dir();
        for log_dir in [&root_log, &child_log] {
            assert!(!log_dir.join("record.jsonl").exists());
        }
        assert!(!dir.path().join("run.lock").exists());
        drop(child);
        drop(agent);
    }

    #[test]
    fn resume_seeds_child_ids_past_existing_session_directories() {
        let dir = tmp("resume-id-seed");
        let sessions = dir.path().join("sessions");
        let log = AgentLog::root(
            &sessions,
            0,
            "old-model",
            &RecordedAccount::for_test("old-provider"),
            0,
        )
        .unwrap();
        fs::create_dir_all(sessions.join("1")).unwrap();
        fs::write(sessions.join("1/sentinel"), b"keep").unwrap();
        drop(log);

        let scratch =
            Scratch::for_test(crate::bootstrap::EXARCH, "resume-id-seed").expect("scratch dir");
        let root = Avatar::root(
            RootConfig {
                system: "system".into(),
                caps: ral_core::types::Capabilities::default(),
                run_dir: dir.path().to_owned(),
                resume: Some(dir.path().to_owned()),
                no_logs: false,
                run_lock: None,
                model: "new-model".into(),
                account: RecordedAccount::for_test("new-provider"),
                allow_schedule: false,
                interactive: true,
                chat: false,
                disk_warn_bytes: None,
                fuel: 1,
                egress: crate::egress::Egress::for_test(),
                dial: None,
            },
            RootSeat::Identity {
                scratch: Arc::new(scratch),
                cwd: std::env::current_dir().expect("test process has a cwd"),
                detach: false,
            },
            scripted("new-model", Script::new()),
        )
        .expect("resumed root");
        let child = root
            .fork(ral_core::types::Capabilities::default())
            .expect("post-resume child");
        let child_id = child
            .log_dir()
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<u64>().ok())
            .expect("numeric child session directory");
        assert!(child_id >= 2);
        assert_eq!(fs::read(sessions.join("1/sentinel")).unwrap(), b"keep");
    }
}
