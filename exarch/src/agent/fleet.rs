//! The fleet: what every node of one run shares — the [`Launch`] fixed once
//! for the whole run, the by-name index a spawn claims its identity at, and
//! the idle lease every reporting child is bounded by.
//!
//! It is not the tree.  The tree is [`Role::Child`](crate::agent::Role)
//! and [`Agent::children`](crate::agent::Agent), and every walk over it — the
//! roster, the cancel cascade, the scope check — runs there.  What lives here
//! is what no single node owns: the run's settings, and whether a name is
//! free.
//!
//! A [`Fleet`] is shared behind one `Arc`, held by every agent it enrolled:
//! every root and fork reaches the same one, so no node can disagree about
//! what is live or what the run was launched with.  `names` is a lookup index;
//! `roots` holds every session — a root reports to nobody, so a walk over it
//! is how `nearest_reap` reaches every live agent in the run.  Both hold
//! [`Weak`], so an agent leaves the fleet by its avatar being dropped and
//! nothing else, and a lookup prunes.
//!
//! There is one late-settle fence, and it lives on the inbox, not the agent:
//! each session's own `crate::bus::inbox` counts its clears as a
//! clear-epoch, and
//! a `Post` that cannot judge its own staleness — an async agent's result, a
//! worker's deferred batch (`InboxDeferred`) — is stamped with that
//! epoch at composition and refused at the inbox's own pop if it has since
//! moved on.  Per inbox, not per fleet — a `/clear` in one tab must not drop
//! work another tab is still waiting on.

use crate::agent::index::{BuiltinIndex, Grants};
use crate::agent::seat::Seat;
use crate::agent::{Agent, EngineLost, Factory, Role, Trunk};
use crate::app::Scratch;
use crate::enquiry::check_name;
use crate::provider::Bureau;
use crate::provider::Toolset;
use ral_core::capability::GrantStack;
use ral_core::carrier::Severed;
use ral_core::engine::EngineInstaller;
use ral_core::process::{self, CancelCause};
use ral_core::sync::{CondvarExt, LockExt};
use ral_core::terminal::TerminalState;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

/// A child's idle lease: the span since its last human exchange (birth is the
/// epoch) before its whole subtree is reaped.
///
/// Only a human message renews the clock, so a never-renewed lease fires
/// exactly this long after birth.
pub const AGENT_LEASE_IDLE: Duration = Duration::from_hours(1);

/// What every node of one run shares, fixed at launch: the recipe for a root,
/// and the services the host opened, held once here rather than copied into
/// each [`Agent`].
pub(crate) struct Launch {
    /// How every root is driven.  Attended, a conversing root is typed into
    /// by a human and parks between messages; otherwise it is driven one
    /// exchange at a time and waits on nothing but its fleet.
    pub trunk: Trunk,
    /// Whether the nodes hold the self-wakeup family.
    pub allow_schedule: bool,
    /// Whether a root, refused until a reset past the in-place wait, arms a
    /// wakeup to resume there: exarch's terminal roots alone.
    pub resume_on_reset: bool,
    /// What a request advertises and dispatch recognises; empty for `--chat`,
    /// which is then never steered.
    pub tools: Toolset,
    /// The operator's disk ceiling; `None` never walks the dirs at all.
    pub disk_warn_bytes: Option<u64>,
    /// Where a wire root gets its helper engines; `None` on an identity root.
    pub factory: Option<Arc<dyn Factory>>,
    /// The one owner of provider construction.
    pub bureau: Arc<Bureau>,
    /// The prompt template, still carrying the builtin-index placeholder, and
    /// the index each node resolves it against for its own grants.
    pub system: Arc<str>,
    pub index: Arc<BuiltinIndex>,
    /// The invocation stack every node runs under.
    pub caps: GrantStack,
    /// Every root's fuel: fuel is depth, and a root is depth 0.
    pub fuel: u32,
    /// The egress verdict on hosted web search.
    pub search: bool,
    /// Where `sessions/<id>/` hang.
    pub sessions_root: PathBuf,
    pub genesis: Option<Genesis>,
}

