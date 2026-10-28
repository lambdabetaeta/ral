//! The agent node, split by who may touch what: [`Agent`], the public half
//! the fleet shares behind an `Arc`, and [`Avatar`], the private half the
//! attend thread owns.  Nodes differ only by [`Role`]; the loop is one.
//!
//! Nothing holds an `Arc<Agent>` to a descendant: up is strong
//! ([`Role::Child`]), down is weak ([`Agent::children`]).  An agent's locks
//! (`status`, `children`, `schedules`, `pins`) are held one at a time, and
//! never while pushing into an inbox.

mod arrival;
mod attend;
mod build;
mod command;
mod deliberate;
pub(crate) mod desk;
pub mod digest;
mod engine_lost;
mod factory;
pub mod fleet;
pub(crate) mod gauge;
mod helm;
pub(crate) mod index;
pub mod nudge;
pub mod outcome;
pub mod resources;
pub mod roster;
pub(crate) mod seat;
mod shell;
pub(crate) mod spawn;
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(crate) mod testkit;
pub(crate) mod tools;

#[cfg(test)]
pub(crate) use build::TestTrunk;
pub(crate) use build::fresh_id;
pub use build::{RootConfig, RootSeat, Trunk};
pub use engine_lost::{EngineLost, EnginePhase};
pub use factory::Factory;
pub use helm::{Ended, Helm};
pub(crate) use shell::{Evaluated, LogCell, ReplyCell, SharedCell};

use crate::agent::fleet::Fleet;
use crate::agent::seat::Seat;
use crate::bus::{AgentMessage, AgentOutcome, AgentResult, Inbox, Mailbox, Post, Stamp, Stamped};
use crate::cancel::{self, InterruptTarget};
use crate::provider::Provider;
use crate::record::AgentId;
use crate::shell_eval;
use ral_core::data::FOValue;
use ral_core::own::OwnDir;
use ral_core::process::CancelCause;
use ral_core::sync::LockExt;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// Retained agent identity and shared registers.
///
/// Its token records consumer retirement; children may retain the identity
/// after its [`Avatar`] has gone.
pub struct Agent {
    pub id: AgentId,
    /// Unique among live agents, enforced at [`Fleet::claim`].
    pub(crate) name: String,
    pub(crate) log_dir: OwnDir,
    /// The run's directory, which this agent's `log_dir` hangs under.
    run_dir: OwnDir,
    started: Instant,
    pub(crate) role: Role,
    /// Weak: a child that fails to upgrade has settled, and is pruned.
    children: Mutex<Vec<Weak<Self>>>,
    /// Spawn generations left below here.  Bounds depth, not fan-out.
    pub(crate) fuel: u32,
    pub(crate) provider: ProviderHandle,
    /// Sticky for the agent's life, so a subtree cascade reaches the live
    /// exchange; reset at each exchange boundary.
    pub(crate) token: cancel::Token,
    /// The network policy's verdict on hosted web search.
    pub(crate) search: bool,
    reach: InterruptTarget,
    pub(crate) mailbox: Mailbox,
    pub(crate) schedules: crate::schedule::ScheduleRegistry,
    /// Mirrors the pin register, for the nudge to name what is pinned.
    pub(crate) pins: shell_eval::PinDigests,
    /// Written by the avatar alone, read by everyone; nothing is computed
    /// under the lock.
    status: Mutex<Status>,
}

/// Whom an agent answers, fixed at birth.  The desk's `reply` refusal and the
/// prompt's taught sections read this one value, so they cannot disagree.
pub(crate) enum Role {
    /// Converses turn after turn and holds no `reply`: an attended or embedded
    /// root.
    Converses,
    /// The headless trunk: replies once, to the process's caller, and ends.
    Headless,
    /// Spawned: replies to its parent and parks to be messaged again.
    Child {
        /// Strong and upward, so a parent whose avatar has gone is still
        /// reachable, terminated token and all, and `deposit_reply` and the
        /// scope climb never dangle.
        parent: Arc<Agent>,
        /// The parent's envelope, minted at birth: what a result posted
        /// upward is addressed to.
        consumer: Stamp,
    },
}

impl Role {
    #[cfg(test)]
    pub(crate) fn child(parent: &Arc<Agent>) -> Self {
        Self::Child {
            consumer: parent.mailbox.stamp(),
            parent: parent.clone(),
        }
    }

    pub(crate) fn returns(&self) -> bool {
        !matches!(self, Self::Converses)
    }

