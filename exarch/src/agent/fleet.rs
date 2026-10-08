//! The fleet: what every node of one run shares — the [`Launch`] fixed once
//! for the whole run, the by-name index a spawn claims its identity at, and
//! the idle lease every reporting child is bounded by.
//!
//! It is not the tree.  The tree is [`Agent::parent`](crate::agent::Agent)
//! and [`Agent::children`](crate::agent::Agent), and every walk over it — the
//! roster, the cancel cascade, the scope check — runs there.  What lives here
//! is what no single node owns: the run's settings, and whether a name is
//! free.
//!
//! A [`Fleet`] is shared behind one `Arc`, held by every agent it enrolled:
//! the trunk and each fork reach the same one, so no node can disagree about
//! what is live or what the run was launched with.  `names` is a lookup index;
//! `roots` holds the trunk and every `/branch` — a root reports to nobody, so
//! a walk over it is how `nearest_reap` reaches every live agent in the
//! run.  Both hold [`Weak`], so an agent leaves the fleet by
//! its avatar being dropped and nothing else, and a lookup prunes.
//!
//! There is one late-settle fence, and it lives on the inbox, not the agent:
//! each session's own `crate::bus::inbox` counts its clears as a
//! clear-epoch, and
//! a `Post` that cannot judge its own staleness — an async agent's result, a
//! detached worker's deferred batch (`InboxDeferred`) — is stamped with that
//! epoch at composition and refused at the inbox's own pop if it has since
//! moved on.  Per inbox, not per fleet — a `/clear` in one tab must not drop
//! work another tab is still waiting on.

use crate::agent::{Agent, Dial};
use crate::enquiry::check_name;
use crate::prompt::BuiltinIndex;
use crate::provider::Bureau;
use crate::provider::Toolset;
use ral_core::process::{self, CancelCause};
use ral_core::sync::LockExt;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// A child's idle lease: the span since its last human exchange (birth is the
/// epoch) before its whole subtree is reaped.
///
/// Only a human message renews the clock, so a never-renewed lease fires
/// exactly this long after birth.
pub const AGENT_LEASE_IDLE: Duration = Duration::from_hours(1);

/// What every node of one run shares, fixed at launch: the host's settings
/// and the services it opened, held once here rather than copied into each
/// [`Agent`].
pub(crate) struct Launch {
    /// Whether a human is attached to this run and types into a conversing
    /// node's inbox — the TUI.  Off, a conversing trunk is driven one
    /// exchange at a time and waits on nothing but its fleet.
    pub attended: bool,
    /// Whether the nodes hold the self-wakeup family.
    pub allow_schedule: bool,
    /// Whether the trunk, refused until a reset past the in-place wait, arms
    /// a wakeup to resume there: exarch's terminal trunk alone.
    pub resume_on_reset: bool,
    /// What a request advertises and dispatch recognises; empty for `--chat`,
    /// which is then never steered.
    pub tools: Toolset,
    /// The operator's disk ceiling; `None` never walks the dirs at all.
    pub disk_warn_bytes: Option<u64>,
    /// How a wire trunk reaches its helpers; `None` on an identity trunk.
    pub dial: Option<Arc<dyn Dial>>,
    /// The one owner of provider construction.
    pub bureau: Arc<Bureau>,
    /// The prompt template, still carrying the builtin-index placeholder, and
    /// the index each node resolves it against for its own grants.
    pub system: Arc<str>,
    pub index: Arc<BuiltinIndex>,
}

#[cfg(test)]
impl Launch {
    /// A launch with nothing granted and nothing to reach: what a fleet-level
    /// test's synthetic agents share.
    pub(crate) fn for_test() -> Self {
        Self {
            attended: false,
            allow_schedule: false,
            resume_on_reset: false,
            tools: Toolset::offered(false),
            disk_warn_bytes: None,
            dial: None,
            bureau: Arc::new(Bureau::Scripted),
            system: Arc::from(""),
            index: BuiltinIndex::resolve(
                ral_core::test_helper::core_shell()
                    .builtin_names()
                    .map(str::to_string)
                    .collect(),
            ),
        }
    }
}