/// What a fresh root's engine boots from; `None` on a wire host, which boots none.
pub(crate) struct Genesis {
    pub(crate) installers: &'static [EngineInstaller],
    pub(crate) cwd: PathBuf,
    pub(crate) terminal: TerminalState,
    /// The run's, and outliving every `/clear` and every root.
    pub(crate) scratch: Arc<Scratch>,
}

impl Genesis {
    /// A fresh identity engine for a root.
    ///
    /// # Errors
    /// The recipe's refusal.
    pub(crate) fn seat(&self) -> Result<Seat, Severed> {
        Seat::root(
            self.installers,
            self.cwd.clone(),
            self.terminal,
            self.scratch.clone(),
        )
    }
}

impl Launch {
    /// A fresh root's seat.
    ///
    /// # Errors
    /// A host that boots no further engines, or the recipe's refusal.
    pub(crate) fn seat(&self) -> io::Result<Seat> {
        let genesis = self.genesis.as_ref().ok_or_else(|| {
            io::Error::other(
                "this host boots no further engines, so it cannot open another session",
            )
        })?;
        genesis.seat().map_err(|lost| {
            io::Error::other(EngineLost::starting(&lost, self.sessions_root.parent()))
        })
    }

    /// The system prompt a node on `seat` reads, resolved against its own
    /// grants.  A chat launch advertises no tool, so its prompt takes no
    /// builtin surface at all: no index, no taught section, the bare stand-in
    /// verbatim.
    ///
    /// # Errors
    /// The seat's engine is severed.
    pub(crate) fn prompt_for(
        &self,
        seat: &Seat,
        caps: &GrantStack,
        role: &Role,
        fuel: u32,
        name: &str,
    ) -> Result<String, Severed> {
        let own = crate::prompt::with_grant(&self.system, &seat.read(|t| t.grants())?, caps);
        Ok(if self.tools.is_empty() {
            own
        } else {
            self.index.apply(
                &own,
                &Grants {
                    role,
                    allow_schedule: self.allow_schedule,
                    spawns: fuel > 0,
                },
                name,
            )
        })
    }
}

#[cfg(test)]
impl Launch {
    /// A launch with nothing granted and nothing to reach: what a fleet-level
    /// test's synthetic agents share.
    pub(crate) fn for_test() -> Self {
        Self {
            trunk: Trunk::Headless,
            allow_schedule: false,
            resume_on_reset: false,
            tools: Toolset::offered(false),
            disk_warn_bytes: None,
            factory: None,
            bureau: Arc::new(Bureau::Scripted),
            system: Arc::from(""),
            index: BuiltinIndex::resolve(
                ral_core::test_helper::core_shell()
                    .builtin_names()
                    .map(str::to_string)
                    .collect(),
            ),
            caps: GrantStack::root(),
            fuel: 0,
            search: false,
            sessions_root: PathBuf::new(),
            genesis: None,
        }
    }
}

/// A name's standing in [`Fleet::names`]: spoken for by a [`Claim`] whose
/// agent is still being built, or borne by one that is born.
enum Slot {
    Claimed,
    Live(Weak<Agent>),
}

impl Slot {
    fn agent(&self) -> Option<Arc<Agent>> {
        match self {
            Self::Claimed => None,
            Self::Live(weak) => weak.upgrade(),
        }
    }

    /// Whether the name is free: only a settled bearer frees it.
    fn vacated(&self) -> bool {
        matches!(self, Self::Live(weak) if weak.strong_count() == 0)
    }
}

/// A name held for an agent not yet built, so the name rules are settled
/// before any log exists for it.  [`Fleet::enrol`] consumes it; one dropped
/// unconsumed gives its name back.
#[must_use = "a claim only holds its name until it is dropped"]
pub(crate) struct Claim {
    fleet: Arc<Fleet>,
    name: String,
}

impl Claim {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut names = self.fleet.names.lock_ignore_poison();
        if matches!(names.get(&self.name), Some(Slot::Claimed)) {
            names.remove(&self.name);
        }
    }
}