    /// `None` ⇔ a root of its tree.
    pub(crate) fn parent(&self) -> Option<&Arc<Agent>> {
        match self {
            Self::Child { parent, .. } => Some(parent),
            Self::Converses | Self::Headless => None,
        }
    }
}

struct Status {
    /// When this agent parked for a message; `None` while it works.
    rest: Option<Instant>,
    /// The last value passed to `reply`.  Survives a wake: a messaged agent
    /// works again with its reply still standing.
    reply: Option<FOValue>,
    /// The reply whose follow-up lease is armed; cleared when work resumes.
    replied_at: Option<Instant>,
    /// Direct children spoken to and not yet heard back from.  Kept on the
    /// consuming side, so the park verdict flips exactly when a result is
    /// taken up.
    awaiting: BTreeSet<AgentId>,
}

/// What an [`Agent`] is born with; [`Agent::new`] adds the empty registers.
pub(crate) struct Birth {
    pub id: AgentId,
    pub name: String,
    pub log_dir: OwnDir,
    pub run_dir: OwnDir,
    pub started: Instant,
    pub role: Role,
    pub fuel: u32,
    pub provider: ProviderHandle,
    pub search: bool,
    pub reach: InterruptTarget,
    pub mailbox: Mailbox,
}

/// An agent's embodiment in this process: the thread that runs it, owning
/// everything only it touches.
pub struct Avatar {
    /// Shared with the per-call desk, which runs on this thread's stack.
    log: LogCell,
    pub(crate) seat: Seat,
    inbox: Inbox,
    /// Where the desk's `reply` stages a value for [`Self::deliberate`].
    reply: ReplyCell,
    pub(crate) fleet: Arc<Fleet>,
    readings: Readings,
    work: Stamp,
    pub(crate) agent: Arc<Agent>,
    // Last: emptiness follows seat teardown and the log's `SessionEnded`.
    _membership: fleet::Membership,
}

/// What an avatar has measured and told about one context: reborn whole on
/// `/clear`, since a rebuilt context has been told nothing.
struct Readings {
    measure: Option<gauge::Measure>,
    gauges: gauge::Gauges,
    nudges: nudge::Nudges,
}

impl Readings {
    /// `steers` is whether the model holds a tool to be steered toward — off
    /// for a toolless `--chat` trunk, whose turns are only ever reported.
    fn new(steers: bool) -> Self {
        Self {
            measure: None,
            gauges: gauge::Gauges::default(),
            nudges: nudge::Nudges::new(steers),
        }
    }

    fn reborn(&self) -> Self {
        Self::new(self.nudges.steers())
    }
}

/// The depth budget exarch's trunks start with.
///
/// At zero the desk refuses `` exarch-agents `start ``
/// ([`crate::agent::desk::ExarchDesk::launch`]), so a runaway spawn chain
/// exhausts fuel instead of threads.  Fan-out is unbounded.
pub const SPAWN_FUEL: u32 = 3;

/// A shared, swappable handle to the active provider: live replacement across
/// the UI / attend thread boundary needs a cell, not an `Arc` the worker owns
/// privately.
///
/// A peer wraps a *snapshot* taken at spawn, so a later root `/model` never
/// disturbs a running child.
#[derive(Clone)]
pub struct ProviderHandle(Arc<Mutex<Arc<Provider>>>);

impl ProviderHandle {
    pub fn new(provider: Arc<Provider>) -> Self {
        Self(Arc::new(Mutex::new(provider)))
    }

    /// The provider in force for the next item.
    pub fn current(&self) -> Arc<Provider> {
        self.0.lock_ignore_poison().clone()
    }

    /// Replace the active provider (a `/model` switch).  An in-flight
    /// deliberation finishes on the provider it started with.
    pub fn swap(&self, provider: Arc<Provider>) {
        *self.0.lock_ignore_poison() = provider;
    }
}

impl Agent {
    /// The one literal: every register starts empty.
    pub(crate) fn new(birth: Birth) -> Arc<Self> {
        let Birth {
            id,
            name,
            log_dir,
            run_dir,
            started,
            role,
            fuel,
            provider,
            search,
            reach,
            mailbox,
        } = birth;
        Arc::new(Self {
            id,
            name,
            log_dir,
            run_dir,
            started,
            role,
            children: Mutex::new(Vec::new()),
            fuel,
            provider,
            token: cancel::Token::new(),
            search,
            reach,
            mailbox,
            schedules: crate::schedule::ScheduleRegistry::new(),
            pins: Arc::default(),
            status: Mutex::new(Status {
                rest: None,
                reply: None,
                replied_at: None,
                awaiting: BTreeSet::new(),
            }),
        })
    }