/// The fleet's two indices, each independently locked, plus the launch and
/// the idle bound every node shares.
///
/// [`Self::names`] is touched by every spawn and every name lookup;
/// [`Self::roots`] only by a root's birth and a walk from the top.
pub struct Fleet {
    /// Weak, so an agent leaves by its avatar being dropped and nothing else.
    names: Mutex<HashMap<String, Weak<Agent>>>,
    /// The trunk and every `/branch` — a root reports to nobody, so this is
    /// where a walk over the whole run starts.
    roots: Mutex<Vec<Weak<Agent>>>,
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
    fn prune_names(names: &mut HashMap<String, Weak<Agent>>) {
        names.retain(|_, weak| weak.strong_count() > 0);
    }

    /// Bring a newborn `agent` into the fleet: claim its name, hang it under
    /// its parent or push it onto [`Self::roots`], and arm the lease a
    /// reporting child is bounded by.  The one door — construction is the
    /// only place an agent joins, and every refusal is decided here.
    ///
    /// # Errors
    /// [`Unborn::SessionDead`] when the parent is already terminated — a spawn
    /// racing a cancel on its own parent.  Checked under the very lock a racing
    /// spawn's own claim takes.
    ///
    /// [`Unborn::NameTaken`] when another live agent bears the name.  Also
    /// under that lock, so two same-name spawns racing within one turn cannot
    /// both succeed — the authoritative half of the spawn-uniqueness rule,
    /// [`Self::name_live`] the cheap one.
    ///
    /// [`Unborn::NameMalformed`] when the name breaks [`check_name`].  Checked
    /// here and not only at the doors, because a wire peer is not trusted to
    /// have used one.
    pub(crate) fn enrol(self: &Arc<Self>, agent: &Arc<Agent>) -> Result<(), Unborn> {
        if let Err(why) = check_name(&agent.name) {
            return Err(Unborn::NameMalformed(why));
        }
        let mut names = self.names.lock_ignore_poison();
        Self::prune_names(&mut names);
        if let Some(parent) = &agent.parent
            && parent.token.terminated()
        {
            return Err(Unborn::SessionDead);
        }
        if names.contains_key(&agent.name) {
            return Err(Unborn::NameTaken(agent.name.clone()));
        }
        names.insert(agent.name.clone(), Arc::downgrade(agent));
        drop(names);
        // Adopted (or rooted) only after the name is claimed: a refused agent
        // must never appear in the fleet, however briefly.
        match &agent.parent {
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
    fn roots(&self) -> Vec<Arc<Agent>> {
        let mut roots = self.roots.lock_ignore_poison();
        let live: Vec<Arc<Agent>> = roots.iter().filter_map(Weak::upgrade).collect();
        if live.len() != roots.len() {
            *roots = live.iter().map(Arc::downgrade).collect();
        }
        live
    }

    /// At most one agent can match, since [`Self::enrol`] keeps names unique
    /// among the live.  The `` `cancel ``, `` `message ``, and `` `read `` tags
    /// all come through here before the scope climb.
    pub fn resolve(&self, name: &str) -> Option<Arc<Agent>> {
        let mut names = self.names.lock_ignore_poison();
        Self::prune_names(&mut names);
        names.get(name).and_then(Weak::upgrade)
    }

    /// The cheap half of the spawn-uniqueness rule [`Self::enrol`] enforces
    /// authoritatively: `` exarch-agents `start `` reads this before forking a nursery
    /// session, so the ordinary duplicate refuses without a fork to unwind.
    pub fn name_live(&self, name: &str) -> bool {
        self.resolve(name).is_some()
    }

    /// Whether any root — trunk or `/branch` — is still live.
    pub fn is_empty(&self) -> bool {
        self.roots().is_empty()
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
                 to settle"
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
