//! One owner, one lock, and scope-vouched deliveries.

use super::post::{ENDED_BEFORE_REPORTING, Source};
use super::{AgentMessage, Item, Next, Read, Rewrite, ScheduleId};
use crate::record::{AgentId, ChildStatus, Emitter as Recorder, Forensic, Settlement, Unavailable};
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
    report: Option<Report>,
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
    last_exchange: Option<Instant>,
    waiting_for_input: bool,
    lease: Option<Arc<Instant>>,
}
struct Work {
    scope: ScopeNode,
    children: Vec<Arc<Child>>,
}
impl Work {
    fn under(life: &Scope) -> Self {
        Self {
            scope: life.child(),
            children: Vec::new(),
        }
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
        self.work.children.iter().any(|child| child.is_working())
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
        self.posts.pop().map(Post::admit)
    }
    fn pop_before_rewrite(&mut self) -> Option<Next> {
        self.posts.pop_before_rewrite().map(Post::admit)
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

/// What a parent can know about a child; shared by both.
pub(crate) struct Child {
    name: Arc<str>,
    born: Instant,
    /// Held across record and publication; `None` once retired.
    record: Mutex<Option<Recorder>>,
    /// A leaf.
    status: Mutex<ChildStatus>,
}
impl Child {
    pub(crate) fn new(name: Arc<str>, born: Instant, record: Recorder) -> Self {
        Self {
            name,
            born,
            record: Mutex::new(Some(record)),
            status: Mutex::new(ChildStatus::Working),
        }
    }
    fn status(&self) -> ChildStatus {
        self.status.lock_ignore_poison().clone()
    }
    fn is_working(&self) -> bool {
        matches!(*self.status.lock_ignore_poison(), ChildStatus::Working)
    }
    fn has_replied(&self) -> bool {
        matches!(*self.status.lock_ignore_poison(), ChildStatus::Replied(_))
    }
    fn set(&self, status: ChildStatus) {
        *self.status.lock_ignore_poison() = status;
    }
}

/// Where a returning agent reports: its status and, if any, its parent's work.
pub(crate) struct Report {
    child: Arc<Child>,
    to: Option<Mailbox>,
}
impl Report {
    pub(crate) fn new(child: Arc<Child>, to: Option<Mailbox>) -> Self {
        Self { child, to }
    }
    fn show(&self, status: ChildStatus) {
        match &self.to {
            Some(parent) => parent.finish(&self.child, status),
            None => self.child.set(status),
        }
    }
    /// `log` is borrowed from the held record lock.
    fn transition(&self, log: &Recorder, status: ChildStatus) {
        log.emit_or_end(Forensic::Status {
            status: status.clone(),
        });
        self.show(status);
    }
    /// A failed append is an error: the latched writer ends the life.
    fn resume(&self) -> Result<(), Unavailable> {
        let record = self.child.record.lock_ignore_poison();
        if let Some(log) = record.as_ref() {
            let _recorded = log.emit(Forensic::Status {
                status: ChildStatus::Working,
            })?;
        }
        drop(record);
        Ok(())
    }
    /// A failed append ends as that failure, which the parent hears instead.
    fn end(&self, reason: String) {
        let mut record = self.child.record.lock_ignore_poison();
        let Some(log) = record.take() else { return };
        if self.child.is_working() {
            let ended = ChildStatus::Failed(reason);
            let shown = if std::thread::panicking() {
                ended
            } else {
                match log.emit(Forensic::Status {
                    status: ended.clone(),
                }) {
                    Ok(_) => ended,
                    Err(failure) => ChildStatus::Failed(failure.to_string()),
                }
            };
            self.show(shown);
        }
        drop(record);
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
    Scoped {
        from: Scope,
        kind: Scoped,
    },
    /// Only `Mailbox::follow_up` makes one: it also moves the child to Working.
    FollowUp {
        from: Scope,
        message: AgentMessage,
    },
}
pub(crate) enum Scoped {
    Agent {
        name: Arc<str>,
        settled: Settlement,
        elapsed: Duration,
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
    Message(AgentMessage),
    Nudge {
        prompt: u64,
        text: String,
    },
}
impl Post {
    fn admit(self) -> Next {
        match self {
            Self::Human(text) => Next::Item(Item::Human(text)),
            Self::Read(read) => Next::Read(read),
            Self::Rewrite(rewrite) => Next::Rewrite(rewrite),
            Self::FollowUp { message, .. } => Next::Item(Item::Message(message)),
            Self::Scoped { kind, .. } => Next::Item(match kind {
                Scoped::Agent {
                    name,
                    settled,
                    elapsed,
                } => Item::Agent {
                    name,
                    settled,
                    elapsed,
                },
                Scoped::Message(message) => Item::Message(message),
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
    fn source(&self) -> Source {
        match self {
            Self::Human(_) => Source::User,
            Self::Read(_) => Source::Read,
            Self::Rewrite(_) => Source::Rewrite,
            Self::FollowUp { .. } => Source::Message,
            Self::Scoped { kind, .. } => match kind {
                Scoped::Agent { .. } => Source::Agent,
                Scoped::Surface { .. } => Source::Surface,
                Scoped::Wakeup { .. } => Source::Schedule,
                Scoped::Message(_) => Source::Message,
                Scoped::Nudge { .. } => Source::Nudge,
            },
        }
    }
    /// The scope that vouches for this post; human input and reads need none.
    fn voucher(&self) -> Option<&Scope> {
        match self {
            Self::Scoped { from, .. } | Self::FollowUp { from, .. } => Some(from),
            Self::Human(_) | Self::Read(_) | Self::Rewrite(_) => None,
        }
    }
    fn is_wakeup(&self, id: ScheduleId) -> bool {
        matches!(self, Self::Scoped { kind: Scoped::Wakeup { id: queued, .. }, .. } if *queued == id)
    }
    fn is_input(&self) -> bool {
        matches!(self.source(), Source::User | Source::Message)
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
    fn has_parent_input(&mut self) -> bool {
        self.fence();
        self.queue
            .iter()
            .any(|(_, post)| matches!(post, Post::FollowUp { .. }))
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
                Post::Scoped { .. } | Post::FollowUp { .. } => {}
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
            .retain(|(_, post)| !post.voucher().is_some_and(Scope::is_cancelled));
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
    pub(crate) fn adopt(&self, child: Arc<Child>) -> Result<ScopeNode, Refused> {
        let mut state = self.shared.lock();
        if self.shared.life.is_cancelled() {
            return Err(Refused::Dead);
        }
        if !state.vouches(&self.scope) {
            return Err(Refused::Abandoned);
        }
        state.work.children.push(child);
        Ok(state.work.scope.child())
    }
    fn finish(&self, child: &Child, status: ChildStatus) {
        let notice = status.settlement().map(|settled| Post::Scoped {
            from: self.scope.clone(),
            kind: Scoped::Agent {
                name: child.name.clone(),
                settled,
                elapsed: child.born.elapsed(),
            },
        });
        let mut state = self.shared.lock();
        child.set(status);
        let queued = notice.is_some_and(|post| self.enqueue(&mut state, post));
        drop(state);
        if queued {
            self.shared.notify();
        }
    }
    pub(crate) fn follow_up(&self, message: AgentMessage) -> bool {
        let Some(report) = &self.shared.report else {
            return self.post(Scoped::Message(message));
        };
        let record = report.child.record.lock_ignore_poison();
        let Some(log) = record.as_ref() else {
            return false;
        };
        let post = Post::FollowUp {
            from: self.scope.clone(),
            message,
        };
        if !self.deliver(post) {
            return false;
        }
        if !report.child.is_working() {
            report.transition(log, ChildStatus::Working);
        }
        drop(record);
        true
    }
    pub(crate) fn child_status(&self, name: &str) -> Option<ChildStatus> {
        self.shared
            .lock()
            .work
            .children
            .iter()
            .rev()
            .find(|child| &*child.name == name)
            .map(|child| child.status())
    }
    pub(crate) fn post(&self, kind: Scoped) -> bool {
        self.deliver(Post::Scoped {
            from: self.scope.clone(),
            kind,
        })
    }
    fn enqueue(&self, state: &mut State, post: Post) -> bool {
        if self.shared.life.is_cancelled() || post.voucher().is_some_and(Scope::is_cancelled) {
            return false;
        }
        if post.is_input() {
            state.last_exchange = Some(Instant::now());
            state.lease = None;
        }
        state.posts.push(post);
        state.waiting_for_input = false;
        true
    }
    fn deliver(&self, post: Post) -> bool {
        let mut state = self.shared.lock();
        let queued = self.enqueue(&mut state, post);
        drop(state);
        if queued {
            self.shared.notify();
        }
        queued
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
            replied: self
                .shared
                .report
                .as_ref()
                .is_some_and(|report| report.child.has_replied()),
            reply_since: state.reply_since(),
            busy: state.has_busy_children(),
            waiting_for_input: state.waiting_for_input,
            last_exchange: state.last_exchange,
        }
    }
    pub(crate) fn reply(&self) -> Option<FOValue> {
        match self.shared.report.as_ref()?.child.status() {
            ChildStatus::Replied(value) => Some(value),
            ChildStatus::Working | ChildStatus::Failed(_) => None,
        }
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
    pub(crate) fn new(life: ScopeNode, report: Option<Report>) -> Self {
        let work = Work::under(&life);
        let exchange = work.scope.child();
        exchange.cancel(CancelCause::Cancelled);
        Self(Arc::new(Shared {
            life,
            report,
            state: Mutex::new(State {
                posts: Posts::default(),
                work,
                exchange,
                opening: None,
                job: None,
                next_job: 0,
                rest: None,
                last_exchange: None,
                waiting_for_input: true,
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
    pub(crate) fn deposit_reply(&self, value: FOValue) -> Option<Lease> {
        let record = self
            .0
            .report
            .as_ref()
            .map(|report| report.child.record.lock_ignore_poison());
        let mut state = self.0.lock();
        let crossed = state.posts.has_parent_input();
        state.lease = (!state.posts.has_input()).then(|| Arc::new(Instant::now()));
        let lease = state.lease.as_ref().map(|held| Lease(Arc::downgrade(held)));
        drop(state);
        if !crossed && let (Some(report), Some(Some(log))) = (&self.0.report, record.as_deref()) {
            report.transition(log, ChildStatus::Replied(value));
        }
        drop(record);
        lease
    }
    /// A resumed life works again, however its log last settled.
    pub(crate) fn resume(&self) -> Result<(), Unavailable> {
        self.0.report.as_ref().map_or(Ok(()), Report::resume)
    }
    pub(crate) fn disarm_reply_lease(&self) {
        self.0.lock().lease = None;
    }
    pub(crate) fn retire(&self, reason: String) {
        let state = self.0.lock();
        self.0.life.cancel(CancelCause::Cancelled);
        drop(state);
        self.0.notify();
        if let Some(report) = &self.0.report {
            report.end(reason);
        }
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
        self.retire(ENDED_BEFORE_REPORTING.into());
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