    pub(crate) fn steer(&self, text: String) {
        self.mailbox.steer(text);
    }

    pub(crate) fn post(&self, post: Post) {
        self.mailbox.push(post);
    }

    pub(crate) fn queued(&self) -> Vec<String> {
        self.mailbox.queued_human_messages()
    }

    pub(crate) fn recall(&self) -> Vec<String> {
        self.mailbox.recall()
    }

    pub(crate) fn waiting_for_input(&self) -> bool {
        self.mailbox.waiting_for_input()
    }

    /// Abandon this exchange and its descendants; leave the agent live.
    pub(crate) fn stop(&self) -> Vec<String> {
        // Hold admission through all out-of-lock cancellation delivery.
        let Some((_hold, recalled, children)) =
            self.mailbox.begin_abandon(Some(&self.token), || {
                self.status.lock_ignore_poison().awaiting.clear();
                self.children()
            })
        else {
            return Vec::new();
        };
        for child in children {
            child.cancel_tree(CancelCause::Cancelled);
        }
        self.reach.interrupt();
        recalled
    }

    /// End this agent and its subtree for good.
    pub(crate) fn end(&self) {
        self.cancel_tree(CancelCause::Cancelled);
    }

    fn abandon_delegation(&self) {
        let Some((_hold, _, children)) = self.mailbox.begin_abandon(None, || {
            self.status.lock_ignore_poison().awaiting.clear();
            self.children()
        }) else {
            return;
        };
        for child in children {
            child.cancel_tree(CancelCause::Cancelled);
        }
    }

    pub(crate) fn current_provider(&self) -> Arc<Provider> {
        self.provider.current()
    }

