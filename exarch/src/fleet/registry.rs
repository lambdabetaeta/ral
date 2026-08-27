//! The fleet's index: the by-name door a spawn claims its identity at, the
//! by-id door a frontend command arrives through, and the idle lease every
//! reporting child is bounded by.
//!
//! It is not the tree.  The tree is [`Agent::parent`] and `Agent::children`,
//! and every walk over it — the roster, the cancel cascade, the scope check —
//! runs there.  What lives here is what only the fleet can answer: whether a
//! name is free, and where an id resolves.  Both indexes hold [`Weak`], so an
//! agent leaves them by its avatar being dropped, and a lookup prunes.
//!
//! Two cancel motions run over that tree and must not be confused.  *Terminate*
//! ([`Agent::cancel_tree`]) ends an agent and cascades to its subtree, tripping
//! both the cooperative [`Token`](crate::agent::cancel::Token) its attend loop
//! polls between steps and its session's durable root, so an in-flight `ral`
//! eval unwinds at ral's poll points rather than grinding to its timeout wall —
//! and, a worker's cancel scope being a descendant of that root, reaching the
//! agent's detached workers with no edge of its own.  *Interrupt*
//! ([`Agent::interrupt`]) cancels only the scope of the dispatch in flight, so
//! the next one, minted fresh from the untouched root, is clean.
//!
//! Each agent carries a generation counter, bumped by its own `/clear`, and
//! that is the fleet's late-settle fence: an async agent's result
//! (`Avatar::admits`) and a detached worker's deferred batch (`InboxDeferred`)
//! both admit against the generation of the session that will *consume* them.
//! Per session, not per fleet — a `/clear` in one tab must not drop work
//! another tab is still waiting on.

use crate::agent::Agent;
use crate::bus::AgentId;
use crate::sync::LockExt;
use ral_core::process::{self, CancelCause, DurableRoot, ForegroundScope};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

/// Where an interrupt lands: a live agent's current-dispatch scope, republished
/// by its transport as each dispatch is minted, into the target the host
/// installed once with
/// [`ral_core::transport::IdentityTransport::set_interrupt_target`].  It sits
/// outside the engine lock the run itself holds, so an interrupt can reach the
/// in-flight run from another thread — and it is published *ahead* of that
/// lock, so an interrupt raised while a dispatch still waits on it lands on
/// the scope that dispatch minted: the run's frame is born a descendant and
/// observes a cancel recorded before it existed.  `None` until the first
/// dispatch.
pub(crate) type InterruptTarget = Arc<Mutex<Option<ForegroundScope>>>;

/// How an agent reaches its own running eval, per seat kind — the eval-layer
/// half of every cancel path, beside the cooperative
/// [`Token`](crate::agent::cancel::Token) [`Agent`] also carries.
pub(crate) enum EvalReach {
    /// The in-process reach: the agent's own shell's durable root (terminate,
    /// and through the cancel-scope ancestor chain its detached workers too),
    /// and the interrupt target its transport refreshes each dispatch
    /// (interrupt).  `eval_root` is `None` for the trunk's interrupt-only
    /// reach — see [`Self::interrupt_only`].
    Identity {
        eval_root: Option<DurableRoot>,
        interrupt_target: InterruptTarget,
    },
    /// The wire reach: the engine lives in another process, whose only
    /// host-reachable cancel primitive is a `Cancel` control frame against
    /// the in-flight dispatch, so both motions collapse into it — a wire
    /// trunk (synod's guest-VM engine) carries this reach directly, and only
    /// an identity seat can be forked into, so no descendant of one ever
    /// does.
    Wire(ral_core::transport::ControlSender),
}

impl EvalReach {
    /// Unwind the in-flight run without ending the agent.  Never touches the
    /// durable root, so the next run is born uncancelled.
    pub(crate) fn interrupt(&self) {
        match self {
            Self::Identity {
                interrupt_target, ..
            } => {
                if let Some(scope) = interrupt_target.lock_ignore_poison().as_ref() {
                    scope.cancel(CancelCause::Interrupt);
                }
            }
            Self::Wire(ctrl) => ctrl.cancel_in_flight(),
        }
    }

    /// End the agent's eval by cancelling its session's durable root.  That
    /// poisons the root permanently, by design: every caller is ending the
    /// agent, so there is no later run to break.  A no-op when `eval_root` is
    /// absent — the trunk's interrupt-only reach.
    pub(crate) fn terminate(&self, cause: CancelCause) {
        match self {
            Self::Identity { eval_root, .. } => {
                if let Some(root) = eval_root {
                    root.cancel(cause);
                }
            }
            Self::Wire(ctrl) => ctrl.cancel_in_flight(),
        }
    }

