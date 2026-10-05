//! The uniform agent node: canonical record log, persistent shell, capability
//! set, hot-swappable provider, and the attend loop every node runs.
//!
//! A run is a tree of these; [`Fleet`](crate::fleet::Fleet) holds what they
//! share — the lookup by name, and the [`Launch`](crate::fleet::Launch) fixed
//! once for the whole run.
//!
//! No node is privileged by special-case code — the distinctions reduce to
//! *position*.  Holding `reply` falls out of `returns`, parking out of the
//! launch's `attended`.  A child's result is posted up its parent's mailbox
//! by the spawn site, not by the loop, so the loop is identical for all.
//!
//! Two types, split along who may touch what.  [`Agent`] is the public half —
//! identity and immutable per-node config — held behind an `Arc` so the fleet
//! can share it.  [`Avatar`] is the private half — the log, the seat, the
//! inbox, and every other field only the attend thread touches — plus the
//! `Arc<Agent>` it embodies; every method that runs the agent takes
//! `&mut Avatar`.
//!
//! An agent is *live* while its avatar holds the `Arc`; nothing deregisters.
//! The tree that carries that liveness has one direction of strength, and it
//! is the invariant everything else here rests on: **nothing holds an
//! `Arc<Agent>` to a descendant.**  Up is strong ([`Agent::parent`]), so the
//! climb a scope check makes never dangles; down is weak
//! ([`Agent::children`]), so a walk prunes what fails to upgrade, and reaper
//! closures and results addressed upward carry [`std::sync::Weak`].
//!
//! Two per-agent mutexes, [`Agent::status`] and [`Agent::children`].  Rule:
//! hold at most one at a time.  `children` is locked to push or to snapshot,
//! never across a walk; `status` is a single-writer register, and nothing is
//! computed under it.  [`Agent::schedules`] and [`Agent::pins`] are two more
//! cells of public agent state, each already self-locked by its own type;
//! the same rule extends to them, and neither is ever held while pushing
//! into an inbox — a fired schedule drops its registry lock before posting
//! the wakeup.
//!
//! This file holds the state; the machinery lives in [`build`], [`attend`],
//! [`deliberate`], [`shell`], [`gauge`], and [`resources`], which reach these
//! private fields directly.

mod attend;
mod build;
pub mod cancel;
mod command;
pub mod deliberate;
mod dial;
pub mod digest;
pub(crate) mod gauge;
pub mod log;
pub mod nudge;
pub mod resources;
pub(crate) mod seat;
mod shell;
#[cfg(test)]
pub(crate) mod testkit;

#[cfg(test)]
pub(crate) use build::TestTrunk;
pub(crate) use build::{Build, fresh_id};
pub use build::{RecordedAccount, RecordedModel, RootConfig, RootSeat, Trunk};
pub use dial::Dial;
pub use log::Resumed;
pub use seat::{EngineLost, EnginePhase};
pub(crate) use shell::{Evaluated, LogCell, ReplyCell};

