//! One owner, one lock, and scope-vouched deliveries.

use super::post::Source;
use super::{AgentMessage, AgentOutcome, AgentResult, Item, Next, Read, Rewrite, ScheduleId};
use crate::record::AgentId;
use ral_core::data::FOValue;
use ral_core::process::{CancelCause, Scope, ScopeId, ScopeNode};
use ral_core::sync::{CondvarExt, LockExt};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ParkMode {
    Held,
    Engaged,
    HeldByChildren,
    UntilCancelled,
    Quiesce,
}

/// How soon a parked owner sees a cancellation nobody notified: an
/// ancestor's, or a terminal failure's; its own end, a Stop and a post notify.
const PARK_POLL: Duration = Duration::from_millis(100);

/// A host job, from a human message until settlement or Stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobId(u64);
impl JobId {
    pub fn get(self) -> u64 {
        self.0
    }
}
impl From<u64> for JobId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Queue {
    Keep,
    Recall,
}

pub(crate) struct Inbox(Arc<Shared>);
#[derive(Clone)]
pub(crate) struct Mailbox {
    shared: Arc<Shared>,
    scope: Scope,
}
#[derive(Clone)]
pub(crate) struct WeakMailbox {
    shared: Weak<Shared>,
    scope: Scope,
}
struct Shared {
    life: ScopeNode,
    state: Mutex<State>,
    signal: Condvar,
}

pub(crate) struct State {
    posts: Posts,
    work: Work,
    exchange: ScopeNode,
    opening: Option<ScopeNode>,
    job: Option<OpenJob>,
    next_job: u64,
    rest: Option<Instant>,
    reply: Option<FOValue>,
    last_exchange: Option<Instant>,
    waiting_for_input: bool,
    report: Option<Arc<Report>>,
    heard: Vec<Owed>,
    lease: Option<Arc<Instant>>,
}
struct Work {
    scope: ScopeNode,
    receipts: Arc<()>,
}
impl Work {
    fn under(life: &Scope) -> Self {
        Self {
            scope: life.child(),
            receipts: Arc::new(()),
        }
    }
    /// Receipts are minted only here, under the owner's lock.
    fn owe(&self, report: Arc<Report>) -> Owed {
        Owed(Some((
            report,
            Receipt {
                _share: self.receipts.clone(),
            },
        )))
    }
}
/// The exchange minted at pop; `open` installs it, Drop declines it.
#[must_use]
pub(crate) struct Opening {
    shared: Arc<Shared>,
    id: ScopeId,
}
impl Opening {
    pub(crate) fn open(self) {
        let mut state = self.shared.lock();
        if let Some(node) = state.take_opening(&self.id) {
            state.exchange.cancel(CancelCause::Cancelled);
            // I3: install the scope minted at pop; never erase a racing Stop or Esc.
            state.exchange = node;
        }
    }
}
impl Drop for Opening {
    fn drop(&mut self) {
        self.shared.lock().take_opening(&self.id);
    }
}
pub(crate) struct Drawn {
    pub(crate) next: Next,
    pub(crate) opening: Option<Opening>,
}
struct OpenJob {
    id: JobId,
    stopped: bool,
}

pub(crate) struct Status {
    pub(crate) rest: Option<Instant>,
    pub(crate) replied: bool,
    pub(crate) reply_since: Option<Instant>,
    pub(crate) busy: bool,
    pub(crate) waiting_for_input: bool,
    pub(crate) last_exchange: Option<Instant>,
}