/// The fleet's two indices, each independently locked, plus the launch and
/// the idle bound every node shares.
///
/// [`Self::names`] is touched by every spawn and every name lookup;
/// [`Self::roots`] only by a root's birth and a walk from the top.
pub struct Fleet {
    /// Each name's [`Slot`]; a live one is `Weak`, so an agent leaves by its
    /// avatar being dropped and nothing else.
    names: Mutex<HashMap<String, Slot>>,
    /// Every root: one reports to nobody, so this is where a walk over the
    /// whole run starts.
    roots: Mutex<Vec<Weak<Agent>>>,
    /// Held by [`Self::departed`] and [`Self::wait_empty`] alone.
    exits: Mutex<()>,
    departures: Condvar,
    /// [`AGENT_LEASE_IDLE`] outside tests.
    lease: Duration,
    pub(crate) launch: Launch,
}

impl Fleet {
    /// A fleet for one run, whose reporting children are reaped `lease` after
    /// their last exchange.
    pub(crate) fn new(launch: Launch, lease: Duration) -> Arc<Self> {
        Arc::new(Self {
            names: Mutex::new(HashMap::new()),
            roots: Mutex::new(Vec::new()),
            exits: Mutex::new(()),
            departures: Condvar::new(),
            lease,
            launch,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Arc<Self> {
        Self::new(Launch::for_test(), AGENT_LEASE_IDLE)
    }

    /// A test fleet whose idle bound is `lease`, so a test can watch a reap
    /// instead of waiting out [`AGENT_LEASE_IDLE`].
    #[cfg(test)]
    pub(crate) fn leased_for_test(lease: Duration) -> Arc<Self> {
        Self::new(Launch::for_test(), lease)
    }

    /// Drop what has settled from `names`, so a name is free the moment its
    /// bearer's avatar goes.
    fn prune_names(names: &mut HashMap<String, Slot>) {
        names.retain(|_, slot| !slot.vacated());
    }

    /// Speak for `name` ahead of the agent that will bear it, so a refused
    /// name leaves no session directory behind and two same-name spawns racing
    /// within one turn cannot both succeed.
    ///
    /// # Errors
    /// [`Unborn::NameMalformed`] when the name breaks [`check_name`].
    ///
    /// [`Unborn::NameTaken`] when a live agent, or another claim, holds it.
    pub(crate) fn claim(self: &Arc<Self>, name: &str) -> Result<Claim, Unborn> {
        check_name(name).map_err(Unborn::NameMalformed)?;
        let mut names = self.names.lock_ignore_poison();
        Self::prune_names(&mut names);
        if names.contains_key(name) {
            return Err(Unborn::NameTaken(name.to_string()));
        }
        names.insert(name.to_string(), Slot::Claimed);
        drop(names);
        Ok(Claim {
            fleet: Arc::clone(self),
            name: name.to_string(),
        })
    }

    /// Bring a newborn `agent` into the fleet under the name `claim` holds:
    /// hang it under its parent or push it onto [`Self::roots`], and arm the
    /// lease a reporting child is bounded by.  The one door — construction is
    /// the only place an agent joins, and no agent can join under a name no
    /// claim holds.
    ///
    /// # Errors
    /// [`Unborn::SessionDead`] when the parent is already terminated — a spawn
    /// racing a cancel on its own parent.  The claim is then given back.
    pub(crate) fn enrol(self: &Arc<Self>, agent: &Arc<Agent>, claim: Claim) -> Result<(), Unborn> {
        let mut names = self.names.lock_ignore_poison();
        if let Some(parent) = agent.role.parent()
            && parent.token.terminated()
        {
            return Err(Unborn::SessionDead);
        }
        names.insert(claim.name.clone(), Slot::Live(Arc::downgrade(agent)));
        // The guard goes first: the claim's drop locks `names` again.
        drop((names, claim));
        // Adopted (or rooted) only after the name is live: a refused agent
        // must never appear in the fleet, however briefly.
        match agent.role.parent() {
            Some(parent) => {
                parent.adopt(agent);
                arm_lease(self, agent, self.lease);
            }
            None => self.roots.lock_ignore_poison().push(Arc::downgrade(agent)),
        }
        Ok(())
    }

    /// This fleet's live roots, pruning the settled ones as it snapshots —
    /// the tree-walk twin of [`Agent::children`].
    pub(crate) fn roots(&self) -> Vec<Arc<Agent>> {
        let mut roots = self.roots.lock_ignore_poison();
        let live: Vec<Arc<Agent>> = roots.iter().filter_map(Weak::upgrade).collect();
        if live.len() != roots.len() {
            *roots = live.iter().map(Arc::downgrade).collect();
        }
        live
    }

    /// At most one agent can match, since [`Self::claim`] keeps names unique
    /// among the live.  The `` `cancel ``, `` `message ``, and `` `read `` tags
    /// all come through here before the scope climb.
    pub fn resolve(&self, name: &str) -> Option<Arc<Agent>> {
        let mut names = self.names.lock_ignore_poison();
        Self::prune_names(&mut names);
        names.get(name).and_then(Slot::agent)
    }

    /// Whether any root is still live.
    pub fn is_empty(&self) -> bool {
        self.roots().is_empty()
    }

    /// Wake every [`Self::wait_empty`], once an avatar has fully dropped.
    /// Taking the mutex after the drop is what rules out a lost wakeup: a
    /// waiter is either before its emptiness check, and sees the drop, or
    /// already parked, and is woken.
    pub(crate) fn departed(&self) {
        drop(self.exits.lock_ignore_poison());
        self.departures.notify_all();
    }

    /// Block until no root is live.
    pub fn wait_empty(&self) {
        let exits = self.exits.lock_ignore_poison();
        drop(
            self.departures
                .wait_while_ignore_poison(exits, |()| !self.is_empty()),
        );
    }

    /// The nearest time-to-reap across the leased agents, `None` when none is
    /// leased.  A pure survey for `/resources`: it renews nothing.
    pub fn nearest_reap(&self) -> Option<Duration> {
        let lease = self.lease;
        self.roots()
            .iter()
            .flat_map(|root| root.walk())
            .map(|live| lease.saturating_sub(live.idle()))
            .min()
    }
}

/// Why an agent could not be born.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unborn {
    /// The parent was terminated out from under a racing spawn.
    SessionDead,
    /// Carries the offending name, so the caller can refuse didactically.
    NameTaken(String),
    /// Carries [`check_name`]'s own sentence, so every door refuses in the
    /// same words.
    NameMalformed(String),
}

impl fmt::Display for Unborn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionDead => write!(f, "this session is no longer live"),
            Self::NameTaken(name) => write!(
                f,
                "a live agent already bears the name '{name}': pick another, or wait for it \
                 to settle. Names identify live agents; agents lists yours."
            ),
            Self::NameMalformed(why) => write!(f, "{why}"),
        }
    }
}

/// Arm one link of `agent`'s idle-lease chain, to fire `after`.
fn arm_lease(fleet: &Arc<Fleet>, agent: &Arc<Agent>, after: Duration) {
    let fleet = Arc::clone(fleet);
    let agent = Arc::downgrade(agent);
    process::arm_callback(after, move || lease_fire(&fleet, &agent)).keep();
}

/// One firing of an agent's idle lease, on the reaper daemon thread.  A
/// settled agent — one whose avatar has gone — ends the chain silently, and one
/// still short of its bound re-arms for the remaining margin: only a steer or a
/// peer message ever touches the exchange clock this reads, and neither reaches
/// the reaper, so this lazy re-arm is the whole mechanism.
fn lease_fire(fleet: &Arc<Fleet>, agent: &Weak<Agent>) {
    let Some(agent) = agent.upgrade() else {
        return;
    };
    let ttl = fleet.lease;
    let idle = agent.idle();
    match ttl.checked_sub(idle) {
        Some(margin) if !margin.is_zero() => arm_lease(fleet, &agent, margin),
        _ => agent.cancel_tree(CancelCause::TimedOut),
    }
}

#[cfg(test)]
mod tests;