use crate::agent::cancel::InterruptTarget;
use crate::agent::seat::Seat;
use crate::bus::{
    AgentId, AgentMessage, AgentOutcome, AgentResult, Inbox, Mailbox, Post, Stamp, Stamped,
};
use crate::fleet::Fleet;
use crate::provider::Provider;
use crate::shell_eval;
use ral_core::process::CancelCause;
use ral_core::serial::FOValue;
use ral_core::sync::LockExt;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// What the fleet knows an agent as.
///
/// Identity and per-node config, fixed at construction — except
/// [`Self::status`], the one register the avatar writes as it runs, and
/// [`Self::children`], which the spawn site pushes onto.  Live exactly while
/// its [`Avatar`] holds the `Arc`: its parent and the fleet's [`Fleet`] hold
/// only [`Weak`], and every walk prunes what fails to upgrade.  What every
/// node shares is the fleet's [`Launch`](crate::fleet::Launch), not a field
/// here.
pub struct Agent {
    pub id: AgentId,
    /// The tab-bar identity [`crate::fleet::check_name`] validates — unique
    /// among live agents, enforced at [`Fleet::enrol`].
    pub(crate) name: String,
    /// Where this agent's own session log is written.
    pub(crate) log_dir: PathBuf,
    started: Instant,
    /// The resolved prompt that reaches the model on every turn.  `Arc<str>`
    /// so a fork's per-turn read is a refcount bump, never a re-copy of the
    /// ~38 KB template.
    pub(crate) system: Arc<str>,
    pub(crate) caps: ral_core::types::GrantStack,
    /// Strong and upward: `None` ⇔ this agent is a root — the trunk, or a
    /// `/branch` child, which converses and reports to nobody.  A parent
    /// whose avatar has gone is still reachable here, terminated token and
    /// all, so `deposit_reply` and the scope climb never dangle.
    pub(crate) parent: Option<Arc<Self>>,
    /// Weak and downward, so no cycle exists to reason about: a child that
    /// fails to upgrade has settled, and the walk that found it prunes it.
    children: Mutex<Vec<Weak<Self>>>,
    /// Spawn generations still available below here.  Bounds depth, not
    /// fan-out: a fork spends none of the parent's, only handing the child one
    /// less, and at zero the desk refuses `` exarch-agents `start ``.
    pub(crate) fuel: u32,
    /// A `/model` swaps this handle alone; a fork seeds the child's from a
    /// snapshot, so neither disturbs the other.
    pub(crate) provider: ProviderHandle,
    /// One sticky token for this agent's life, so the subtree cascade reaches
    /// the live exchange.  The attend loop
    /// [`reset`](cancel::Token::reset)s it at each exchange boundary so an Esc
    /// never bleeds into the next; the process's trunk additionally hears OS
    /// signals through [`crate::signals::face`], called by the site that
    /// launches it.
    pub(crate) token: cancel::Token,
    /// Whether this agent may ride the provider's hosted web search: the
    /// network policy's verdict at the trunk, and at most its parent's for a
    /// fork, which a spawn may narrow and never widen.
    pub(crate) search: bool,
    /// Whether this agent holds `reply`: a headless trunk and every fork do, a
    /// conversing trunk and every `/branch` do not.  The desk's refusal and
    /// the prompt's builtin index read this same bit.
    pub(crate) returns: bool,
    /// The seat's own reach into this agent's running eval: the cell its
    /// seat republishes on every rebuild, so it never goes stale.
    reach: InterruptTarget,
    /// The sender end of this agent's own inbox; the [`Inbox`] itself stays on
    /// [`Avatar`], reachable only by the attend thread.
    pub(crate) mailbox: Mailbox,
    /// Live wakeups (cron / after), posted into this agent's own inbox; the
    /// builtins that arm them gate on the launch's `allow_schedule`.  Public
    /// agent state: the desk writes it and other threads read it, so it lives
    /// here rather than on [`Avatar`].
    pub(crate) schedules: crate::fleet::schedule::ScheduleRegistry,
    /// Pins flow straight past the session to the frontend; the periodic
    /// [`nudge`] reminder reads this mirror to name what the model has
    /// pinned.  Public agent state, for the same reason as [`Self::schedules`].
    pub(crate) pins: shell_eval::PinDigests,
    /// A process publishing its status: written by the avatar alone
    /// ([`Self::set_resting`], [`Self::deposit_reply`], [`Self::heard`],
    /// [`Self::message`], [`Self::forget`]), read by everyone else.  A reader
    /// takes one snapshot and computes nothing under the lock — the mutex buys
    /// atomicity, nothing more — and a writer drops its guard before pushing
    /// to any inbox, since a park verdict reads this under one.
    status: Mutex<Status>,
    /// The parent's envelope, minted at this agent's birth: the context a
    /// line posted upward is addressed to, since the parent is who reads it.
    /// `None` for a root, which reports to nobody.
    consumer: Option<Stamp>,
}