    /// The run's directory: where the engine's captured output and the lock
    /// live.
    pub(crate) fn run_dir(&self) -> &OwnDir {
        &self.run_dir
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub(crate) fn rest(&self) -> Option<Instant> {
        self.status.lock_ignore_poison().rest
    }

    pub(crate) fn reply(&self) -> Option<FOValue> {
        self.status.lock_ignore_poison().reply.clone()
    }

    pub(crate) fn has_reply(&self) -> bool {
        self.status.lock_ignore_poison().reply.is_some()
    }

    /// Mark this agent parked for a message, or working again.  Setting it
    /// twice running keeps the first stamp, so the roster's `idle-s` measures
    /// from when the agent parked and not from the attend loop's latest pass.
    pub(crate) fn set_resting(&self, resting: bool) {
        let mut status = self.status.lock_ignore_poison();
        if resting {
            status.rest.get_or_insert_with(Instant::now);
        } else {
            status.rest = None;
        }
    }

    pub(crate) fn reply_completion_armed(&self) -> bool {
        self.status.lock_ignore_poison().replied_at.is_some()
    }

    pub(crate) fn disarm_reply_lease(&self) {
        self.status.lock_ignore_poison().replied_at = None;
    }

    /// Stage `reply` for this agent's consumer to fetch, then report
    /// [`AgentOutcome::Replied`] — deposit first, so a parent woken by the
    /// notice always finds the value already there.  A root reports to
    /// nobody; its driver reads the deposit off [`Self::reply`].
    pub(crate) fn deposit_reply(self: &Arc<Self>, fleet: &Arc<Fleet>, reply: FOValue) {
        let since = Instant::now();
        {
            let mut status = self.status.lock_ignore_poison();
            status.reply = Some(reply);
            status.replied_at = Some(since);
        }
        fleet.lease_reply(self, since);
        self.report(AgentOutcome::Replied);
    }

    pub(crate) fn reply_since(&self) -> Option<Instant> {
        let since = self.status.lock_ignore_poison().replied_at?;
        // A queued follow-up protects the child before its loop takes it up.
        self.mailbox
            .last_exchange()
            .is_none_or(|at| at <= since)
            .then_some(since)
    }

    /// Deliver `outcome` to the parent's inbox as this agent's result.  A root
    /// has no parent and reports to nobody.
    pub(crate) fn report(&self, outcome: AgentOutcome) {
        let Role::Child { consumer, .. } = &self.role else {
            return;
        };
        consumer.post(Stamped::AgentResult(AgentResult {
            id: self.id,
            name: self.name.clone(),
            outcome,
            elapsed: self.elapsed(),
        }));
    }

    /// Send a marked model-visible note to `to` as an exchange, so a woken
    /// resting child is not reaped mid-answer.  A direct child so woken is
    /// awaited again; anyone else answers their own parent, not us.  Unscoped:
    /// any live agent may be messaged, in any direction across the tree.
    pub(crate) fn message(&self, to: &Self, text: String) {
        if to.role.parent().is_some_and(|p| p.id == self.id) {
            self.status.lock_ignore_poison().awaiting.insert(to.id);
        }
        to.mailbox.exchange(Post::AgentMessage(AgentMessage {
            from: self.id,
            from_name: self.name.clone(),
            text,
        }));
    }

    /// A result from direct child `child` has been taken up: it no longer
    /// holds this agent's park.
    pub(crate) fn heard(&self, child: AgentId) {
        self.status.lock_ignore_poison().awaiting.remove(&child);
    }

    /// Time since this agent's last human exchange, or since birth if never
    /// engaged.
    pub(crate) fn idle(&self) -> Duration {
        self.mailbox
            .last_exchange()
            .unwrap_or(self.started)
            .elapsed()
    }

    /// This agent's cancellation handle.
    pub(crate) fn reach(&self) -> cancel::Reach {
        cancel::Reach {
            token: self.token.clone(),
            target: self.reach.clone(),
        }
    }

    /// Cancel this agent across both terminate-class layers: the cooperative
    /// [`cancel::Token`] the attend loop polls and its engine's durable root.
    pub(crate) fn cancel(&self, cause: CancelCause) {
        self.mailbox.admission(|| self.token.cancel(cause));
        self.reach.terminate();
    }

    /// Unwind this agent's in-flight run without ending it: the Esc/Ctrl-C
    /// path, and the `` exarch-agents `cancel `` scoped verb's per-target primitive.
    pub(crate) fn interrupt(&self) {
        self.reach().interrupt();
    }

    /// Take `child` under this agent — the one downward edge, weak, written
    /// once at the child's construction.
    pub(crate) fn adopt(&self, child: &Arc<Self>) {
        self.children
            .lock_ignore_poison()
            .push(Arc::downgrade(child));
        self.status.lock_ignore_poison().awaiting.insert(child.id);
    }

    /// This agent's live direct children, pruning the settled ones as it
    /// snapshots.  The lock is held for the snapshot alone: every walk runs
    /// outside it.
    pub(crate) fn children(&self) -> Vec<Arc<Self>> {
        let mut kids = self.children.lock_ignore_poison();
        let live: Vec<Arc<Self>> = kids.iter().filter_map(Weak::upgrade).collect();
        if live.len() != kids.len() {
            *kids = live.iter().map(Arc::downgrade).collect();
        }
        live
    }

    /// This agent's live proper descendants at any depth — the pruning
    /// descent.  An agent walks what it spawned, never itself.
    pub(crate) fn walk(&self) -> Vec<Arc<Self>> {
        let mut out = Vec::new();
        let mut frontier = self.children();
        while let Some(node) = frontier.pop() {
            frontier.extend(node.children());
            out.push(node);
        }
        out
    }

    /// `target` if it is a proper descendant of this agent, else `None` — the
    /// scoping `` exarch-agents `cancel `` and `` `read `` share.  A climb
    /// from `target`'s *parent*, so it costs O(depth) and takes no lock, and
    /// so nothing is a proper descendant of itself.
    pub(crate) fn descendant(&self, target: &Arc<Self>) -> Option<Arc<Self>> {
        let mut above = target.role.parent();
        while let Some(node) = above {
            if std::ptr::eq(Arc::as_ptr(node), std::ptr::from_ref(self)) {
                return Some(target.clone());
            }
            above = node.role.parent();
        }
        None
    }

    /// The root of this agent's tree — itself, if it is one.  Every root is
    /// its own session, which is what keeps one tab's listing out of another's.
    pub(crate) fn root(self: &Arc<Self>) -> Arc<Self> {
        let mut here = self.clone();
        while let Some(up) = here.role.parent().cloned() {
            here = up;
        }
        here
    }

    /// The park signal for a node that launched async agents: a live direct
    /// child this agent is still [`Status::awaiting`] may yet deliver to this
    /// mailbox.  One heard from has said what it had to say — it holds its
    /// reply and waits to be messaged — and one that died delivered first.
    pub(crate) fn has_busy_children(&self) -> bool {
        let live = self.children();
        let status = self.status.lock_ignore_poison();
        live.iter().any(|c| status.awaiting.contains(&c.id))
    }

    /// Cancel this agent and its whole subtree.  Every victim is stamped
    /// across both layers, so an in-flight eval unwinds instead of grinding on
    /// as an orphan whose result nobody will collect.
    pub(crate) fn cancel_tree(&self, cause: CancelCause) {
        let children = self.mailbox.admission(|| {
            self.token.cancel(cause);
            self.children()
        });
        self.reach.terminate();
        for child in children {
            child.cancel_tree(cause);
        }
    }

    /// Cancel this agent's proper descendants, leaving it live — a settling
    /// parent abandoning its children, and `/clear`.  Both rebuild in place,
    /// so this must never stamp the root's own [`cancel::Token`]: a terminate
    /// cause there is permanent ([`cancel::Token::reset`] clears only a bare
    /// [`CancelCause::Interrupted`]) and every later run would fail.
    pub(crate) fn cancel_descendants(&self, cause: CancelCause) {
        let (_hold, children) = self.mailbox.hold(|| self.children());
        for child in children {
            child.cancel_tree(cause);
        }
    }

    /// `/clear`: abandon the subtree the rebuilt context no longer owns, and
    /// every register the model wrote into it — what it was awaiting, its
    /// wakeups, its pins. Admission stays closed through subtree cancellation.
    pub(crate) fn forget(&self) {
        self.abandon_delegation();
        self.schedules.clear();
        self.pins.lock_ignore_poison().clear();
    }
}

impl Avatar {
    pub fn helm(&self) -> Helm {
        Helm(Arc::downgrade(&self.agent))
    }

