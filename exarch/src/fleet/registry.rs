//! The fleet's live agents — the trunk and every forked child alike — keyed by
//! [`AgentId`].
//!
//! Each entry names its parent, so this map is the spawn *tree*, and the
//! fleet is alive exactly while it is non-empty.  Entries are ephemeral: a
//! child's result goes to its parent's inbox, never collected here.
//!
//! Two cancel motions run over that tree and must not be confused.  *Terminate*
//! ends an agent and cascades to its subtree, tripping both the cooperative
//! [`Token`] its attend loop polls between steps and its session's durable
//! root, so an in-flight `ral` eval unwinds at ral's poll points rather than
//! grinding to its timeout wall — and, a worker's cancel scope being a
//! descendant of that root, reaching the agent's detached workers with no edge
//! of its own.  *Interrupt* cancels only the foreground scope the current run
//! installed, so the next run, minted fresh from the untouched root, is clean.
//!
//! The generation counter, bumped by `/clear`, is the fleet's one late-settle
//! fence: an async agent's result (`Agent::admits`) and a detached worker's
//! deferred batch (`InboxDeferred`) both admit against it.

use crate::agent::ProviderHandle;
use crate::agent::cancel::Token;
use crate::bus::{AgentId, AgentMessage, InboxReject, Mailbox, Post};
use crate::sync::LockExt;
use ral_core::process::{self, CancelCause, DurableRoot, ForegroundScope};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A live agent's current-run foreground scope, republished by its transport at
/// the start of every run
/// ([`ral_core::transport::IdentityTransport::observe_foreground`]).  It sits
/// outside the engine lock the run itself holds, so an interrupt can reach the
/// in-flight run from another thread.  `None` until the first run.
pub(crate) type RunScope = Arc<Mutex<Option<ForegroundScope>>>;

/// How the registry reaches one agent's running eval, per seat kind — the
/// eval-layer half of every cancel path, beside the cooperative [`Token`] each
/// entry also carries.
pub(crate) enum EvalReach {
    /// The in-process reach: the agent's own shell's durable root (terminate,
    /// and through the cancel-scope ancestor chain its detached workers too),
    /// and the run-scope cell its transport refreshes each run (interrupt).
    Identity {
        eval_root: DurableRoot,
        run_scope: RunScope,
    },
    /// The wire reach: the engine lives in another process, whose only
    /// host-reachable cancel primitive is a `Cancel` control frame against the
    /// in-flight dispatch, so both motions collapse into it.  Only a parented
    /// agent carries a reach and only an identity seat can be forked into, so
    /// nothing constructs this in production — but it must still answer.
    Wire(ral_core::transport::ControlSender),
}

impl EvalReach {
    /// Unwind the in-flight run without ending the agent.  Never touches the
    /// durable root, so the next run is born uncancelled.
    pub(crate) fn interrupt(&self) {
        match self {
            Self::Identity { run_scope, .. } => {
                if let Some(scope) = run_scope.lock_ignore_poison().as_ref() {
                    scope.cancel(CancelCause::Interrupt);
                }
            }
            Self::Wire(ctrl) => ctrl.cancel_in_flight(),
        }
    }