/// [`Agent`]'s single-writer register — see [`Agent::status`]'s doc for who
/// writes what.
struct Status {
    /// When this agent parked waiting for a message, or `None` while it is
    /// working.  The roster's `idle-s`, and the whole of "not busy".
    rest: Option<Instant>,
    /// The value this agent last passed to `reply`, held for whoever consumes
    /// it: a parent's `` exarch-agents `read ``, or the driver of a root's
    /// loop.  Kept apart from [`Self::rest`] because it must survive a wake: a
    /// messaged agent is busy again with its reply still standing, until it
    /// replies afresh.
    reply: Option<FOValue>,
    /// Direct children this agent has spoken to — spawned or messaged — and
    /// not yet heard back from.  The busy/at-rest bit of the parent–child
    /// edge lives here, on the side that consumes the result, so the park
    /// verdict flips exactly when the result is taken up and never reads a
    /// child's own status.
    awaiting: BTreeSet<AgentId>,
}

/// What an [`Agent`] is born with — the one literal, which [`Agent::new`]
/// completes with the registers every agent starts empty.
pub(crate) struct Birth {
    pub id: AgentId,
    pub name: String,
    pub log_dir: PathBuf,
    pub started: Instant,
    pub system: Arc<str>,
    pub caps: ral_core::types::GrantStack,
    pub parent: Option<Arc<Agent>>,
    pub fuel: u32,
    pub provider: ProviderHandle,
    pub returns: bool,
    pub search: bool,
    pub reach: InterruptTarget,
    pub mailbox: Mailbox,
}

/// An agent's embodiment in this process: the thread that thinks and acts
/// for it, owning everything only it touches, so every method is plain
/// `&mut self`.
pub struct Avatar {
    /// The public half this avatar embodies — readable crate-wide, so a
    /// caller outside the `agent` module reaches identity and config at
    /// `.agent.…` rather than through a delegator for every field.
    pub(crate) agent: Arc<Agent>,
    /// Under its own lock so a per-call desk can capture it off `&mut Avatar`.
    /// [`LogCell::lock`] panics on contention rather than blocking — the desk
    /// runs only while the attend thread is parked in `run_shell`.
    log: LogCell,
    /// Every engine-side reach goes through this seat's methods.
    pub(crate) seat: Seat,
    /// Self-nudges and armed wakeups land here; a child's result lands in its
    /// *parent's*, never reaching across into a sibling's.
    inbox: Inbox,
    /// Where the desk's `reply` handler stages the value a `ral` call returns,
    /// holding no `&mut Avatar` to write it any other way.  Within one batch
    /// the last write wins; [`Self::deliberate`] takes it once the batch
    /// drains and empties it on entry, so a reply a cancel or an error cut
    /// short never poisons the next deliberation.
    reply: ReplyCell,
    /// The fleet itself, `Arc`-shared with every other node: the launch, the
    /// name a spawn claims, and the lease scan.  Not the tree — that is
    /// [`Agent::parent`] and [`Agent::children`].
    pub(crate) fleet: Arc<Fleet>,
    /// What this avatar has measured and told about its context.
    readings: Readings,
    /// When the disk ceiling was last walked; `None` before the first walk.
    disk_checked: Option<Instant>,
}