    pub(crate) fn await_human(&self) -> bool {
        self.inbox.await_human(&self.agent.token)
    }

    pub fn id(&self) -> AgentId {
        self.agent.id
    }

    /// Deliver `outcome` to the parent, then retire: dropping the avatar is the
    /// retirement, and consuming `self` here is what keeps the two in order.
    /// A replied child was already reported at deposit time, so `outcome` is
    /// reported here only when it is not [`AgentOutcome::Replied`].
    pub(crate) fn settle(self, outcome: AgentOutcome) {
        if !matches!(outcome, AgentOutcome::Replied) {
            self.agent.report(outcome);
        }
        drop(self);
    }

    /// Where `record.jsonl` and its siblings are written.
    pub fn log_dir(&self) -> &OwnDir {
        &self.agent.log_dir
    }

    /// See [`Agent::run_dir`].
    pub fn run_dir(&self) -> &OwnDir {
        self.agent.run_dir()
    }

    /// Why the engine is gone, if it is: for a front-end to fetch the
    /// engine's own account from wherever it ran.
    pub fn severance(&self) -> Option<ral_core::carrier::Severed> {
        self.seat.severed()
    }

    /// Whether a root, refused until a reset, resumes there by a wakeup —
    /// the launch's say, for roots alone: a fork fails up to its parent.
    pub(crate) fn resumes_at_reset(&self) -> bool {
        self.agent.role.parent().is_none() && self.fleet.launch.resume_on_reset
    }

    /// The queue the attend loop is draining, for a test to read from.
    #[cfg(test)]
    pub(crate) fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    /// Every deliberation must hand the session back at a ready boundary.
    ///
    /// # Panics
    /// Panics if the log cell is contended — see [`SharedCell::borrow`].
    pub fn is_ready(&self) -> bool {
        self.log.borrow().context().is_ready()
    }

    /// The model-view messages the next request would carry.
    ///
    /// # Panics
    /// Panics if the log cell is contended — see [`SharedCell::borrow`].
    pub fn rendered_messages(&self) -> Vec<genai::chat::ChatMessage> {
        self.log.borrow().context().rendered()
    }

    /// For a test polling an async spawn's settle without a full deliberation.
    #[cfg(test)]
    pub(crate) fn next_item_for_test(&self) -> Option<crate::bus::Next> {
        self.inbox.next_item()
    }
}

#[cfg(test)]
mod helm_tests {
    use super::testkit::{started_child, trunk};
    use super::*;
    use crate::bus::Emitter;