impl State {
    fn vouches(&self, scope: &Scope) -> bool {
        self.work.scope.id() == scope.id() && !scope.is_cancelled()
    }
    pub(crate) fn has_busy_children(&self) -> bool {
        Arc::strong_count(&self.work.receipts) > 1
    }
    pub(crate) fn leased(&self) -> bool {
        self.lease.is_some()
    }
    fn reply_since(&self) -> Option<Instant> {
        self.lease.as_deref().copied()
    }
    fn take_opening(&mut self, id: &ScopeId) -> Option<ScopeNode> {
        self.opening.take_if(|node| node.id() == *id)
    }
    fn pop(&mut self) -> Option<Next> {
        self.posts.pop().map(|post| self.admit(post))
    }
    fn pop_before_rewrite(&mut self) -> Option<Next> {
        self.posts.pop_before_rewrite().map(|post| self.admit(post))
    }
    fn admit(&mut self, post: Post) -> Next {
        match post {
            Post::Human(text) => Next::Item(Item::Human(text)),
            Post::Read(read) => Next::Read(read),
            Post::Rewrite(rewrite) => Next::Rewrite(rewrite),
            Post::Scoped { kind, .. } => Next::Item(match kind {
                Scoped::Result { result, paid } => {
                    drop(paid);
                    Item::Agent(result)
                }
                Scoped::Message { message, owed } => {
                    self.heard.extend(owed);
                    Item::Message(message)
                }
                Scoped::Surface { id, values } => Item::Surface { id, values },
                Scoped::Wakeup {
                    label,
                    trigger,
                    prompt,
                    ..
                } => Item::Wakeup(format!("[scheduled '{label}' · {trigger}] {prompt}")),
                Scoped::Nudge { prompt, text } => Item::Nudge { prompt, text },
            }),
        }
    }
    fn abandon(&mut self, life: &Scope, queue: Queue) -> Vec<String> {
        let old = std::mem::replace(&mut self.work, Work::under(life));
        old.scope.cancel(CancelCause::Cancelled);
        match queue {
            Queue::Keep => Vec::new(),
            Queue::Recall => self.posts.recall(),
        }
    }
}

/// A share of one work's outstanding answers; dropping it takes no lock.
pub(crate) struct Receipt {
    _share: Arc<()>,
}

/// Where and as whom a child reports; one per child, shared by its debts.
pub(crate) struct Report {
    to: Mailbox,
    name: Arc<str>,
    born: Instant,
}
impl Report {
    /// A further answer owed to the adopting work, while it stands.
    pub(crate) fn owe(self: &Arc<Self>) -> Option<Owed> {
        let state = self.to.shared.lock();
        state
            .vouches(&self.to.scope)
            .then(|| state.work.owe(self.clone()))
    }
    /// The one way a result reaches a parent.
    fn pay(&self, outcome: AgentOutcome, paid: impl IntoIterator<Item = Receipt>) {
        self.to.post(Scoped::Result {
            result: AgentResult {
                name: self.name.to_string(),
                outcome,
                elapsed: self.born.elapsed(),
            },
            paid: paid.into_iter().collect(),
        });
    }
}
/// One answer owed under a `Report`; dropping it pays `Cancelled`.
#[must_use]
pub(crate) struct Owed(Option<(Arc<Report>, Receipt)>);
impl Owed {
    fn withdraw(mut self) {
        self.0.take();
    }
    fn into_receipt(mut self) -> Receipt {
        self.0
            .take()
            .expect("an owed holds its debt until consumed")
            .1
    }
}
impl Drop for Owed {
    fn drop(&mut self) {
        if let Some((report, receipt)) = self.0.take() {
            report.pay(AgentOutcome::Cancelled, [receipt]);
        }
    }
}

/// The inbox is the only strong owner, so a lease lapses exactly while no
/// input has broken it.
#[derive(Clone)]
pub(crate) struct Lease(Weak<Instant>);

/// Why a parent's work refused to adopt a child.
#[derive(Debug)]
pub(crate) enum Refused {
    Abandoned,
    Dead,
}