    /// Weaken a reach so its terminate side can no longer poison a session:
    /// a seat rebuilt in place under a standing agent — the trunk's — must
    /// never carry a `DurableRoot`, for two reasons.  [`Agent::cancel`] fires
    /// on every agent a cascade visits, such a seat's included, and its root
    /// would turn that into a permanent kill of the whole session rather than
    /// a subtree reap.  And the rebuild mints a fresh root under a standing
    /// agent, so a root captured then would go stale — the interrupt target is
    /// host-owned precisely so it survives that rebuild, and a root has no such
    /// target.  Because this is stated once, at construction, and never touched
    /// again, the rebuild has nothing to keep in step with it.
    ///
    /// A wire reach passes through: its only primitive is `Control::Cancel`
    /// against the in-flight dispatch, which is not permanent, so it carries
    /// nothing to weaken.
    pub(crate) fn interrupt_only(self) -> Self {
        match self {
            Self::Identity {
                interrupt_target, ..
            } => Self::Identity {
                eval_root: None,
                interrupt_target,
            },
            wire @ Self::Wire(_) => wire,
        }
    }
}

/// A child's idle lease: the span since its last human exchange (birth is the
/// epoch) before its whole subtree is reaped.
///
/// Only a human message renews the clock, so a never-renewed lease fires
/// exactly this long after birth.
pub const AGENT_LEASE_IDLE: Duration = Duration::from_hours(1);

/// How long a leased child may sit idle-and-parked before the frontend demotes
/// its tab out of the TAB cycle.  The registry owns the clock this measures
/// against but takes no action of its own at the mark.
pub const AGENT_DEMOTE_IDLE: Duration = Duration::from_mins(5);

/// What a listed agent is doing, as a roster row states it.  Derived at
/// [`listing`] time from an agent's rest and deposit and from its own children;
/// nothing stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RosterState {
    /// Working: not parked for a message.
    Busy,
    /// Working only in the sense of holding for a busy child of its own.
    WaitingOnAgents,
    /// Parked holding a reply the parent has yet to fetch.
    Replied,
    /// Parked with no reply — a child a human is talking to.
    Waiting,
}

impl RosterState {
    /// The variant label the roster row carries.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::WaitingOnAgents => "waiting-on-agents",
            Self::Replied => "replied",
            Self::Waiting => "waiting",
        }
    }
}

/// One live agent, for the `agents` listing.
pub struct AgentInfo {
    pub name: String,
    pub log_dir: PathBuf,
    pub elapsed: Duration,
    pub state: RosterState,
    /// How long it has been parked; zero while busy.
    pub idle: Duration,
}

impl AgentInfo {
    fn of(agent: &Agent) -> Self {
        let rest = agent.rest();
        let state = match (rest, agent.has_reply()) {
            (None, _) if agent.has_busy_children() => RosterState::WaitingOnAgents,
            (None, _) => RosterState::Busy,
            (Some(_), true) => RosterState::Replied,
            (Some(_), false) => RosterState::Waiting,
        };
        Self {
            name: agent.name().to_string(),
            log_dir: agent.log_dir().to_path_buf(),
            elapsed: agent.elapsed(),
            state,
            idle: rest.map_or(Duration::ZERO, |at| at.elapsed()),
        }
    }
}

/// The tab-bar contract every agent name must meet.
///
/// Non-empty, at most 24 characters, ASCII alphanumeric plus `-`/`_`, since the
/// tab bar lays a name out as a single token.  Stated here because this is where
/// a name becomes identity — [`AgentRegistry::enrol`] refuses whatever this
/// refuses, so no door can mint an agent around it, however a name reached that
/// door.
///
/// # Errors
/// The sentence to show, naming the rule and the name that broke it.
pub fn check_name(name: &str) -> Result<(), String> {
    if !name.is_empty()
        && name.len() <= 24
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Ok(());
    }
    Err(format!(
        "`name` must be non-empty, at most 24 characters, and only ASCII letters, digits, `-`, \
         or `_` (the tab-bar contract) — got {name:?}"
    ))
}


/// The fleet's index over its live agents.
///
/// Every clone shares the one pair of doors, so the trunk, each forked child,
/// and the frontend hold the same fleet — and a detached worker keeps a handle
/// it can resolve through after its exchange has ended.
#[derive(Clone)]
pub struct AgentRegistry {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// Weak, so an agent leaves by its avatar being dropped and nothing else.
    live: HashMap<AgentId, Weak<Agent>>,
    /// The idle bound every reporting child is reaped at.  [`AGENT_LEASE_IDLE`]
    /// outside tests; each fire re-reads it, so nothing caches a copy.
    lease: Duration,
}