    #[test]
    fn stop_cancels_every_descendant_and_leaves_the_agent_live() {
        let root = trunk(false);
        let child = started_child(&root, "helper", "`inherit");
        let grandchild = started_child(&child, "nested", "`inherit");
        assert!(root.agent.has_busy_children());
        root.agent.stop();
        assert!(child.agent.token.terminated());
        assert!(grandchild.agent.token.terminated());
        assert!(root.agent.token.is_cancelled());
        assert!(!root.agent.token.terminated());
        assert!(!root.agent.has_busy_children());
    }

    #[test]
    fn retiring_an_avatar_closes_admission_but_preserves_descendant_identity() {
        let root = trunk(false);
        let child = started_child(&root, "helper", "`inherit");
        let helm = root.helm();
        let fleet = root.fleet.clone();
        let identity = Arc::downgrade(&root.agent);
        drop(root);
        let retired = identity.upgrade().expect("the child retains its parent");
        assert!(retired.token.terminated());
        assert!(!child.agent.token.terminated());
        assert_eq!(helm.steer("late".into()), Err(Ended));
        assert_eq!(helm.queued(), Vec::<String>::new());
        assert!(!fleet.is_empty());
        assert!(child.fork_named("late-grandchild").is_ok());
        retired.end();
        assert!(child.agent.token.terminated());
        drop(retired);
        drop(child);
        assert!(identity.upgrade().is_none());
        assert!(fleet.is_empty());
    }

    #[test]
    fn wait_empty_ignores_retained_resolved_identity_and_helm() {
        let root = trunk(false);
        let fleet = root.fleet.clone();
        let identity = fleet.resolve(&root.agent.name).unwrap();
        let helm = root.helm();
        assert!(!fleet.is_empty());
        drop(root);
        assert!(fleet.is_empty());
        let (done, finished) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            fleet.wait_empty();
            done.send(()).unwrap();
        });
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        waiter.join().unwrap();
        assert!(identity.token.terminated());
        assert_eq!(helm.steer("late".into()), Err(Ended));
    }

    #[test]
    fn wait_empty_counts_child_teardown_after_its_root_retires() {
        let root = trunk(false);
        let child = started_child(&root, "helper", "`inherit");
        let fleet = root.fleet.clone();
        let identity = fleet.resolve("helper").unwrap();
        let helm = child.helm();
        drop(root);
        assert!(!fleet.is_empty());
        // Retained ancestors still let shutdown reach the live child.
        for root in fleet.roots() {
            root.end();
        }
        assert!(identity.token.terminated());
        assert!(!fleet.is_empty(), "cancellation is not teardown");
        let (started, waiting) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        let waiting_fleet = fleet.clone();
        let waiter = std::thread::spawn(move || {
            waiting_fleet.wait_empty_after(|| started.send(()).unwrap());
            done.send(()).unwrap();
        });
        waiting.recv().unwrap();
        assert!(matches!(
            finished.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(child);
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        waiter.join().unwrap();
        assert!(fleet.is_empty());
        assert!(fleet.resolve("helper").is_some());
        assert_eq!(helm.steer("late".into()), Err(Ended));
    }

    #[test]
    fn clear_has_one_abandonment_boundary_and_rebinds_service_captures() {
        let mut root = trunk(false);
        let (sender, _receiver) = crate::bus::channel();
        let emit = Emitter::new(sender, root.agent.id);
        let before = root.host_services(&emit).stamp;
        let epoch = root.agent.mailbox.epoch();
        root.clear().unwrap();
        assert_eq!(root.agent.mailbox.epoch(), epoch + 1);
        assert!(before.is_stale());
        assert!(!root.host_services(&emit).stamp.is_stale());
    }

    #[test]
    fn an_abandoned_invocation_cannot_capture_a_fresh_service_stamp() {
        let root = trunk(false);
        let (sender, _receiver) = crate::bus::channel();
        let emit = Emitter::new(sender, root.agent.id);
        let before = root.host_services(&emit).stamp;
        root.agent.stop();
        let after = root.host_services(&emit).stamp;
        assert!(before.is_stale());
        assert!(after.is_stale());
        assert!(root.agent.mailbox.stamp().reset(&root.agent.token));
        assert!(root.host_services(&emit).stamp.is_stale());
    }

    #[test]
    fn stop_returns_queued_prompts_oldest_first() {
        let root = trunk(false);
        root.agent.steer("first".into());
        root.agent.post(Post::Nudge {
            prompt: 1,
            text: "continue".into(),
        });
        root.agent.steer("second".into());
        assert_eq!(root.agent.stop(), ["first", "second"]);
        assert!(root.inbox.is_empty());
    }
}