/// What an avatar has measured and told about one context — the token
/// measure the gauges weigh, the ladders they have climbed, the nudges owed —
/// reborn whole with the context on `/clear`, since a rebuilt context has been
/// told nothing.
struct Readings {
    /// The provider's last input-token count and where in the log it landed:
    /// the numerator for the eviction trigger and the pressure gauge, `None`
    /// until a completion has reported one.
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
/// ([`crate::fleet::desk::ExarchDesk::launch`]), so a runaway spawn chain
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
    /// The one literal: every register starts empty, and the parent's
    /// envelope is minted now.  A `/clear` racing this only widens refusal —
    /// the stamp would fall one epoch further behind — so minting it unlocked
    /// is safe.
    pub(crate) fn new(birth: Birth) -> Arc<Self> {
        let Birth {
            id,
            name,
            log_dir,
            started,
            system,
            caps,
            parent,
            fuel,
            provider,
            returns,
            search,
            reach,
            mailbox,
        } = birth;
        let consumer = parent.as_ref().map(|p| p.mailbox.stamp());
        Arc::new(Self {
            id,
            name,
            log_dir,
            started,
            system,
            caps,
            parent,
            children: Mutex::new(Vec::new()),
            fuel,
            provider,
            token: cancel::Token::new(),
            search,
            returns,
            reach,
            mailbox,
            schedules: crate::fleet::schedule::ScheduleRegistry::new(),
            pins: Arc::default(),
            status: Mutex::new(Status {
                rest: None,
                reply: None,
                awaiting: BTreeSet::new(),
            }),
            consumer,
        })
    }

    pub(crate) fn current_provider(&self) -> Arc<Provider> {
        self.provider.current()
    }