    /// End the agent's eval by cancelling its session's durable root.  That
    /// poisons the root permanently, by design: every caller is ending the
    /// agent, so there is no later run to break.
    pub(crate) fn terminate(&self, cause: CancelCause) {
        match self {
            Self::Identity { eval_root, .. } => eval_root.cancel(cause),
            Self::Wire(ctrl) => ctrl.cancel_in_flight(),
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

/// One live agent, for the `agents` listing.
pub struct AgentInfo {
    pub name: String,
    pub log_dir: PathBuf,
    pub elapsed: Duration,
}

/// Threaded once at birth into [`AgentRegistry::register`].
pub struct Registration {
    pub id: AgentId,
    /// `None` for the trunk.
    pub parent: Option<AgentId>,
    /// The idle lease to arm over this agent's subtree, or `None` for none.
    /// Duration-typed rather than a bare flag so a test can arm a millisecond
    /// lease instead of waiting out [`AGENT_LEASE_IDLE`].
    pub lease: Option<Duration>,
    pub name: String,
    pub log_dir: PathBuf,
    pub cancel: Token,
    /// `None` for the trunk, whose session outlives any cancel: an Esc reaches
    /// its *run* through the published foreground slot, never its eval layer.
    pub(crate) reach: Option<EvalReach>,
    pub mailbox: Mailbox,
    pub provider: ProviderHandle,
}

/// The fleet's live agents.
///
/// Every clone shares the one map, so the trunk, each forked child, and the
/// frontend hold the same registry — and a detached worker keeps a handle it
/// can settle through after its exchange has ended.
#[derive(Clone)]
pub struct AgentRegistry {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// Bumped by [`AgentRegistry::clear_subtree`]; a worker that settles under
    /// an older generation is rejected.
    generation: u64,
    entries: HashMap<AgentId, Entry>,
}

struct Entry {
    /// `None` for the trunk — the edge that makes this map a tree.
    parent: Option<AgentId>,
    name: String,
    log_dir: PathBuf,
    started: Instant,
    cancel: Token,
    /// A tripped [`Token`] alone cannot stop an in-flight `ral` tool call —
    /// `shell_eval`'s `run_shell` blocks until the engine reports — so both
    /// cancel motions also go through this.  `None` for the trunk.
    reach: Option<EvalReach>,
    mailbox: Mailbox,
    /// Hot-swappable, so a `/model` on the focused agent swaps *its* provider
    /// and disturbs no other.
    provider: ProviderHandle,
    /// [`AgentRegistry::register`] arms the first fire from this; each fire
    /// re-reads it by id rather than closing over a copy, so a removed entry
    /// ends the chain simply by not being found.
    lease: Option<Duration>,
    /// The last human exchange, or `None` before the first one — `started` is
    /// the epoch until then.  [`AgentRegistry::renew`] is the only writer.
    last_exchange: Option<Instant>,
}

impl Entry {
    /// Time since the last human exchange, or since birth if never engaged.
    fn idle(&self) -> Duration {
        self.last_exchange.unwrap_or(self.started).elapsed()
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
                generation: 0,
                entries: HashMap::new(),
            })),
        }
    }

    /// Captured by a worker at birth, so a result arriving after a `/clear` can
    /// be rejected.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// The fleet's liveness: no entries, no fleet.
    pub fn is_empty(&self) -> bool {
        self.lock().entries.is_empty()
    }

    /// The park signal for a node that launched async agents: a child delivers
    /// only to its own parent's inbox, so a live direct child is exactly what
    /// keeps this mailbox from being permanently empty.
    pub fn has_children(&self, parent: AgentId) -> bool {
        self.lock()
            .entries
            .values()
            .any(|e| e.parent == Some(parent))
    }

    /// A conversing agent reads this at each park: once its entry is reaped
    /// (`/clear`, `/close`) it must quiesce rather than park Held as a zombie.
    pub fn is_live(&self, id: AgentId) -> bool {
        self.lock().entries.contains_key(&id)
    }

    /// Cancel and reap `root`'s proper descendants, leaving `root` live — a
    /// settling parent abandoning its children, and `/clear` on the trunk.
    /// Both rebuild in place, so the reap must never stamp the root's own
    /// [`Token`]: a terminate cause there is permanent ([`Token::reset`] clears
    /// only a bare [`CancelCause::Interrupt`]) and every later run would fail.
    pub fn cancel_descendants(&self, root: AgentId) {
        let mut g = self.lock();
        cancel_and_remove(&mut g, root, false);
    }

    /// Register an agent, returning the birth generation it carries into its
    /// result.  A lease is the caller's choice, not a consequence of being
    /// parented: a detached worker gets one (abandoned, it must be reaped), a
    /// branch does not (a conversation must not lose an exchange at the hour
    /// mark).
    ///
    /// # Errors
    /// [`RegisterError::SessionDead`] when `parent` is not, at this instant, a
    /// live and un-terminated entry — a spawn racing a cancel on its own
    /// parent.  The check runs under the very lock the cascade holds for its
    /// whole walk, so it never sees a parent mid-teardown.
    ///
    /// [`RegisterError::NameTaken`] when another live entry bears the name.
    /// Also under that lock, so two same-name spawns racing within one turn
    /// cannot both succeed — the authoritative half of the spawn-uniqueness
    /// rule, [`Self::name_live`] the cheap one.
    pub fn register(&self, reg: Registration) -> Result<u64, RegisterError> {
        let Registration {
            id,
            parent,
            lease,
            name,
            log_dir,
            cancel,
            reach,
            mailbox,
            provider,
        } = reg;
        let mut g = self.lock();
        if let Some(p) = parent
            && g.entries.get(&p).is_none_or(|e| e.cancel.terminated())
        {
            drop(g);
            return Err(RegisterError::SessionDead);
        }
        // Excludes `id`'s own current entry: `Agent::register_self` re-registers
        // under the same id and must overwrite in place, not be refused for
        // colliding with the very entry it is about to replace.
        if g.entries
            .iter()
            .any(|(&other, e)| other != id && e.name == name)
        {
            drop(g);
            return Err(RegisterError::NameTaken(name));
        }
        g.entries.insert(
            id,
            Entry {
                parent,
                name,
                log_dir,
                started: Instant::now(),
                cancel,
                reach,
                mailbox,
                provider,
                lease,
                last_exchange: None,
            },
        );
        // Armed only after the entry exists: `lease_fire` re-reads the entry by
        // id, so arming earlier would let the first fire find nothing and end
        // the chain before it started.
        if let Some(ttl) = lease {
            let reg = self.clone();
            process::arm_callback(ttl, move || lease_fire(&reg, id)).keep();
        }
        Ok(g.generation)
    }

    /// The cheap half of the spawn-uniqueness rule [`Self::register`] enforces
    /// authoritatively: `agent-start` reads this before forking a nursery
    /// session, so the ordinary duplicate refuses without a fork to unwind.
    pub fn name_live(&self, name: &str) -> bool {
        self.lock().entries.values().any(|e| e.name == name)
    }

    /// At most one entry can match, since [`Self::register`] keeps names unique
    /// among the live.  `agent-cancel` and `message` come through here before
    /// reaching the id-scoped primitives.
    pub fn resolve_name(&self, name: &str) -> Option<AgentId> {
        let g = self.lock();
        g.entries
            .iter()
            .find(|(_, e)| e.name == name)
            .map(|(id, _)| *id)
    }

    /// The live agent's inbox sender, or `None` once it has settled or been
    /// cleared.  The frontend reads it to decide whether a tab is steerable at
    /// all; [`Self::steer`] is the delivery door.
    pub fn mailbox(&self, id: AgentId) -> Option<Mailbox> {
        let g = self.lock();
        g.entries.get(&id).map(|e| e.mailbox.clone())
    }

    /// Deliver a human message to `id`: the registry's one steering door.
    /// Renews before pushing, so the woken agent's park verdict — recomputed
    /// under its inbox mutex before it pops — already reads `engaged`; and
    /// pushes after the guard drops, as the inbox → registry lock order
    /// requires.  `false` once the entry is gone, and the line is dropped.
    pub fn steer(&self, id: AgentId, text: String) -> bool {
        let mailbox = {
            let mut g = self.lock();
            let Some(e) = g.entries.get_mut(&id) else {
                return false;
            };
            renew_entry(e);
            let mailbox = e.mailbox.clone();
            drop(g);
            mailbox
        };
        mailbox.push_user(text);
        true
    }

    /// Stamp `id`'s exchange clock, without a mailbox delivery.  Neither
    /// focusing, nor [`Self::message`], nor a `/resources` probe renews:
    /// attention alone must never immortalise a child.
    pub fn renew(&self, id: AgentId) -> bool {
        self.lock().entries.get_mut(&id).map(renew_entry).is_some()
    }

    /// `false` for an entry never renewed, and for one already gone.
    pub fn engaged(&self, id: AgentId) -> bool {
        self.lock()
            .entries
            .get(&id)
            .is_some_and(|e| e.last_exchange.is_some())
    }

    /// Time since `id`'s last human exchange, or since birth if never engaged.
    /// `None` once the entry is gone.
    pub fn idle(&self, id: AgentId) -> Option<Duration> {
        let g = self.lock();
        g.entries.get(&id).map(Entry::idle)
    }

    /// The nearest time-to-reap across the leased entries, `None` when none
    /// carries a lease.  A pure survey for `/resources`: it renews nothing.
    pub fn nearest_reap(&self) -> Option<Duration> {
        let g = self.lock();
        g.entries
            .values()
            .filter_map(|e| {
                let ttl = e.lease?;
                Some(ttl.saturating_sub(e.idle()))
            })
            .min()
    }

    /// `(idle, ttl)` for a live leased entry, read by [`lease_fire`].  `None`
    /// once the entry is gone or if it carries no lease — either way the chain
    /// has nothing left to arm.
    fn lease_state(&self, id: AgentId) -> Option<(Duration, Duration)> {
        self.lock().entries.get(&id).and_then(|e| {
            let ttl = e.lease?;
            Some((e.idle(), ttl))
        })
    }

    /// Send a marked model-visible message to a proper descendant of `from` —
    /// the same scoping [`Self::cancel_scoped`] enforces.  The mailbox is
    /// cloned under the registry lock and posted after it drops, because a park
    /// verdict reads this registry under the consumer's inbox mutex and the
    /// process-wide order is inbox → registry.
    ///
    /// # Errors
    /// A dead sender or recipient, a scope violation, or a recipient at quota.
    pub fn message(&self, from: AgentId, to: AgentId, text: String) -> Result<(), MessageError> {
        let (mailbox, from_name) = {
            let g = self.lock();
            let from_name = g
                .entries
                .get(&from)
                .ok_or(MessageError::UnknownSender(from))?
                .name
                .clone();
            let mailbox = g
                .entries
                .get(&to)
                .ok_or(MessageError::UnknownRecipient(to))?
                .mailbox
                .clone();
            if !is_descendant_of(&g.entries, from, to) {
                return Err(MessageError::NotADescendant(to));
            }
            drop(g);
            (mailbox, from_name)
        };
        mailbox
            .push(Post::AgentMessage(AgentMessage {
                from,
                from_name,
                text,
            }))
            .map_err(|reject| MessageError::RecipientInboxFull(to, reject))
    }

    /// For a `/model` swap on the focused agent.  `None` once it has settled.
    pub fn provider(&self, id: AgentId) -> Option<ProviderHandle> {
        let g = self.lock();
        g.entries.get(&id).map(|e| e.provider.clone())
    }

    /// Settle a worker born under `generation`.  The entry is reaped either way
    /// — a worker is authoritative over its *own* lifetime, so a stale one can
    /// never leave a zombie behind — while the return admits the result only
    /// when `generation` is still current.
    pub fn settle(&self, id: AgentId, generation: u64) -> bool {
        let mut g = self.lock();
        let current = g.generation;
        let existed = g.entries.remove(&id).is_some();
        drop(g);
        existed && current == generation
    }

    /// The trunk's self-removal at the end of its attend; a child leaves
    /// through [`Self::settle`] instead.
    pub fn deregister(&self, id: AgentId) {
        self.lock().entries.remove(&id);
    }

    /// Cancel an agent and its whole subtree, unscoped; `true` if it existed.
    /// The entries stay in the map — only [`Self::clear_subtree`] and
    /// [`Self::remove_subtree`] reap.  `pub(crate)` because it trusts the
    /// caller to have decided `id` is legitimate, which the model-facing
    /// `agent-cancel` earns through [`Self::cancel_scoped`].
    pub(crate) fn cancel(&self, id: AgentId) -> bool {
        self.cancel_cause(id, CancelCause::Explicit)
    }

    /// The cascade proper, parameterised on the cause so an expired lease
    /// reports [`CancelCause::Deadline`] ("timed out") rather than
    /// [`CancelCause::Explicit`] ("cancelled").
    fn cancel_cause(&self, id: AgentId, cause: CancelCause) -> bool {
        let g = self.lock();
        let existed = g.entries.contains_key(&id);
        for d in descendants(&g.entries, id, true) {
            if let Some(e) = g.entries.get(&d) {
                terminate_entry(e, cause);
            }
        }
        existed
    }

    /// `agent-cancel`'s model-facing entry point: [`Self::cancel`], but only
    /// down `caller`'s own subtree — an agent may stop what it started, never a
    /// sibling, an ancestor, or itself.  An unknown `target` is `Ok(false)`,
    /// the tool's no-op report, not an error.
    ///
    /// # Errors
    /// [`NotADescendant`] when `target` is live but out of `caller`'s scope.
    pub fn cancel_scoped(&self, caller: AgentId, target: AgentId) -> Result<bool, NotADescendant> {
        let scoped = {
            let g = self.lock();
            if !g.entries.contains_key(&target) {
                return Ok(false);
            }
            is_descendant_of(&g.entries, caller, target)
        };
        if !scoped {
            return Err(NotADescendant(target));
        }
        Ok(self.cancel(target))
    }

    /// Where Esc and Ctrl-C on a focused sub-agent tab land: unwind exactly
    /// this entry's in-flight run, cascading to no descendant and removing
    /// nothing.  The agent drops the run and re-parks; its next one is clean.
    pub fn interrupt(&self, id: AgentId) {
        let g = self.lock();
        if let Some(e) = g.entries.get(&id) {
            interrupt_entry(e);
        }
    }

    /// The `agents` listing: `ancestor`'s live proper descendants at any depth,
    /// ordered by id.  An agent lists what it spawned, never itself.
    pub fn list(&self, ancestor: AgentId) -> Vec<AgentInfo> {
        let mut rows: Vec<(AgentId, AgentInfo)> = {
            let g = self.lock();
            descendants(&g.entries, ancestor, false)
                .into_iter()
                .filter_map(|id| {
                    g.entries.get(&id).map(|e| {
                        (
                            id,
                            AgentInfo {
                                name: e.name.clone(),
                                log_dir: e.log_dir.clone(),
                                elapsed: e.started.elapsed(),
                            },
                        )
                    })
                })
                .collect()
        };
        rows.sort_unstable_by_key(|(id, _)| *id);
        rows.into_iter().map(|(_, info)| info).collect()
    }

    /// `/clear` on `root`: reap the subtree the rebuilt context no longer owns,
    /// and bump the generation so any late result from the old one is rejected.
    /// `root` is being rebuilt, not torn down, so it stays registered.
    pub fn clear_subtree(&self, root: AgentId) {
        let mut g = self.lock();
        cancel_and_remove(&mut g, root, false);
        g.generation += 1;
    }

    /// `/close`: the inclusive twin of the `/clear` reap, taking `root` itself
    /// with the subtree.  No generation bump — a closed branch pushes no
    /// result, so there is nothing to fence.
    pub fn remove_subtree(&self, root: AgentId) {
        let mut g = self.lock();
        cancel_and_remove(&mut g, root, true);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock_ignore_poison()
    }
}