enum Post {
    Human(String),
    Read(Read),
    Rewrite(Rewrite),
    Scoped { from: Scope, kind: Scoped },
}
pub(crate) enum Scoped {
    Result {
        result: AgentResult,
        paid: Vec<Receipt>,
    },
    Surface {
        id: AgentId,
        values: Vec<FOValue>,
    },
    Wakeup {
        id: ScheduleId,
        label: String,
        trigger: String,
        prompt: String,
    },
    Message {
        message: AgentMessage,
        owed: Option<Owed>,
    },
    Nudge {
        prompt: u64,
        text: String,
    },
}
impl Post {
    fn source(&self) -> Source {
        match self {
            Self::Human(_) => Source::User,
            Self::Read(_) => Source::Read,
            Self::Rewrite(_) => Source::Rewrite,
            Self::Scoped { kind, .. } => match kind {
                Scoped::Result { .. } => Source::Agent,
                Scoped::Surface { .. } => Source::Surface,
                Scoped::Wakeup { .. } => Source::Schedule,
                Scoped::Message { .. } => Source::Message,
                Scoped::Nudge { .. } => Source::Nudge,
            },
        }
    }
    fn is_wakeup(&self, id: ScheduleId) -> bool {
        matches!(self, Self::Scoped { kind: Scoped::Wakeup { id: queued, .. }, .. } if *queued == id)
    }
    fn is_input(&self) -> bool {
        matches!(self.source(), Source::User | Source::Message)
    }
    /// A refused post pays nothing: its debt, if any, is withdrawn.
    fn withdraw(self) {
        if let Self::Scoped {
            kind: Scoped::Message {
                owed: Some(owed), ..
            },
            ..
        } = self
        {
            owed.withdraw();
        }
    }
}
/// Human fragments retain their arrival until consumed, so a clear can split
/// a coalesced prompt without changing the queue during preparation.
#[derive(Default)]
struct Posts {
    queue: VecDeque<(u64, Post)>,
    next: u64,
}
#[must_use]
pub(crate) struct ClearBoundary(u64);
impl Posts {
    fn stamp(&mut self, post: Post) -> (u64, Post) {
        let order = self.next;
        self.next = order
            .checked_add(1)
            .expect("inbox arrival counter exhausted");
        (order, post)
    }
    fn push(&mut self, post: Post) {
        let replace = match &post {
            Post::Scoped {
                kind: Scoped::Wakeup { id, .. },
                ..
            } => self.queue.iter().position(|(_, p)| p.is_wakeup(*id)),
            Post::Scoped {
                kind: Scoped::Nudge { .. },
                ..
            } => self
                .queue
                .iter()
                .position(|(_, p)| p.source() == Source::Nudge),
            _ => None,
        };
        let stamped = self.stamp(post);
        if let Some(index) = replace {
            self.queue[index] = stamped;
        } else {
            self.queue.push_back(stamped);
        }
    }
    fn pop(&mut self) -> Option<Post> {
        self.fence();
        self.take()
    }
    /// The next live post, unless it is a rewrite, which waits for the exchange boundary.
    fn pop_before_rewrite(&mut self) -> Option<Post> {
        self.fence();
        if matches!(self.queue.front(), Some((_, Post::Rewrite(_)))) {
            None
        } else {
            self.take()
        }
    }
    fn take(&mut self) -> Option<Post> {
        let (_, mut post) = self.queue.pop_front()?;
        if let Post::Human(text) = &mut post {
            while matches!(self.queue.front(), Some((_, Post::Human(_)))) {
                if let Some((_, Post::Human(next))) = self.queue.pop_front() {
                    text.push('\n');
                    text.push_str(&next);
                }
            }
        }
        Some(post)
    }
    fn is_empty(&mut self) -> bool {
        self.fence();
        self.queue.is_empty()
    }
    fn has_wakeup(&mut self, id: ScheduleId) -> bool {
        self.fence();
        self.queue.iter().any(|(_, p)| p.is_wakeup(id))
    }
    fn has_human(&mut self) -> bool {
        self.fence();
        self.queue.iter().any(|(_, p)| matches!(p, Post::Human(_)))
    }
    fn has_input(&mut self) -> bool {
        self.fence();
        self.queue.iter().any(|(_, p)| p.is_input())
    }
    fn take_owed(&mut self) -> Vec<Owed> {
        self.fence();
        self.queue
            .iter_mut()
            .filter_map(|(_, post)| match post {
                Post::Scoped {
                    kind: Scoped::Message { owed, .. },
                    ..
                } => owed.take(),
                _ => None,
            })
            .collect()
    }
    fn recall(&mut self) -> Vec<String> {
        self.fence();
        let mut prompts = Vec::new();
        let mut human_tail = false;
        self.queue.retain(|(_, post)| {
            if let Post::Human(text) = post {
                Self::join(&mut prompts, human_tail, text);
                human_tail = true;
                false
            } else {
                human_tail = false;
                true
            }
        });
        prompts
    }
    fn texts(&mut self) -> Vec<String> {
        self.fence();
        let mut texts = Vec::new();
        let mut human_tail = false;
        for (_, post) in &self.queue {
            match post {
                Post::Human(text) => Self::join(&mut texts, human_tail, text),
                Post::Read(r) => texts.push(r.to_string()),
                Post::Rewrite(r) => texts.push(r.to_string()),
                Post::Scoped { .. } => {}
            }
            human_tail = matches!(post, Post::Human(_));
        }
        texts
    }
    fn join(texts: &mut Vec<String>, human_tail: bool, text: &str) {
        if human_tail {
            let tail = texts
                .last_mut()
                .expect("a human tail has a preceding fragment");
            tail.push('\n');
            tail.push_str(text);
        } else {
            texts.push(text.to_string());
        }
    }
    fn depths(&mut self) -> Vec<(Source, u64)> {
        self.fence();
        let mut rows: Vec<_> = Source::ALL.into_iter().map(|s| (s, 0)).collect();
        let mut human_tail = false;
        for (_, post) in &self.queue {
            let human = matches!(post, Post::Human(_));
            if !(human && human_tail)
                && let Some((_, count)) = rows.iter_mut().find(|(s, _)| *s == post.source())
            {
                *count += 1;
            }
            human_tail = human;
        }
        rows
    }
    fn fence(&mut self) {
        self.queue
            .retain(|(_, post)| !matches!(post, Post::Scoped { from, .. } if from.is_cancelled()));
    }
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock_ignore_poison()
    }
    fn notify(&self) {
        self.signal.notify_all();
    }
}
impl WeakMailbox {
    pub(crate) fn upgrade(&self) -> Option<Mailbox> {
        (!self.scope.is_cancelled())
            .then(|| self.shared.upgrade())
            .flatten()
            .map(|shared| Mailbox {
                shared,
                scope: self.scope.clone(),
            })
    }
}
impl Mailbox {
    pub(crate) fn scope(&self) -> Scope {
        self.scope.clone()
    }
    pub(crate) fn downgrade(&self) -> WeakMailbox {
        WeakMailbox {
            shared: Arc::downgrade(&self.shared),
            scope: self.scope.clone(),
        }
    }
    pub(crate) fn is_current(&self) -> bool {
        self.shared.lock().vouches(&self.scope)
    }
    pub(crate) fn adopt(
        &self,
        name: Arc<str>,
        born: Instant,
    ) -> Result<(ScopeNode, (Arc<Report>, Owed)), Refused> {
        let state = self.shared.lock();
        if self.shared.life.is_cancelled() {
            return Err(Refused::Dead);
        }
        if !state.vouches(&self.scope) {
            return Err(Refused::Abandoned);
        }
        let report = Arc::new(Report {
            to: self.clone(),
            name,
            born,
        });
        let owed = state.work.owe(report.clone());
        Ok((state.work.scope.child(), (report, owed)))
    }
    pub(crate) fn report(&self) -> Option<Arc<Report>> {
        self.shared.lock().report.clone()
    }
    pub(crate) fn post(&self, kind: Scoped) -> bool {
        self.deliver(Post::Scoped {
            from: self.scope.clone(),
            kind,
        })
    }
    fn deliver(&self, post: Post) -> bool {
        let mut state = self.shared.lock();
        if self.shared.life.is_cancelled()
            || matches!(&post, Post::Scoped { from, .. } if from.is_cancelled())
        {
            drop(state);
            post.withdraw();
            return false;
        }
        if post.is_input() {
            state.last_exchange = Some(Instant::now());
            state.lease = None;
        }
        state.posts.push(post);
        state.waiting_for_input = false;
        drop(state);
        self.shared.notify();
        true
    }
    pub(crate) fn vouched_by(&self, scope: Scope) -> Self {
        Self {
            shared: self.shared.clone(),
            scope,
        }
    }
    pub(crate) fn is_live(&self) -> bool {
        !self.shared.life.is_cancelled()
    }
    pub(crate) fn end(&self, cause: CancelCause) {
        let _state = self.shared.lock();
        self.shared.life.cancel(cause);
        self.shared.notify();
    }
    pub(crate) fn notify(&self) {
        self.shared.notify();
    }
    pub(crate) fn interrupt(&self) {
        let state = self.shared.lock();
        state.exchange.cancel(CancelCause::Interrupted);
        if let Some(opening) = &state.opening {
            opening.cancel(CancelCause::Interrupted);
        }
        drop(state);
        self.shared.notify();
    }
    pub(crate) fn steer(&self, text: String) -> bool {
        self.deliver(Post::Human(text))
    }
    pub(crate) fn post_read(&self, read: Read) {
        self.deliver(Post::Read(read));
    }
    pub(crate) fn post_rewrite(&self, rewrite: Rewrite) {
        self.deliver(Post::Rewrite(rewrite));
    }
    pub(crate) fn queued(&self) -> Vec<String> {
        let mut state = self.shared.lock();
        if self.shared.life.is_cancelled() {
            return Vec::new();
        }
        state.posts.texts()
    }
    pub(crate) fn recall(&self) -> Vec<String> {
        let mut state = self.shared.lock();
        if self.shared.life.is_cancelled() {
            return Vec::new();
        }
        state.posts.recall()
    }
    pub(crate) fn stop(&self, id: JobId) -> Option<Vec<String>> {
        let mut state = self.shared.lock();
        if self.shared.life.is_cancelled() {
            return None;
        }
        // I7: Stop and close linearise on this lock.
        let job = state
            .job
            .as_mut()
            .filter(|job| job.id == id && !job.stopped)?;
        job.stopped = true;
        let recalled = state.abandon(&self.shared.life, Queue::Recall);
        drop(state);
        self.shared.notify();
        Some(recalled)
    }
    pub(crate) fn status(&self) -> Status {
        let state = self.shared.lock();
        Status {
            rest: state.rest,
            replied: state.reply.is_some(),
            reply_since: state.reply_since(),
            busy: state.has_busy_children(),
            waiting_for_input: state.waiting_for_input,
            last_exchange: state.last_exchange,
        }
    }
    pub(crate) fn reply(&self) -> Option<FOValue> {
        self.shared.lock().reply.clone()
    }
    pub(crate) fn lapse(&self, lease: &Lease) {
        let _state = self.shared.lock();
        if !self.shared.life.is_cancelled() && lease.0.upgrade().is_some() {
            self.shared.life.cancel(CancelCause::TimedOut);
            self.shared.notify();
        }
    }
    pub(crate) fn has_queued_wakeup(&self, id: ScheduleId) -> bool {
        let mut state = self.shared.lock();
        state.posts.has_wakeup(id)
    }
}
impl Inbox {
    pub(crate) fn new(life: ScopeNode, parent: Option<(Arc<Report>, Owed)>) -> Self {
        let work = Work::under(&life);
        let (report, owed) = parent.unzip();
        let exchange = work.scope.child();
        exchange.cancel(CancelCause::Cancelled);
        Self(Arc::new(Shared {
            life,
            state: Mutex::new(State {
                posts: Posts::default(),
                work,
                exchange,
                opening: None,
                job: None,
                next_job: 0,
                rest: None,
                reply: None,
                last_exchange: None,
                waiting_for_input: true,
                report,
                heard: owed.into_iter().collect(),
                lease: None,
            }),
            signal: Condvar::new(),
        }))
    }
    pub(crate) fn mailbox(&self) -> Mailbox {
        Mailbox {
            shared: self.0.clone(),
            scope: (*self.0.lock().work.scope).clone(),
        }
    }
    pub(crate) fn life(&self) -> Scope {
        (*self.0.life).clone()
    }
    pub(crate) fn life_mailbox(&self) -> Mailbox {
        Mailbox {
            shared: self.0.clone(),
            scope: (*self.0.life).clone(),
        }
    }
    pub(crate) fn exchange(&self) -> Scope {
        (*self.0.lock().exchange).clone()
    }
    pub(crate) fn draw(&self, park: impl Fn(&State) -> ParkMode) -> Option<Drawn> {
        let mut state = self.0.lock();
        loop {
            if self.0.life.is_cancelled() || state.job.as_ref().is_some_and(|j| j.stopped) {
                return None;
            }
            let mode = park(&state);
            if let Some(next) = state.pop() {
                let opening =
                    matches!(&next, Next::Item(item) if item.opens_exchange()).then(|| {
                        let node = state.work.scope.child();
                        let id = node.id();
                        state.opening = Some(node);
                        Opening {
                            shared: self.0.clone(),
                            id,
                        }
                    });
                state.waiting_for_input = false;
                return Some(Drawn { next, opening });
            }
            if mode == ParkMode::Quiesce {
                state.waiting_for_input = false;
                return None;
            }
            state.waiting_for_input = matches!(mode, ParkMode::Held | ParkMode::Engaged);
            (state, _) = self.0.signal.wait_timeout_ignore_poison(state, PARK_POLL);
        }
    }
    pub(crate) fn drain(&self) -> Vec<Next> {
        let mut state = self.0.lock();
        let mut arrivals = Vec::new();
        if state.exchange.is_cancelled() {
            return arrivals;
        }
        while let Some(next) = state.pop_before_rewrite() {
            arrivals.push(next);
        }
        drop(state);
        arrivals
    }
    pub(crate) fn close_exchange(&self) {
        let state = self.0.lock();
        state.exchange.cancel(CancelCause::Cancelled);
        debug_assert!(state.opening.is_none(), "rewrites never open an exchange");
        drop(state);
    }
    pub(crate) fn abandon(&self, queue: Queue) -> Vec<String> {
        let recalled = self.0.lock().abandon(&self.0.life, queue);
        self.0.notify();
        recalled
    }
    pub(crate) fn clear_boundary(&self) -> ClearBoundary {
        ClearBoundary(self.0.lock().posts.next)
    }
    #[expect(
        clippy::needless_pass_by_value,
        reason = "consuming the non-Copy witness prevents applying a clear boundary twice"
    )]
    pub(crate) fn apply_clear(&self, boundary: ClearBoundary) {
        let mut state = self.0.lock();
        state.abandon(&self.0.life, Queue::Keep);
        state.posts.queue.retain(|(order, _)| *order >= boundary.0);
        drop(state);
        self.0.notify();
    }
    pub(crate) fn await_job(&self) -> Option<Job> {
        let mut state = self.0.lock();
        loop {
            if self.0.life.is_cancelled() {
                return None;
            }
            if state.job.is_none() && state.posts.has_human() {
                state.next_job += 1;
                let id = JobId(state.next_job);
                state.job = Some(OpenJob { id, stopped: false });
                return Some(Job(Ticket {
                    id,
                    shared: self.0.clone(),
                }));
            }
            state.waiting_for_input = true;
            (state, _) = self.0.signal.wait_timeout_ignore_poison(state, PARK_POLL);
        }
    }
    pub(crate) fn nudge(&self, prompt: u64, text: String) {
        self.life_mailbox()
            .vouched_by(self.exchange())
            .post(Scoped::Nudge { prompt, text });
    }
    pub(crate) fn set_resting(&self, resting: bool) {
        let mut state = self.0.lock();
        if resting {
            state.rest.get_or_insert_with(Instant::now);
        } else {
            state.rest = None;
        }
    }
    pub(crate) fn deposit_reply(&self, reply: FOValue) -> Option<Lease> {
        let mut state = self.0.lock();
        state.reply = Some(reply);
        let heard = std::mem::take(&mut state.heard);
        state.lease = (!state.posts.has_input()).then(|| Arc::new(Instant::now()));
        let lease = state.lease.as_ref().map(|held| Lease(Arc::downgrade(held)));
        let report = state.report.clone();
        drop(state);
        if let Some(report) = report {
            report.pay(
                AgentOutcome::Replied,
                heard.into_iter().map(Owed::into_receipt),
            );
        }
        lease
    }
    pub(crate) fn disarm_reply_lease(&self) {
        self.0.lock().lease = None;
    }
    pub(crate) fn retire(&self, outcome: AgentOutcome) {
        let mut state = self.0.lock();
        self.0.life.cancel(CancelCause::Cancelled);
        let report = state.report.take();
        let mut debts = std::mem::take(&mut state.heard);
        debts.extend(state.posts.take_owed());
        let clean = state.leased();
        drop(state);
        self.0.notify();
        let Some(report) = report else { return };
        let outcome = match outcome {
            AgentOutcome::Replied if clean => {
                debug_assert!(debts.is_empty(), "a clean reply leaves no debt behind");
                return;
            }
            AgentOutcome::Replied => AgentOutcome::Cancelled,
            outcome => outcome,
        };
        report.pay(outcome, debts.into_iter().map(Owed::into_receipt));
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.0.lock().posts.is_empty()
    }
    pub(crate) fn source_depths(&self) -> Vec<(Source, u64)> {
        self.0.lock().posts.depths()
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.retire(AgentOutcome::Failed("ended before reporting".into()));
    }
}