impl Inner {
    /// Drop what has settled.  Every door prunes before it answers, so a name
    /// is free the moment its bearer's avatar goes.
    fn prune(&mut self) {
        self.live.retain(|_, weak| weak.strong_count() > 0);
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                live: HashMap::new(),
                lease: AGENT_LEASE_IDLE,
            })),
        }
    }

    /// Shorten the idle bound so a test can watch a reap instead of waiting out
    /// [`AGENT_LEASE_IDLE`].  Shared by every clone, like the rest of the fleet.
    #[cfg(test)]
    pub fn set_lease(&self, bound: Duration) {
        self.lock().lease = bound;
    }

    /// Bring a newborn `agent` into the fleet: claim its name, hang it under
    /// its parent, and arm the lease a reporting child is bounded by.  The one
    /// door — construction is the only place an agent joins, and every refusal
    /// is decided here.
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
    pub(crate) fn enrol(&self, agent: &Arc<Agent>) -> Result<(), Unborn> {
        if let Err(why) = check_name(agent.name()) {
            return Err(Unborn::NameMalformed(why));
        }
        let mut g = self.lock();
        g.prune();
        if let Some(parent) = agent.parent()
            && parent.cancel_token().terminated()
        {
            return Err(Unborn::SessionDead);
        }
        if g.live
            .values()
            .filter_map(Weak::upgrade)
            .any(|live| live.name() == agent.name())
        {
            return Err(Unborn::NameTaken(agent.name().to_string()));
        }
        g.live.insert(agent.id, Arc::downgrade(agent));
        drop(g);
        // Adopted only after the name is claimed: a refused agent must never
        // appear in its parent's subtree, however briefly.
        if let Some(parent) = agent.parent() {
            parent.adopt(agent);
            arm_lease(self, agent, self.lease());
        }
        Ok(())
    }

    /// The live agent behind `id`, or `None` once its avatar has gone — how a
    /// frontend command, which travels as an id, reaches its target.
    pub fn by_id(&self, id: AgentId) -> Option<Arc<Agent>> {
        let mut g = self.lock();
        g.prune();
        g.live.get(&id).and_then(Weak::upgrade)
    }

    /// At most one agent can match, since [`Self::enrol`] keeps names unique
    /// among the live.  The `` `cancel ``, `` `message ``, and `` `read `` tags
    /// all come through here before the scope climb.
    pub fn resolve(&self, name: &str) -> Option<Arc<Agent>> {
        let mut g = self.lock();
        g.prune();
        g.live
            .values()
            .filter_map(Weak::upgrade)
            .find(|live| live.name() == name)
    }

    /// The cheap half of the spawn-uniqueness rule [`Self::enrol`] enforces
    /// authoritatively: `` agents `start `` reads this before forking a nursery
    /// session, so the ordinary duplicate refuses without a fork to unwind.
    pub fn name_live(&self, name: &str) -> bool {
        self.resolve(name).is_some()
    }

    /// The nearest time-to-reap across the leased agents, `None` when none is
    /// leased.  A pure survey for `/resources`: it renews nothing.
    pub fn nearest_reap(&self) -> Option<Duration> {
        let lease = self.lease();
        let mut g = self.lock();
        g.prune();
        g.live
            .values()
            .filter_map(Weak::upgrade)
            .filter(|live| live.parent().is_some())
            .map(|live| lease.saturating_sub(live.idle()))
            .min()
    }

    fn lease(&self) -> Duration {
        self.lock().lease
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock_ignore_poison()
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
                "a live agent already bears the name '{name}' — pick another, or wait for it \
                 to settle"
            ),
            Self::NameMalformed(why) => write!(f, "{why}"),
        }
    }
}

/// The `agents` listing: `ancestor`'s live proper descendants at any depth,
/// ordered by id.  An agent lists what it spawned, never itself.
pub(crate) fn listing(ancestor: &Agent) -> Vec<AgentInfo> {
    let mut live = ancestor.walk();
    live.sort_unstable_by_key(|node| node.id);
    live.iter().map(|node| AgentInfo::of(node)).collect()
}

/// Arm one link of `agent`'s idle-lease chain, to fire `after`.
fn arm_lease(registry: &AgentRegistry, agent: &Arc<Agent>, after: Duration) {
    let registry = registry.clone();
    let agent = Arc::downgrade(agent);
    process::arm_callback(after, move || lease_fire(&registry, &agent)).keep();
}