/// Why [`AgentRegistry::register`] refused a registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// The parent vanished or was terminated out from under a racing spawn.
    SessionDead,
    /// Carries the offending name, so the caller can refuse didactically.
    NameTaken(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageError {
    UnknownSender(AgentId),
    UnknownRecipient(AgentId),
    NotADescendant(AgentId),
    /// The recipient's inbox is at quota; surfaced to the `message` tool's
    /// caller rather than dropped silently.
    RecipientInboxFull(AgentId, InboxReject),
}

/// The scoping violation `agent-cancel` and `message` share: each takes a
/// target id but may only reach a proper descendant of the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotADescendant(pub AgentId);

/// Walks `id`'s parent chain upward rather than scanning `ancestor`'s subtree,
/// so a scoping check costs one climb, not a tree-wide search.  Starting at
/// `id`'s *parent* is what makes nothing a proper descendant of itself.
fn is_descendant_of(entries: &HashMap<AgentId, Entry>, ancestor: AgentId, id: AgentId) -> bool {
    let mut cur = id;
    while let Some(parent) = entries.get(&cur).and_then(|e| e.parent) {
        if parent == ancestor {
            return true;
        }
        cur = parent;
    }
    false
}

/// The ids in `ancestor`'s subtree at any depth, `inclusive` adding `ancestor`
/// itself.  Builds the child adjacency once rather than rescanning the whole
/// map at every frontier pop.
fn descendants(
    entries: &HashMap<AgentId, Entry>,
    ancestor: AgentId,
    inclusive: bool,
) -> Vec<AgentId> {
    let mut children_of: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
    for (id, e) in entries {
        if let Some(p) = e.parent {
            children_of.entry(p).or_default().push(*id);
        }
    }
    let mut out = if inclusive {
        vec![ancestor]
    } else {
        Vec::new()
    };
    let mut frontier = vec![ancestor];
    while let Some(cur) = frontier.pop() {
        if let Some(kids) = children_of.get(&cur) {
            for &k in kids {
                out.push(k);
                frontier.push(k);
            }
        }
    }
    out
}

/// Cancel and drop a subtree, `inclusive` deciding whether `root` goes with it.
/// Every victim is cancelled across both layers *before* any removal, so an
/// in-flight eval unwinds instead of grinding on as an orphan.
fn cancel_and_remove(g: &mut Inner, root: AgentId, inclusive: bool) {
    let victims = descendants(&g.entries, root, inclusive);
    for d in &victims {
        if let Some(e) = g.entries.get(d) {
            terminate_entry(e, CancelCause::Explicit);
        }
    }
    for d in victims {
        g.entries.remove(&d);
    }
}

/// One firing of an agent's idle lease, on the reaper daemon thread.  A gone or
/// unleased entry ends the chain silently, and one still short of its bound
/// re-arms for the remaining margin — [`AgentRegistry::renew`] never touches
/// the reaper, so this lazy re-arm is the whole mechanism.
fn lease_fire(reg: &AgentRegistry, id: AgentId) {
    let Some((idle, ttl)) = reg.lease_state(id) else {
        return;
    };
    if idle >= ttl {
        reg.cancel_cause(id, CancelCause::Deadline);
    } else {
        let reg = reg.clone();
        process::arm_callback(ttl.checked_sub(idle).unwrap(), move || lease_fire(&reg, id)).keep();
    }
}

/// The one write [`AgentRegistry::renew`] and [`AgentRegistry::steer`] share,
/// each under its own lock scope, shaped by what else it does there.
fn renew_entry(e: &mut Entry) {
    e.last_exchange = Some(Instant::now());
}

/// Cancel one entry across both terminate-class layers: the cooperative
/// [`Token`] the attend loop polls between steps and, for a parented agent, its
/// eval layer.
fn terminate_entry(e: &Entry, cause: CancelCause) {
    e.cancel.cancel(cause);
    if let Some(reach) = &e.reach {
        reach.terminate(cause);
    }
}

/// Unwind one entry's in-flight run without ending the agent.  Neither layer's
/// terminate side is touched, so the next run is born clean.
fn interrupt_entry(e: &Entry) {
    e.cancel.cancel(CancelCause::Interrupt);
    if let Some(reach) = &e.reach {
        reach.interrupt();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> ProviderHandle {
        ProviderHandle::new(std::sync::Arc::new(crate::provider::Provider::scripted(
            "test",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new(),
        )))
    }

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

    /// Register `id` under `parent`, returning the birth generation.
    fn entry(reg: &AgentRegistry, id: AgentId, parent: Option<AgentId>) -> u64 {
        reg.register(Registration {
            id,
            parent,
            lease: parent.is_some().then_some(AGENT_LEASE_IDLE),
            name: format!("a{id}"),
            log_dir: PathBuf::from("/tmp"),
            cancel: Token::new(),
            reach: parent.map(|_| EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: crate::bus::Inbox::new().mailbox(),
            provider: provider(),
        })
        .expect("a fresh trunk, or a child of one, always registers")
    }

    #[test]
    fn settle_delivers_for_current_generation() {
        let reg = AgentRegistry::new();
        let g = entry(&reg, 1, None);
        assert!(reg.settle(1, g), "a current-generation worker delivers");
        assert!(!reg.settle(1, g), "a settled entry is gone");
    }

    /// A lease is an explicit knob, not a consequence of being parented.
    /// Asserted behaviourally, since there is no guard field to peek at: the
    /// reap only stamps the cancel layers, and a live attend loop — absent
    /// here — is what would go on to settle the entry out of the map.
    #[test]
    fn register_arms_a_lease_only_when_asked() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk: a live parent for 1 and 2
        let branch_token = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None,
            name: "branch".into(),
            log_dir: PathBuf::from("/tmp"),
            cancel: branch_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let worker_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(0),
            lease: Some(Duration::from_millis(50)),
            name: "worker".into(),
            log_dir: PathBuf::from("/tmp"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: worker_root.clone(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        assert!(
            eventually(Duration::from_secs(2), || worker_root
                .as_scope()
                .is_cancelled()),
            "lease: Some(short) is reaped once its bound elapses"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !branch_token.is_cancelled(),
            "lease: None arms no reaper deadline"
        );
    }

    #[test]
    fn clear_subtree_bumps_generation_and_rejects_late_results() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let g = entry(&reg, 1, Some(0)); // its child
        reg.clear_subtree(0);
        assert!(
            !reg.settle(1, g),
            "a result from before /clear is rejected by generation"
        );
        assert!(reg.mailbox(1).is_none(), "the cleared child is reaped");
        assert!(reg.mailbox(0).is_some(), "the cleared agent itself stays");
        assert_ne!(g, reg.generation(), "the generation advanced on clear");
    }

    /// Both layers, so an in-flight eval unwinds instead of grinding on as an
    /// orphan whose result nobody will collect.
    #[test]
    fn clear_subtree_cancels_descendant_eval_roots() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let token = Token::new();
        let eval_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "worker".into(),
            log_dir: PathBuf::from("/tmp/worker"),
            cancel: token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        reg.clear_subtree(0);
        assert!(token.is_cancelled(), "the reaped worker's token is set");
        assert!(
            eval_root.as_scope().is_cancelled(),
            "the reap cancels the worker's eval layer, not just its token"
        );
    }

    /// The invariant the `/clear` UI handler leans on.  A sticky [`Token`]
    /// never forgets a terminate-class cause, so no gesture that leaves the
    /// trunk live may stamp one on the trunk's own token;
    /// [`AgentRegistry::cancel`]'s inclusive cascade would, and every later run
    /// would fail forever.
    #[test]
    fn clear_gesture_reaps_descendants_but_spares_the_trunks_token() {
        let reg = AgentRegistry::new();
        let trunk_token = Token::new();
        let _ = reg.register(Registration {
            id: 0,
            parent: None,
            lease: None,
            name: "trunk".into(),
            log_dir: PathBuf::from("/tmp/trunk"),
            cancel: trunk_token.clone(),
            reach: None,
            mailbox: mb(),
            provider: provider(),
        });
        let child_token = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: child_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        reg.cancel_descendants(0);

        assert!(
            child_token.terminated(),
            "the descendant's token is terminate-stamped"
        );
        assert!(!reg.is_live(1), "the descendant's entry is reaped");
        assert!(reg.is_live(0), "the trunk's entry survives the gesture");
        assert!(
            !trunk_token.is_cancelled(),
            "cancel_descendants never touches the trunk's own token"
        );

        // A stamped terminate cause would survive this round trip and an
        // uncancelled token does not, so this proves the trunk carries no
        // terminate cause at all — not merely that this call skipped it.
        trunk_token.cancel(CancelCause::Interrupt);
        trunk_token.reset();
        assert!(
            !trunk_token.is_cancelled(),
            "the trunk's token round-trips through an interrupt and reset \
             uncancelled, so the next run after /clear would run"
        );
    }

    /// Where the `/clear` reap would have left the root live.
    #[test]
    fn remove_subtree_cancels_and_removes_the_root_too() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let branch_token = Token::new();
        let branch_root = DurableRoot::default();
        let child_token = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None, // a branch carries no lease
            name: "branch".into(),
            log_dir: PathBuf::from("/tmp/branch"),
            cancel: branch_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: branch_root.clone(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "grandchild".into(),
            log_dir: PathBuf::from("/tmp/gc"),
            cancel: child_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        reg.remove_subtree(1);

        assert!(
            branch_token.is_cancelled(),
            "the closed branch's token is set"
        );
        assert!(
            branch_root.as_scope().is_cancelled(),
            "close reaches the branch's eval layer, not just its token"
        );
        assert!(child_token.is_cancelled(), "a spawned descendant cascades");
        assert!(
            !reg.is_live(1),
            "the branch itself is gone, unlike a /clear reap"
        );
        assert!(!reg.is_live(2), "and so is its descendant");
        assert!(
            reg.is_live(0),
            "the trunk above the closed subtree survives"
        );
    }

    /// An interrupt drops the run without ending the agent, so the child's
    /// token is cancelled but not `terminated`, and it walks no descendants and
    /// deregisters nothing.  Contrast `cancel(1)`, which would trip the
    /// grandchild too, and with a terminate cause.
    #[test]
    fn interrupt_unwinds_exactly_one_entry() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let child_token = Token::new();
        let child_root = DurableRoot::default();
        // What `IdentityTransport::observe_foreground` would write at the start
        // of the child's in-flight run.
        let child_run_scope: RunScope = Arc::new(Mutex::new(Some(
            child_root.foreground(&child_root.worker()),
        )));
        let grandchild_token = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None,
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: child_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: child_root.clone(),
                run_scope: child_run_scope.clone(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "grandchild".into(),
            log_dir: PathBuf::from("/tmp/gc"),
            cancel: grandchild_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        reg.interrupt(1);

        assert!(
            child_token.is_cancelled(),
            "interrupt trips the child's token"
        );
        assert!(
            !child_token.terminated(),
            "an interrupt is not a terminate cause"
        );
        assert!(
            child_run_scope
                .lock()
                .unwrap()
                .as_ref()
                .expect("the cell holds the in-flight run's scope")
                .is_cancelled(),
            "the interrupt reaches whatever scope the run-scope cell holds"
        );
        assert!(
            !child_root.as_scope().is_cancelled(),
            "eval_root itself is never touched by an interrupt"
        );
        assert!(
            !grandchild_token.is_cancelled(),
            "no descendant walk: the grandchild is untouched"
        );
        assert!(reg.is_live(1), "interrupt never deregisters the entry");
        assert!(reg.is_live(2), "nor its descendant");
    }

    /// An interrupted agent's *next* run must run uncancelled: the interrupt
    /// reaches only the run-scope cell, never `eval_root`, so a foreground
    /// scope freshly minted from the root starts clean.
    #[test]
    fn interrupt_never_poisons_the_next_run() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let eval_root = DurableRoot::default();
        let run_scope: RunScope =
            Arc::new(Mutex::new(Some(eval_root.foreground(&eval_root.worker()))));
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None,
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                run_scope: run_scope.clone(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        reg.interrupt(1);
        assert!(
            run_scope.lock().unwrap().as_ref().unwrap().is_cancelled(),
            "the in-flight run did unwind"
        );

        // The next run mints a fresh scope from the same `eval_root` and the
        // transport republishes it into the same cell, as
        // `IdentityTransport::observe_foreground` does at every dispatch.
        let next_run = eval_root.foreground(&eval_root.worker());
        *run_scope.lock().unwrap() = Some(next_run.clone());

        assert!(
            !next_run.is_cancelled(),
            "the next run is born uncancelled — the interrupt never poisoned eval_root"
        );
    }

    #[test]
    fn cancel_sets_the_token_and_list_reports_the_subtree() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let token = Token::new();
        let eval_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 7,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "lint".into(),
            log_dir: PathBuf::from("/log/7"),
            cancel: token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                run_scope: RunScope::default(),
            }),
            mailbox: crate::bus::Inbox::new().mailbox(),
            provider: provider(),
        });
        assert_eq!(reg.list(0).len(), 1, "the trunk lists its one child");
        assert_eq!(reg.list(0)[0].name, "lint");
        assert!(reg.cancel(7), "an existing agent is cancellable");
        assert!(token.is_cancelled(), "cancel sets the worker's token");
        assert!(
            eval_root.as_scope().is_cancelled(),
            "cancel reaches the worker's eval layer through its session root"
        );
        assert!(!reg.cancel(99), "an unknown id is not cancellable");
    }

    #[test]
    fn cancel_cascades_to_the_whole_subtree() {
        let reg = AgentRegistry::new();
        let r = Token::new();
        let c = Token::new();
        let g = Token::new();
        let sibling = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: None,
            lease: None,
            name: "r".into(),
            log_dir: "/l".into(),
            cancel: r.clone(),
            reach: None,
            mailbox: mb(),
            provider: provider(),
        });
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "c".into(),
            log_dir: "/l".into(),
            cancel: c.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let _ = reg.register(Registration {
            id: 3,
            parent: Some(2),
            lease: Some(AGENT_LEASE_IDLE),
            name: "g".into(),
            log_dir: "/l".into(),
            cancel: g.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let _ = reg.register(Registration {
            id: 4,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "s".into(),
            log_dir: "/l".into(),
            cancel: sibling.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert!(reg.cancel(2));
        assert!(c.is_cancelled(), "the cancelled node");
        assert!(g.is_cancelled(), "its descendant cascades");
        assert!(!sibling.is_cancelled(), "a sibling is untouched");
        assert!(!r.is_cancelled(), "the parent is untouched");
    }

    #[test]
    fn mailbox_resolves_only_while_an_agent_is_live() {
        let reg = AgentRegistry::new();
        let g = entry(&reg, 1, None);
        assert!(
            reg.mailbox(1).is_some(),
            "a live entry hands out its sender"
        );
        assert!(reg.mailbox(99).is_none(), "an unknown id has no sender");
        assert!(reg.settle(1, g));
        assert!(reg.mailbox(1).is_none(), "a settled entry hands out none");
    }

    #[test]
    fn message_posts_a_marked_note_to_a_live_agent() {
        let reg = AgentRegistry::new();
        entry(&reg, 1, None);
        let inbox = crate::bus::Inbox::new();
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "worker".into(),
            log_dir: PathBuf::from("/tmp/worker"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: inbox.mailbox(),
            provider: provider(),
        });

        reg.message(1, 2, "check the lexer".into())
            .expect("message to a live agent succeeds");
        let Some(crate::bus::Item::Message(msg)) = inbox.next_item() else {
            panic!("expected a peer message");
        };
        assert_eq!(msg.from, 1);
        assert_eq!(msg.from_name, "a1");
        assert_eq!(msg.text, "check the lexer");
    }

    #[test]
    fn message_reports_unknown_sender_or_recipient() {
        let reg = AgentRegistry::new();
        entry(&reg, 1, None);
        assert_eq!(
            reg.message(1, 99, "hello".into()),
            Err(MessageError::UnknownRecipient(99))
        );
        assert_eq!(
            reg.message(99, 1, "hello".into()),
            Err(MessageError::UnknownSender(99))
        );
    }

    /// An ancestor, a sibling, and the sender's own id are all refused, even
    /// though all three are live agents the existence checks alone would admit.
    #[test]
    fn message_refuses_self_and_non_descendant_targets() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        entry(&reg, 1, Some(0)); // trunk's child
        entry(&reg, 2, Some(0)); // trunk's other child, 1's sibling

        assert_eq!(
            reg.message(1, 1, "hi".into()),
            Err(MessageError::NotADescendant(1)),
            "a self-message is refused"
        );
        assert_eq!(
            reg.message(1, 2, "hi".into()),
            Err(MessageError::NotADescendant(2)),
            "a sibling is refused"
        );
        assert_eq!(
            reg.message(1, 0, "hi".into()),
            Err(MessageError::NotADescendant(0)),
            "an ancestor is refused"
        );

        let inbox = crate::bus::Inbox::new();
        let _ = reg.register(Registration {
            id: 3,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/3"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: inbox.mailbox(),
            provider: provider(),
        });
        assert!(
            reg.message(1, 3, "ok".into()).is_ok(),
            "a proper descendant is reachable"
        );
    }

    #[test]
    fn cancel_scoped_reaches_only_a_proper_descendant() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None,
            name: "leaf".into(),
            log_dir: PathBuf::from("/tmp/1"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let sibling_token = Token::new();
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(0),
            lease: None,
            name: "sibling".into(),
            log_dir: PathBuf::from("/tmp/2"),
            cancel: sibling_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let grandchild_token = Token::new();
        let _ = reg.register(Registration {
            id: 3,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "grandchild".into(),
            log_dir: PathBuf::from("/tmp/3"),
            cancel: grandchild_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        assert_eq!(
            reg.cancel_scoped(1, 0),
            Err(NotADescendant(0)),
            "a leaf cannot cancel an ancestor (the trunk)"
        );
        assert_eq!(
            reg.cancel_scoped(1, 2),
            Err(NotADescendant(2)),
            "nor a sibling"
        );
        assert_eq!(
            reg.cancel_scoped(1, 1),
            Err(NotADescendant(1)),
            "nor itself"
        );
        assert!(
            !sibling_token.is_cancelled(),
            "none of the refused calls touched anything"
        );

        assert_eq!(
            reg.cancel_scoped(0, 3),
            Ok(true),
            "the trunk can cancel its own grandchild"
        );
        assert!(grandchild_token.is_cancelled());

        assert_eq!(
            reg.cancel_scoped(0, 999),
            Ok(false),
            "an unknown id is reported, not an error"
        );
    }

    /// The tail of a spawn racing a `/clear` or `agent-cancel` on its parent is
    /// refused outright, rather than landing orphaned and uncancellable.
    #[test]
    fn register_refuses_when_the_parent_is_gone() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let outcome = reg.register(Registration {
            id: 1,
            parent: Some(99), // never registered
            lease: None,
            name: "orphan".into(),
            log_dir: PathBuf::from("/tmp/orphan"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert_eq!(
            outcome,
            Err(RegisterError::SessionDead),
            "a parent absent from the map refuses the registration"
        );
        assert!(!reg.is_live(1), "the refused entry is never inserted");
    }

    /// The other half: a cancelled but not yet self-settled parent refuses a
    /// new child exactly as an absent one would.  The bare cascade never
    /// removes entries, so "absent from the map" alone would miss this window.
    #[test]
    fn register_refuses_when_the_parent_is_already_terminated() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let parent_token = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None,
            name: "parent".into(),
            log_dir: PathBuf::from("/tmp/parent"),
            cancel: parent_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        parent_token.cancel(CancelCause::Explicit);
        assert!(reg.is_live(1), "the cancelled parent's entry still lingers");

        let outcome = reg.register(Registration {
            id: 2,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "late-child".into(),
            log_dir: PathBuf::from("/tmp/late-child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert_eq!(
            outcome,
            Err(RegisterError::SessionDead),
            "a parent already carrying a terminate cause refuses new children"
        );
        assert!(!reg.is_live(2));
    }

    /// Two same-name registrations never both succeed, even with nothing
    /// settled between them: the desk's [`AgentRegistry::name_live`] pre-check
    /// is the cheap half, this the check that closes the race.
    #[test]
    fn register_refuses_a_name_already_borne_by_a_live_entry() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "helper".into(),
            log_dir: PathBuf::from("/tmp/helper"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert!(reg.name_live("helper"), "the first spawn's name is live");

        let outcome = reg.register(Registration {
            id: 2,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "helper".into(),
            log_dir: PathBuf::from("/tmp/helper-2"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert_eq!(
            outcome,
            Err(RegisterError::NameTaken("helper".to_string())),
            "a second live agent may not bear the same name"
        );
        assert!(
            !reg.is_live(2),
            "the refused registration is never inserted"
        );
        assert_eq!(
            reg.resolve_name("helper"),
            Some(1),
            "the name still resolves to the entry that actually holds it"
        );
    }

    /// A reaped agent and an `agent-cancel`ed one share one cascade,
    /// distinguished only by the cause each caller passes.  Pinned directly
    /// here, away from the lease chain's own timing.
    #[test]
    fn lease_reap_reports_deadline_not_explicit() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let eval_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "worker".into(),
            log_dir: PathBuf::from("/tmp/worker"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert!(reg.cancel_cause(1, CancelCause::Deadline));
        assert_eq!(
            eval_root.as_scope().cause(),
            Some(CancelCause::Deadline),
            "a reaped worker's eval layer reports it timed out, not that it was explicitly cancelled"
        );
    }

    /// In production `/clear` removes every worker it could make stale in the
    /// same stroke as the bump; this pins the invariant independent of that
    /// coincidence.
    #[test]
    fn settle_always_reaps_its_entry_even_across_a_bare_generation_bump() {
        let reg = AgentRegistry::new();
        let g = entry(&reg, 1, None);
        reg.lock().generation += 1;
        assert!(
            !reg.settle(1, g),
            "a stale-generation result is not delivered"
        );
        assert!(
            !reg.is_live(1),
            "but its entry is reaped regardless, never left dangling"
        );
    }

    #[test]
    fn renew_defers_the_fire() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let ttl = Duration::from_millis(300);
        let eval_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(ttl),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        std::thread::sleep(ttl / 2);
        assert!(reg.renew(1), "a live entry renews");

        std::thread::sleep(ttl / 2 + Duration::from_millis(50));
        assert!(
            !eval_root.as_scope().is_cancelled(),
            "renewed at half the ttl, still alive past the original bound"
        );

        assert!(
            eventually(Duration::from_secs(2), || eval_root
                .as_scope()
                .is_cancelled()),
            "reaped once the renewed span elapses"
        );
    }

    #[test]
    fn settle_before_the_fire_ends_the_chain_silently() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let ttl = Duration::from_millis(80);
        let token = Token::new();
        let g = reg
            .register(Registration {
                id: 1,
                parent: Some(0),
                lease: Some(ttl),
                name: "child".into(),
                log_dir: PathBuf::from("/tmp/child"),
                cancel: token.clone(),
                reach: Some(EvalReach::Identity {
                    eval_root: DurableRoot::default(),
                    run_scope: RunScope::default(),
                }),
                mailbox: mb(),
                provider: provider(),
            })
            .expect("registers");

        assert!(reg.settle(1, g), "settles well before the lease would fire");

        std::thread::sleep(ttl * 4);
        assert!(
            !token.is_cancelled(),
            "the chain's late fire found no entry and never touched the settled token"
        );
    }

    #[test]
    fn clear_subtree_outranks_the_lease() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let ttl = Duration::from_millis(80);
        let eval_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(ttl),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                run_scope: RunScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        reg.clear_subtree(0);
        assert!(!reg.is_live(1), "the reap removes the entry immediately");
        assert_eq!(
            eval_root.as_scope().cause(),
            Some(CancelCause::Explicit),
            "clear_subtree cancels through the ordinary /clear cause"
        );

        std::thread::sleep(ttl * 4);
        assert_eq!(
            eval_root.as_scope().cause(),
            Some(CancelCause::Explicit),
            "the lease's late fire finds no entry and never overwrites the cause"
        );
    }

    /// Notably, a raw push onto the mailbox, bypassing `renew`, is not an
    /// exchange.
    #[test]
    fn engaged_and_idle_read_the_exchange_clock() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let inbox = crate::bus::Inbox::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: inbox.mailbox(),
            provider: provider(),
        });

        assert!(!reg.engaged(1), "a fresh entry has never been engaged");
        assert!(
            reg.idle(1).is_some(),
            "idle reads off birth before any exchange"
        );

        inbox.push(Post::UserSteering("hello".into())).unwrap();
        assert!(!reg.engaged(1), "a raw mailbox push is not an exchange");

        assert!(reg.renew(1), "renew stamps a live entry");
        assert!(reg.engaged(1), "renewed at least once");
        let idle_after_renew = reg.idle(1).expect("still live");
        assert!(
            idle_after_renew < Duration::from_secs(1),
            "idle resets to (near) zero right after a renewal"
        );
    }

    #[test]
    fn steer_renews_and_delivers() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let inbox = crate::bus::Inbox::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
            mailbox: inbox.mailbox(),
            provider: provider(),
        });

        assert!(!reg.engaged(1), "not yet engaged");
        assert!(
            reg.steer(1, "hello".into()),
            "a live entry accepts steering"
        );
        assert!(reg.engaged(1), "steer renews the exchange clock");
        assert!(
            reg.idle(1).expect("still live") < Duration::from_secs(1),
            "idle resets to (near) zero right after a steer"
        );

        let Some(crate::bus::Item::Human(text)) = inbox.next_item() else {
            panic!("expected the steered text to have actually landed");
        };
        assert_eq!(text, "hello");

        assert!(
            !reg.steer(99, "nobody".into()),
            "a dead id is refused, not panicked"
        );
    }

    fn mb() -> Mailbox {
        crate::bus::Inbox::new().mailbox()
    }
}