    /// The whole run's directory — the one a session's `sessions/<id>/` hangs
    /// under, and so the one that holds everything a failed start left behind:
    /// the engine's captured output, the readable log, the lock.  It is what a
    /// failure invites a reader into, because a start that never reached a
    /// first turn has nothing in its session directory worth opening.
    ///
    /// Derived rather than stored: the layout is `<run>/sessions/<id>`, fixed
    /// by [`App::log_run_dir`](crate::bootstrap::App::log_run_dir) and by
    /// [`AgentLog`](crate::agent::log::AgentLog) between them, and a second
    /// copy of the path would be a second thing to keep true.  `None` only
    /// for a log rooted somewhere shallower than that shape, which is the
    /// test fixtures' business and not a run's.
    pub(crate) fn run_dir(&self) -> Option<&Path> {
        self.log_dir.parent()?.parent()
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

    /// Stage `reply` for this agent's consumer to fetch, then report
    /// [`AgentOutcome::Replied`] — deposit first, so a parent woken by the
    /// notice always finds the value already there.  A root reports to
    /// nobody; its driver reads the deposit off [`Self::reply`].
    pub(crate) fn deposit_reply(&self, reply: FOValue) {
        self.status.lock_ignore_poison().reply = Some(reply);
        self.report(AgentOutcome::Replied);
    }

    /// Deliver `outcome` to the parent's inbox as this agent's result.  A root
    /// has no parent and reports to nobody.
    pub(crate) fn report(&self, outcome: AgentOutcome) {
        let Some(consumer) = &self.consumer else {
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
        if to.parent.as_ref().is_some_and(|p| p.id == self.id) {
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

    /// Whether a human or parent has ever exchanged with this agent.
    pub(crate) fn engaged(&self) -> bool {
        self.mailbox.last_exchange().is_some()
    }

    /// Time since this agent's last human exchange, or since birth if never
    /// engaged.
    pub(crate) fn idle(&self) -> Duration {
        self.mailbox
            .last_exchange()
            .unwrap_or(self.started)
            .elapsed()
    }

    /// Cancel this agent across both terminate-class layers: the cooperative
    /// [`cancel::Token`] the attend loop polls and its engine's durable root.
    pub(crate) fn cancel(&self, cause: CancelCause) {
        self.token.cancel(cause);
        self.reach.terminate();
    }

    /// Unwind this agent's in-flight run without ending it: the Esc/Ctrl-C
    /// path, and the `` exarch-agents `cancel `` scoped verb's per-target primitive.
    pub(crate) fn interrupt(&self) {
        self.token.cancel(CancelCause::Interrupt);
        self.reach.interrupt();
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
        let mut above = target.parent.as_ref();
        while let Some(node) = above {
            if std::ptr::eq(Arc::as_ptr(node), std::ptr::from_ref(self)) {
                return Some(target.clone());
            }
            above = node.parent.as_ref();
        }
        None
    }

    /// The root of this agent's tree — itself, if it is one.  The trunk for
    /// anything the model spawned; a `/branch` tab is its own root, which is
    /// what keeps one tab's listing out of another's.
    pub(crate) fn root(self: &Arc<Self>) -> Arc<Self> {
        let mut here = self.clone();
        while let Some(up) = here.parent.clone() {
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
        self.cancel(cause);
        self.cancel_descendants(cause);
    }

    /// Cancel this agent's proper descendants, leaving it live — a settling
    /// parent abandoning its children, and `/clear`.  Both rebuild in place,
    /// so this must never stamp the root's own [`cancel::Token`]: a terminate
    /// cause there is permanent ([`cancel::Token::reset`] clears only a bare
    /// [`CancelCause::Interrupt`]) and every later run would fail.
    pub(crate) fn cancel_descendants(&self, cause: CancelCause) {
        for node in self.walk() {
            node.cancel(cause);
        }
    }

    /// `/clear`: abandon the subtree the rebuilt context no longer owns, and
    /// every register the model wrote into it — what it was awaiting, its
    /// wakeups, its pins.  The inbox fence is bumped by the drain in
    /// `Avatar::clear`, not here.
    pub(crate) fn forget(&self) {
        self.cancel_descendants(CancelCause::Explicit);
        self.status.lock_ignore_poison().awaiting.clear();
        self.schedules.clear();
        self.pins.lock_ignore_poison().clear();
    }
}

impl Avatar {
    /// This agent's wire identity — the public half's `id`, reachable without
    /// the `agent` field's crate-only visibility.
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

    /// Where this agent's own session log is written — `record.jsonl` and its
    /// siblings sit directly inside.
    pub fn log_dir(&self) -> PathBuf {
        self.agent.log_dir.clone()
    }

    /// The whole run's directory — the parent of the `sessions/` this agent's
    /// own log lives under, and where a front-end puts anything that belongs
    /// to the run rather than to one session: the engine's captured output,
    /// the lock.  See [`Agent::run_dir`] on why it is derived and when it is
    /// `None`.
    pub fn run_dir(&self) -> Option<PathBuf> {
        self.agent.run_dir().map(Path::to_path_buf)
    }

    /// Why no further frame will cross this agent's seat, if that has already
    /// happened.
    ///
    /// A front-end asks after an exchange that failed, because a severed
    /// engine is the one failure whose explanation is not in this process at
    /// all — it is wherever the engine was — and the front-end is the only
    /// party that knows how to go and fetch it while the corpse is still
    /// warm.  synod reaches its guest's console this way; an exchange that
    /// merely went badly answers `None` and nothing is fetched.
    pub fn severance(&self) -> Option<ral_core::protocol::Severed> {
        self.seat.severed()
    }

    /// Whether the trunk, refused until a reset, resumes there by a wakeup —
    /// the launch's say, for the trunk alone: a fork fails up to its parent.
    pub(crate) fn resumes_at_reset(&self) -> bool {
        self.agent.parent.is_none() && self.fleet.launch.resume_on_reset
    }

    /// So a frontend's emitters mint mailboxes onto the queue the attend loop
    /// is draining, rather than a second one.
    pub(crate) fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    /// Every deliberation must hand the session back at a ready boundary.
    ///
    /// # Panics
    /// Panics if the log cell is contended — see [`LogCell::lock`].
    pub fn is_ready(&self) -> bool {
        self.log.lock().context().is_ready()
    }

    /// The model-view messages the next request would carry.
    ///
    /// # Panics
    /// Panics if the log cell is contended — see [`LogCell::lock`].
    pub fn rendered_messages(&self) -> Vec<genai::chat::ChatMessage> {
        self.log.lock().context().rendered()
    }

    /// For a test polling an async spawn's settle without a full deliberation.
    #[cfg(test)]
    pub(crate) fn next_item_for_test(&self) -> Option<crate::bus::Next> {
        self.inbox.next_item()
    }
}

/// The message of a recovered panic payload, for either string shape.
pub(crate) fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
        .unwrap_or_else(|| "non-string payload".into())
}