/// One firing of an agent's idle lease, on the reaper daemon thread.  A
/// settled agent — one whose avatar has gone — ends the chain silently, and one
/// still short of its bound re-arms for the remaining margin: only a steer or a
/// peer message ever touches the exchange clock this reads, and neither reaches
/// the reaper, so this lazy re-arm is the whole mechanism.
fn lease_fire(registry: &AgentRegistry, agent: &Weak<Agent>) {
    let Some(agent) = agent.upgrade() else {
        return;
    };
    let ttl = registry.lease();
    let idle = agent.idle();
    match ttl.checked_sub(idle) {
        Some(margin) if !margin.is_zero() => arm_lease(registry, &agent, margin),
        _ => agent.cancel_tree(CancelCause::Deadline),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::cancel::Token;
    use crate::agent::testkit::{TestAgentSpec, test_agent};
    use crate::bus::{AgentOutcome, AgentResult, Inbox, Item, ParkMode, Post};
    use std::time::Instant;

    /// The reaper fires on its own daemon thread, so a lease test asserts
    /// against wall time rather than a synchronous call.
    fn eventually(timeout: Duration, pred: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// A root (`parent` `None`) or a reporting child, with handles no test
    /// below inspects.  The caller holds the only strong reference: dropping
    /// it is what settles the agent.
    fn agent(fleet: &AgentRegistry, name: &str, parent: Option<&Arc<Agent>>) -> Arc<Agent> {
        born(fleet, spec(name, parent)).expect("a fresh name under a live parent is always born")
    }

    fn spec(name: &str, parent: Option<&Arc<Agent>>) -> TestAgentSpec {
        let mut spec = TestAgentSpec::new(name);
        spec.parent = parent.cloned();
        spec
    }

    fn born(fleet: &AgentRegistry, spec: TestAgentSpec) -> Result<Arc<Agent>, Unborn> {
        test_agent(fleet, spec)
    }

    /// The in-process reach, over an eval root the test goes on to assert on.
    fn reach_into(root: &DurableRoot) -> EvalReach {
        EvalReach::Identity {
            eval_root: Some(root.clone()),
            interrupt_target: InterruptTarget::default(),
        }
    }

    /// A lease is a consequence of having a reporting parent, and of nothing
    /// else.  Asserted behaviourally, since there is no guard to peek at: the
    /// reap only stamps the cancel layers.
    #[test]
    fn a_lease_is_armed_only_for_a_reporting_child() {
        let fleet = AgentRegistry::new();
        fleet.set_lease(Duration::from_millis(50));
        let trunk = agent(&fleet, "trunk", None);
        let branch = agent(&fleet, "branch", None);
        let worker_root = DurableRoot::default();
        let mut worker = spec("worker", Some(&trunk));
        worker.reach = reach_into(&worker_root);
        let _worker = born(&fleet, worker).expect("a fresh child of a live trunk");

        assert!(
            eventually(Duration::from_secs(2), || worker_root
                .as_scope()
                .is_cancelled()),
            "a reporting child is reaped once the bound elapses"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !branch.cancel_token().is_cancelled(),
            "a root arms no reaper deadline"
        );
    }

    /// Both layers, so an abandoned child's in-flight eval unwinds instead of
    /// grinding on as an orphan whose result nobody will collect.
    #[test]
    fn clear_subtree_bumps_the_generation_and_cancels_the_subtree() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let child_root = DurableRoot::default();
        let mut child = spec("child", Some(&trunk));
        child.reach = reach_into(&child_root);
        let child = born(&fleet, child).expect("a fresh child of a live trunk");
        let born_under = child.consumer();

        trunk.clear_subtree();

        assert_ne!(
            born_under,
            trunk.generation(),
            "the generation advanced on clear, so a result from before it is rejected"
        );
        assert!(
            child.cancel_token().terminated(),
            "the abandoned child's token is terminate-stamped"
        );
        assert!(
            child_root.as_scope().is_cancelled(),
            "the reap cancels the child's eval layer, not just its token"
        );
    }

    /// The fence is per session, so a `/clear` on one root must leave another
    /// root's in-flight result alone.  `/clear` in a `/branch` tab reaches this
    /// through `Avatar::admits`, which reads the *consuming* session's counter.
    #[test]
    fn a_clear_on_one_root_does_not_reject_another_roots_late_result() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let branch = agent(&fleet, "branch", None);
        let worker = agent(&fleet, "worker", Some(&trunk));

        branch.clear_subtree();

        assert_eq!(
            worker.consumer(),
            trunk.generation(),
            "the branch's /clear must not poison the trunk's outstanding result"
        );
    }

    /// The invariant the `/clear` UI handler leans on.  A sticky [`Token`]
    /// never forgets a terminate-class cause, so no gesture that leaves the
    /// trunk live may stamp one on the trunk's own token; the inclusive
    /// [`Agent::cancel_tree`] would, and every later run would fail forever.
    #[test]
    fn the_clear_gesture_cancels_descendants_but_spares_the_trunks_token() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let child = agent(&fleet, "child", Some(&trunk));

        trunk.cancel_descendants(CancelCause::Explicit);

        assert!(
            child.cancel_token().terminated(),
            "the descendant's token is terminate-stamped"
        );
        assert!(
            !trunk.cancel_token().is_cancelled(),
            "cancel_descendants never touches the trunk's own token"
        );

        // A stamped terminate cause would survive this round trip and an
        // uncancelled token does not, so this proves the trunk carries no
        // terminate cause at all — not merely that this call skipped it.
        trunk.cancel_token().cancel(CancelCause::Interrupt);
        trunk.cancel_token().reset();
        assert!(
            !trunk.cancel_token().is_cancelled(),
            "the trunk's token round-trips through an interrupt and reset \
             uncancelled, so the next run after /clear would run"
        );
    }

    /// The other reason the trunk must never carry a `DurableRoot`: a
    /// terminate of the `cancel`/`` agents `cancel `` class landing on it must
    /// not poison the session.  A root states its reach pre-weakened, so a
    /// terminate never reaches the root that construction captured.
    #[test]
    fn a_terminate_on_the_trunk_leaves_its_session_root_uncancelled() {
        let fleet = AgentRegistry::new();
        let root = DurableRoot::default();
        let mut trunk = spec("trunk", None);
        trunk.reach = reach_into(&root).interrupt_only();
        let trunk = born(&fleet, trunk).expect("a fresh trunk");

        trunk.cancel_tree(CancelCause::Explicit);

        assert!(
            trunk.cancel_token().terminated(),
            "the cooperative token still trips: an in-flight attend loop must still unwind"
        );
        assert_eq!(
            root.as_scope().cause(),
            None,
            "a root states its reach pre-weakened, so terminate never reaches the root — \
             the session survives a cancel aimed at the trunk"
        );
    }

    /// Where the `/clear` cascade would have left the root live, `/close`
    /// takes it with the subtree.
    #[test]
    fn cancel_tree_takes_the_root_too() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let branch_root = DurableRoot::default();
        let mut branch = spec("branch", Some(&trunk));
        branch.reach = reach_into(&branch_root);
        let branch = born(&fleet, branch).expect("a fresh child of a live trunk");
        let grandchild = agent(&fleet, "grandchild", Some(&branch));

        branch.cancel_tree(CancelCause::Explicit);

        assert!(
            branch.cancel_token().is_cancelled(),
            "the closed branch's token is set"
        );
        assert!(
            branch_root.as_scope().is_cancelled(),
            "close reaches the branch's eval layer, not just its token"
        );
        assert!(
            grandchild.cancel_token().is_cancelled(),
            "a spawned descendant cascades"
        );
        assert!(
            !trunk.cancel_token().is_cancelled(),
            "the trunk above the closed subtree survives"
        );
    }

    /// An interrupt drops the run without ending the agent, so the child's
    /// token is cancelled but not `terminated`, and it walks no descendants.
    /// Contrast `cancel_tree`, which would trip the grandchild too, and with a
    /// terminate cause.
    #[test]
    fn interrupt_unwinds_exactly_one_agent() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let child_root = DurableRoot::default();
        // What the child's own dispatch would publish into its interrupt target
        // as that dispatch was minted: a scope under its session, whose run
        // frame is born a descendant.
        let target: InterruptTarget = Arc::new(Mutex::new(Some(child_root.worker().child())));
        let mut child = spec("child", Some(&trunk));
        child.reach = EvalReach::Identity {
            eval_root: Some(child_root.clone()),
            interrupt_target: target.clone(),
        };
        let child = born(&fleet, child).expect("a fresh child of a live trunk");
        let grandchild = agent(&fleet, "grandchild", Some(&child));

        child.interrupt();

        assert!(
            child.cancel_token().is_cancelled(),
            "interrupt trips the child's token"
        );
        assert!(
            !child.cancel_token().terminated(),
            "an interrupt is not a terminate cause"
        );
        assert!(
            target
                .lock()
                .unwrap()
                .as_ref()
                .expect("the cell holds the in-flight dispatch's scope")
                .is_cancelled(),
            "the interrupt reaches whatever scope the interrupt target holds"
        );
        assert!(
            !child_root.as_scope().is_cancelled(),
            "eval_root itself is never touched by an interrupt"
        );
        assert!(
            !grandchild.cancel_token().is_cancelled(),
            "no descendant walk: the grandchild is untouched"
        );
    }

    /// An interrupted agent's *next* run must run uncancelled: the interrupt
    /// reaches only the interrupt target, never `eval_root`, so a foreground
    /// scope freshly minted from the root starts clean.
    #[test]
    fn interrupt_never_poisons_the_next_run() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let eval_root = DurableRoot::default();
        let dispatches = eval_root.worker();
        let target: InterruptTarget = Arc::new(Mutex::new(Some(dispatches.child())));
        let mut child = spec("child", Some(&trunk));
        child.reach = EvalReach::Identity {
            eval_root: Some(eval_root),
            interrupt_target: target.clone(),
        };
        let child = born(&fleet, child).expect("a fresh child of a live trunk");

        child.interrupt();
        assert!(
            target.lock().unwrap().as_ref().unwrap().is_cancelled(),
            "the in-flight run did unwind"
        );

        // The next dispatch mints a fresh scope beside the cancelled one and
        // republishes it into the same target, as `IdentityTransport::dispatch`
        // does every time.
        let next_run = dispatches.child();
        *target.lock().unwrap() = Some(next_run.clone());

        assert!(
            !next_run.is_cancelled(),
            "the next run is born uncancelled — the interrupt never poisoned eval_root"
        );
    }

    #[test]
    fn cancel_sets_the_token_and_the_roster_reports_the_subtree() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let eval_root = DurableRoot::default();
        let mut lint = spec("lint", Some(&trunk));
        lint.reach = reach_into(&eval_root);
        let lint = born(&fleet, lint).expect("a fresh child of a live trunk");

        assert_eq!(listing(&trunk).len(), 1, "the trunk lists its one child");
        assert_eq!(listing(&trunk)[0].name, "lint");

        lint.cancel_tree(CancelCause::Explicit);
        assert!(lint.cancel_token().is_cancelled(), "cancel sets the token");
        assert!(
            eval_root.as_scope().is_cancelled(),
            "cancel reaches the worker's eval layer through its session root"
        );
    }

    #[test]
    fn cancel_cascades_to_the_whole_subtree() {
        let fleet = AgentRegistry::new();
        let root = agent(&fleet, "r", None);
        let child = agent(&fleet, "c", Some(&root));
        let grandchild = agent(&fleet, "g", Some(&child));
        let sibling = agent(&fleet, "s", Some(&root));

        child.cancel_tree(CancelCause::Explicit);

        assert!(child.cancel_token().is_cancelled(), "the cancelled node");
        assert!(
            grandchild.cancel_token().is_cancelled(),
            "its descendant cascades"
        );
        assert!(
            !sibling.cancel_token().is_cancelled(),
            "a sibling is untouched"
        );
        assert!(!root.cancel_token().is_cancelled(), "the parent is untouched");
    }

    /// Liveness is the avatar: an agent leaves both fleet doors and its
    /// parent's subtree by being dropped, and by nothing else.
    #[test]
    fn an_agent_resolves_only_while_something_holds_it() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let child = agent(&fleet, "child", Some(&trunk));
        let id = child.id;

        assert!(fleet.by_id(id).is_some(), "a live agent resolves by id");
        assert!(fleet.name_live("child"), "and by name");
        assert_eq!(listing(&trunk).len(), 1, "and stands in its parent's roster");

        drop(child);

        assert!(fleet.by_id(id).is_none(), "a settled agent resolves nowhere");
        assert!(!fleet.name_live("child"), "and frees its name");
        assert!(
            listing(&trunk).is_empty(),
            "and the walk that looked for it pruned it"
        );
    }

    #[test]
    fn a_message_posts_a_marked_note_to_a_descendant() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let inbox = Inbox::new();
        let mut worker = spec("worker", Some(&trunk));
        worker.mailbox = inbox.mailbox();
        let worker = born(&fleet, worker).expect("a fresh child of a live trunk");

        let to = trunk
            .descendant(&worker)
            .expect("a proper descendant is reachable");
        trunk
            .message(&to, "check the lexer".into())
            .expect("a fresh inbox admits one note");

        let Some(Item::Message(msg)) = inbox.next_item() else {
            panic!("expected a peer message");
        };
        assert_eq!(msg.from, trunk.id);
        assert_eq!(msg.from_name, "trunk");
        assert_eq!(msg.text, "check the lexer");
    }

    /// An ancestor, a sibling, and the caller's own self are all refused, even
    /// though all three are live agents an existence check alone would admit.
    #[test]
    fn the_scope_climb_refuses_self_and_non_descendants() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let child = agent(&fleet, "child", Some(&trunk));
        let sibling = agent(&fleet, "sibling", Some(&trunk));
        let grandchild = agent(&fleet, "grandchild", Some(&child));

        assert!(child.descendant(&child).is_none(), "self is refused");
        assert!(child.descendant(&sibling).is_none(), "a sibling is refused");
        assert!(child.descendant(&trunk).is_none(), "an ancestor is refused");
        assert!(
            trunk.descendant(&grandchild).is_some(),
            "a descendant at any depth is reachable"
        );
    }

    /// A `/branch` child roots its own tree rather than joining its creator's:
    /// the fleet the model manages never lists it, and the model's own cancel
    /// verb cannot reach it through the spawner.
    #[test]
    fn a_branch_is_a_root_outside_its_spawners_fleet() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let branch = agent(&fleet, "branch", None);

        assert!(
            listing(&trunk).is_empty(),
            "the trunk's listing omits a branch that shares its fleet but no edge"
        );
        assert!(
            trunk.descendant(&branch).is_none(),
            "the scope climb refuses a target the caller never spawned"
        );
    }

    /// The tail of a spawn racing a `/clear` or `` agents `cancel `` on its
    /// parent is refused outright, rather than landing orphaned and
    /// uncancellable.  The cascade never drops an agent, so "gone" alone would
    /// miss this window.
    #[test]
    fn a_terminated_parent_bears_no_more_children() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let parent = agent(&fleet, "parent", Some(&trunk));
        parent.cancel_token().cancel(CancelCause::Explicit);

        assert_eq!(
            born(&fleet, spec("late-child", Some(&parent))).err(),
            Some(Unborn::SessionDead),
            "a parent already carrying a terminate cause refuses new children"
        );
        assert!(!fleet.name_live("late-child"), "and the name stays free");
        assert!(
            parent.children().is_empty(),
            "a refused child is never adopted"
        );
    }

    /// Two same-name births never both succeed, even with nothing settled
    /// between them: the desk's [`AgentRegistry::name_live`] pre-check is the
    /// cheap half, this the check that closes the race.
    #[test]
    fn a_name_borne_by_a_live_agent_refuses_a_second() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let helper = agent(&fleet, "helper", Some(&trunk));

        assert_eq!(
            born(&fleet, spec("helper", Some(&trunk))).err(),
            Some(Unborn::NameTaken("helper".to_string())),
            "a second live agent may not bear the same name"
        );
        assert!(
            fleet
                .resolve("helper")
                .is_some_and(|found| Arc::ptr_eq(&found, &helper)),
            "the name still resolves to the agent that actually holds it"
        );
        assert_eq!(trunk.children().len(), 1, "the refused child is not adopted");
    }

    /// A reaped agent and a cancelled one share one cascade, distinguished
    /// only by the cause each caller passes.  Pinned directly here, away from
    /// the lease chain's own timing.
    #[test]
    fn a_lease_reap_reports_deadline_not_explicit() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let eval_root = DurableRoot::default();
        let mut worker = spec("worker", Some(&trunk));
        worker.reach = reach_into(&eval_root);
        let worker = born(&fleet, worker).expect("a fresh child of a live trunk");

        worker.cancel_tree(CancelCause::Deadline);

        assert_eq!(
            eval_root.as_scope().cause(),
            Some(CancelCause::Deadline),
            "a reaped worker's eval layer reports it timed out, not that it was explicitly cancelled"
        );
    }

    /// `steer` is the one door that stamps the exchange clock, and it always
    /// delivers too.
    #[test]
    fn a_steer_defers_the_fire() {
        let fleet = AgentRegistry::new();
        let ttl = Duration::from_millis(300);
        fleet.set_lease(ttl);
        let trunk = agent(&fleet, "trunk", None);
        let eval_root = DurableRoot::default();
        let mut child = spec("child", Some(&trunk));
        child.reach = reach_into(&eval_root);
        let child = born(&fleet, child).expect("a fresh child of a live trunk");

        std::thread::sleep(ttl / 2);
        child.mailbox().steer("still there".into());

        std::thread::sleep(ttl / 2 + Duration::from_millis(50));
        assert!(
            !eval_root.as_scope().is_cancelled(),
            "steered at half the ttl, still alive past the original bound"
        );
        assert!(
            eventually(Duration::from_secs(2), || eval_root
                .as_scope()
                .is_cancelled()),
            "reaped once the renewed span elapses"
        );
    }

    #[test]
    fn settling_before_the_fire_ends_the_chain_silently() {
        let fleet = AgentRegistry::new();
        let ttl = Duration::from_millis(80);
        fleet.set_lease(ttl);
        let trunk = agent(&fleet, "trunk", None);
        let token = Token::new();
        let mut child = spec("child", Some(&trunk));
        child.cancel = token.clone();
        let child = born(&fleet, child).expect("a fresh child of a live trunk");

        drop(child);

        std::thread::sleep(ttl * 4);
        assert!(
            !token.is_cancelled(),
            "the chain's late fire found nothing to upgrade and never touched the settled token"
        );
    }

    #[test]
    fn a_clear_outranks_the_lease() {
        let fleet = AgentRegistry::new();
        let ttl = Duration::from_millis(80);
        fleet.set_lease(ttl);
        let trunk = agent(&fleet, "trunk", None);
        let eval_root = DurableRoot::default();
        let mut child = spec("child", Some(&trunk));
        child.reach = reach_into(&eval_root);
        let child = born(&fleet, child).expect("a fresh child of a live trunk");

        trunk.clear_subtree();
        assert_eq!(
            eval_root.as_scope().cause(),
            Some(CancelCause::Explicit),
            "clear_subtree cancels through the ordinary /clear cause"
        );

        // The cancelled child's own loop would retire it; here the drop is
        // that retirement, and it is what ends the lease chain.
        drop(child);
        std::thread::sleep(ttl * 4);
        assert_eq!(
            eval_root.as_scope().cause(),
            Some(CancelCause::Explicit),
            "the lease's late fire finds a settled agent and never overwrites the cause"
        );
    }

    /// Notably, a raw push onto the mailbox, bypassing `steer`, is not an
    /// exchange.
    #[test]
    fn engaged_and_idle_read_the_exchange_clock() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let inbox = Inbox::new();
        let mut child = spec("child", Some(&trunk));
        child.mailbox = inbox.mailbox();
        let child = born(&fleet, child).expect("a fresh child of a live trunk");

        assert!(!child.messageable(), "a fresh agent has never been engaged");

        inbox.push(Post::UserSteering("hello".into())).unwrap();
        assert!(!child.messageable(), "a raw mailbox push is not an exchange");
        // Drained, so the steer below lands as its own item rather than
        // coalescing into this one.
        inbox.next_item();

        child.mailbox().steer("hi".into());
        assert!(child.messageable(), "steered at least once");
        assert!(
            child.idle() < Duration::from_secs(1),
            "idle resets to (near) zero right after a steer"
        );

        let Some(Item::Human(text)) = inbox.next_item() else {
            panic!("expected the steered text to have actually landed");
        };
        assert_eq!(text, "hi", "a steer delivers as well as stamping");
    }

    /// The fleet's ordering law: a settling child delivers *before* it retires,
    /// so the parent's park verdict — computed ahead of the pop — can never
    /// quiesce between the two facts.
    #[test]
    fn a_settling_childs_result_outruns_the_quiesce_verdict() {
        let fleet = AgentRegistry::new();
        let trunk = agent(&fleet, "trunk", None);
        let child = agent(&fleet, "child", Some(&trunk));
        let generation = child.consumer();
        let inbox = Inbox::new();
        let park = || {
            if trunk.has_busy_children() {
                ParkMode::HeldByChildren
            } else {
                ParkMode::Quiesce
            }
        };
        assert_eq!(
            park(),
            ParkMode::HeldByChildren,
            "a live direct child holds its parent's park"
        );

        // The worker epilogue in production order: deliver, then retire.
        inbox
            .mailbox()
            .push(Post::AgentResult(AgentResult {
                name: "child".into(),
                outcome: AgentOutcome::Stopped("done".into()),
                elapsed: Duration::ZERO,
                generation,
            }))
            .expect("a fresh inbox admits one result");
        drop(child);
        assert_eq!(
            park(),
            ParkMode::Quiesce,
            "the last child settling drains the fleet"
        );

        let token = Token::new();
        assert!(
            matches!(inbox.next_or_idle(park, &token), Some(Item::Agent(r)) if r.name == "child"),
            "a quiescing fleet still hands over the result it was already owed"
        );
        assert!(
            inbox.next_or_idle(park, &token).is_none(),
            "and with nothing left queued it quiesces rather than parking forever"
        );
    }
}