/// The open job's claim on its inbox; dropping it closes the job.
struct Ticket {
    id: JobId,
    shared: Arc<Shared>,
}
impl Drop for Ticket {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        if state.job.take_if(|job| job.id == self.id).is_some() {
            drop(state);
            self.shared.notify();
        }
    }
}

/// An open job, attended at most once; closing or dropping it ends it.
#[must_use]
pub struct Job(Ticket);

/// How a job ended: still running, or already stopped and held open.
pub(crate) enum Closed {
    Ran,
    Stopped(StoppedJob),
}

/// A stopped job; it blocks `draw` until dropped.
#[must_use]
pub(crate) struct StoppedJob {
    _ticket: Ticket,
}

impl Job {
    pub fn id(&self) -> JobId {
        self.0.id
    }
    pub(crate) fn belongs_to(&self, mailbox: &Mailbox) -> bool {
        Arc::ptr_eq(&self.0.shared, &mailbox.shared)
    }
    pub(crate) fn close(self) -> Closed {
        let mut state = self.0.shared.lock();
        let running = state
            .job
            .take_if(|job| job.id == self.0.id && !job.stopped)
            .is_some();
        drop(state);
        if running {
            self.0.shared.notify();
            Closed::Ran
        } else {
            Closed::Stopped(StoppedJob { _ticket: self.0 })
        }
    }
}

#[cfg(test)]
mod tests;
