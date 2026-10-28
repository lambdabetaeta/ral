//! One owner, one lock, and scope-vouched deliveries.

use super::post::Source;
use super::{AgentMessage, AgentOutcome, AgentResult, Item, Next, Read, Rewrite, ScheduleId};
use crate::record::AgentId;
use ral_core::data::FOValue;
use ral_core::process::{CancelCause, Scope, ScopeNode};
use ral_core::sync::{CondvarExt, LockExt};
use std::collections::{BTreeMap, VecDeque};
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

/// How soon a parked owner sees severance or an ancestor's cancellation;
/// its own end, a Stop and a post notify.
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
    Discard,
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
    posts: VecDeque<Post>,
    work: Work,
    exchange: ScopeNode,
    opening: Option<ScopeNode>,
    job: Option<OpenJob>,
    next_job: u64,
    next_message: u64,
    accepted: u64,
    answered: u64,
    rest: Option<Instant>,
    reply: Option<FOValue>,
    replied_at: Option<Instant>,
    last_exchange: Option<Instant>,
    waiting_for_input: bool,
}
struct Work {
    scope: ScopeNode,
    awaiting: BTreeMap<AgentId, u64>,
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
    pub(crate) fn has_busy_children(&self) -> bool {
        !self.work.awaiting.is_empty()
    }
    pub(crate) fn replied_at(&self) -> Option<Instant> {
        self.replied_at
    }
    fn reply_since(&self) -> Option<Instant> {
        self.replied_at.filter(|since| {
            self.accepted <= self.answered && self.last_exchange.is_none_or(|at| at <= *since)
        })
    }
    fn pop(&mut self) -> Option<Next> {
        // I2: fence provenance at pop, then coalesce newly adjacent human posts.
        fence(&mut self.posts);
        let next = match self.posts.pop_front()? {
            Post::Human(text) => Next::Item(Item::Human(text)),
            Post::Read(read) => Next::Read(read),
            Post::Rewrite(rewrite) => Next::Rewrite(rewrite),
            Post::Scoped { kind, .. } => Next::Item(match kind {
                Scoped::Result(result) => {
                    // I6: an old reply cannot discharge a follow-up message.
                    if self.work.awaiting.get(&result.id).is_some_and(|n| {
                        !matches!(result.outcome, AgentOutcome::Replied) || result.answers >= *n
                    }) {
                        self.work.awaiting.remove(&result.id);
                    }
                    Item::Agent(result)
                }
                Scoped::Message(message) => {
                    self.answered = self.answered.max(message.seq);
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
        };
        Some(next)
    }
    fn abandon(&mut self, life: &Scope, queue: Queue) -> Vec<String> {
        let old = std::mem::replace(
            &mut self.work,
            Work {
                scope: life.child(),
                awaiting: BTreeMap::new(),
            },
        );
        old.scope.cancel(CancelCause::Cancelled);
        match queue {
            Queue::Keep => Vec::new(),
            Queue::Recall => recall(&mut self.posts),
            Queue::Discard => {
                self.posts.clear();
                Vec::new()
            }
        }
    }
}

enum Post {
    Human(String),
    Read(Read),
    Rewrite(Rewrite),
    Scoped { from: Scope, kind: Scoped },
}
pub(crate) enum Scoped {
    Result(AgentResult),
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
    fn source(&self) -> Source {
        match self {
            Self::Human(_) => Source::User,
            Self::Read(_) => Source::Read,
            Self::Rewrite(_) => Source::Rewrite,
            Self::Scoped { kind, .. } => match kind {
                Scoped::Result(_) => Source::Agent,
                Scoped::Surface { .. } => Source::Surface,
                Scoped::Wakeup { .. } => Source::Schedule,
                Scoped::Message(_) => Source::Message,
                Scoped::Nudge { .. } => Source::Nudge,
            },
        }
    }
}
fn wakeup(post: &Post, id: ScheduleId) -> bool {
    matches!(post, Post::Scoped { kind: Scoped::Wakeup { id: queued, .. }, .. } if *queued == id)
}
fn enqueue(posts: &mut VecDeque<Post>, post: Post) {
    let replace = match &post {
        Post::Scoped {
            kind: Scoped::Wakeup { id, .. },
            ..
        } => posts.iter().position(|p| wakeup(p, *id)),
        Post::Scoped {
            kind: Scoped::Nudge { .. },
            ..
        } => posts.iter().position(|p| {
            matches!(
                p,
                Post::Scoped {
                    kind: Scoped::Nudge { .. },
                    ..
                }
            )
        }),
        _ => None,
    };
    if let Some(index) = replace {
        posts[index] = post;
        return;
    }
    if let Post::Human(text) = &post
        && let Some(Post::Human(tail)) = posts.back_mut()
    {
        tail.push('\n');
        tail.push_str(text);
        return;
    }
    posts.push_back(post);
}
fn fence(posts: &mut VecDeque<Post>) {
    let before = posts.len();
    posts.retain(|post| !matches!(post, Post::Scoped { from, .. } if from.is_cancelled()));
    if posts.len() != before {
        let live = std::mem::take(posts);
        for post in live {
            enqueue(posts, post);
        }
    }
}
fn recall(posts: &mut VecDeque<Post>) -> Vec<String> {
    fence(posts);
    let mut prompts = Vec::new();
    let mut kept = VecDeque::new();
    for post in posts.drain(..) {
        match post {
            Post::Human(text) => prompts.push(text),
            other => enqueue(&mut kept, other),
        }
    }
    *posts = kept;
    prompts
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
        let state = self.shared.lock();
        state.work.scope.id() == self.scope.id() && !self.scope.is_cancelled()
    }
    pub(crate) fn adopt(&self, child: AgentId) -> Option<ScopeNode> {
        let mut state = self.shared.lock();
        if state.work.scope.id() != self.scope.id() || self.scope.is_cancelled() {
            return None;
        }
        state.work.awaiting.insert(child, 0);
        Some(state.work.scope.child())
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
            return false;
        }
        match &post {
            Post::Human(_) => state.last_exchange = Some(Instant::now()),
            Post::Scoped {
                kind: Scoped::Message(message),
                ..
            } => {
                state.last_exchange = Some(Instant::now());
                state.accepted = state.accepted.max(message.seq);
            }
            _ => {}
        }
        enqueue(&mut state.posts, post);
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
    pub(crate) fn retire(&self) -> bool {
        let state = self.shared.lock();
        self.shared.life.cancel(CancelCause::Cancelled);
        let replied = state.reply_since().is_some();
        drop(state);
        self.shared.notify();
        replied
    }
    pub(crate) fn queued(&self) -> Vec<String> {
        let state = self.shared.lock();
        if self.shared.life.is_cancelled() {
            return Vec::new();
        }
        state
            .posts
            .iter()
            .filter_map(|p| match p {
                Post::Human(text) => Some(text.clone()),
                Post::Read(r) => Some(r.to_string()),
                Post::Rewrite(r) => Some(r.to_string()),
                Post::Scoped { .. } => None,
            })
            .collect()
    }
    pub(crate) fn recall(&self) -> Vec<String> {
        let mut state = self.shared.lock();
        if self.shared.life.is_cancelled() {
            return Vec::new();
        }
        recall(&mut state.posts)
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
    pub(crate) fn answers(&self) -> u64 {
        self.shared.lock().answered
    }
    pub(crate) fn expect(&self, child: AgentId) -> u64 {
        let mut state = self.shared.lock();
        state.next_message += 1;
        let seq = state.next_message;
        state.work.awaiting.insert(child, seq);
        seq
    }
    pub(crate) fn unexpect(&self, child: AgentId, seq: u64) {
        let mut state = self.shared.lock();
        if state.work.awaiting.get(&child) == Some(&seq) {
            state.work.awaiting.remove(&child);
        }
    }
    pub(crate) fn lapse(&self, since: Instant) {
        let state = self.shared.lock();
        if !self.shared.life.is_cancelled() && state.reply_since() == Some(since) {
            self.shared.life.cancel(CancelCause::TimedOut);
            self.shared.notify();
        }
    }
    pub(crate) fn has_queued_wakeup(&self, id: ScheduleId) -> bool {
        let mut state = self.shared.lock();
        fence(&mut state.posts);
        state.posts.iter().any(|p| wakeup(p, id))
    }
    fn close_job(&self, id: JobId) {
        let mut state = self.shared.lock();
        if state.job.as_ref().is_some_and(|j| j.id == id) {
            state.job.take();
            drop(state);
            self.shared.notify();
        }
    }
}
impl Inbox {
    pub(crate) fn new(life: ScopeNode) -> Self {
        let work = Work {
            scope: life.child(),
            awaiting: BTreeMap::new(),
        };
        let exchange = work.scope.child();
        exchange.cancel(CancelCause::Cancelled);
        Self(Arc::new(Shared {
            life,
            state: Mutex::new(State {
                posts: VecDeque::new(),
                work,
                exchange,
                opening: None,
                job: None,
                next_job: 0,
                next_message: 0,
                accepted: 0,
                answered: 0,
                rest: None,
                reply: None,
                replied_at: None,
                last_exchange: None,
                waiting_for_input: true,
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
    pub(crate) fn life_child(&self) -> ScopeNode {
        self.0.life.child()
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
    pub(crate) fn draw(&self, park: impl Fn(&State) -> ParkMode) -> Option<Next> {
        let mut state = self.0.lock();
        loop {
            if self.0.life.is_cancelled() || state.job.as_ref().is_some_and(|j| j.stopped) {
                return None;
            }
            let mode = park(&state);
            if let Some(next) = state.pop() {
                if matches!(&next, Next::Item(item) if item.opens_exchange()) {
                    state.opening = Some(state.work.scope.child());
                }
                state.waiting_for_input = false;
                return Some(next);
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
        loop {
            match state.pop() {
                Some(Next::Rewrite(rewrite)) => {
                    state.posts.push_front(Post::Rewrite(rewrite));
                    break;
                }
                Some(next) => arrivals.push(next),
                None => break,
            }
        }
        drop(state);
        arrivals
    }
    pub(crate) fn open_exchange(&self) {
        let mut state = self.0.lock();
        state.exchange.cancel(CancelCause::Cancelled);
        // I3: use the scope minted at pop; never erase a racing Stop or Esc.
        state.exchange = state.opening.take().expect("an opening was minted at pop");
    }
    pub(crate) fn decline_opening(&self) {
        self.0.lock().opening.take();
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
    pub(crate) fn await_job(&self) -> Option<Job> {
        let mut state = self.0.lock();
        loop {
            if self.0.life.is_cancelled() {
                return None;
            }
            if state.job.is_none() && state.posts.iter().any(|p| matches!(p, Post::Human(_))) {
                state.next_job += 1;
                let id = JobId(state.next_job);
                state.job = Some(OpenJob { id, stopped: false });
                return Some(Job {
                    id,
                    mailbox: self.life_mailbox(),
                });
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
    pub(crate) fn deposit_reply(&self, reply: FOValue) -> Instant {
        let mut state = self.0.lock();
        let since = Instant::now();
        state.reply = Some(reply);
        state.replied_at = Some(since);
        since
    }
    pub(crate) fn disarm_reply_lease(&self) {
        self.0.lock().replied_at = None;
    }
    pub(crate) fn is_empty(&self) -> bool {
        let mut state = self.0.lock();
        fence(&mut state.posts);
        state.posts.is_empty()
    }
    pub(crate) fn source_depths(&self) -> Vec<(Source, u64)> {
        let mut rows: Vec<_> = Source::ALL.into_iter().map(|s| (s, 0)).collect();
        for post in &self.0.lock().posts {
            if let Some((_, count)) = rows.iter_mut().find(|(s, _)| *s == post.source()) {
                *count += 1;
            }
        }
        rows
    }
}

/// An open job, attended at most once; dropping it unattended closes it.
#[must_use]
pub struct Job {
    id: JobId,
    mailbox: Mailbox,
}
impl Job {
    pub fn id(&self) -> JobId {
        self.id
    }
    pub(crate) fn belongs_to(&self, mailbox: &Mailbox) -> bool {
        Arc::ptr_eq(&self.mailbox.shared, &mailbox.shared)
    }
    pub(crate) fn close_if_running(&self) -> bool {
        let mut state = self.mailbox.shared.lock();
        if state
            .job
            .as_ref()
            .is_some_and(|job| job.id == self.id && !job.stopped)
        {
            state.job.take();
            drop(state);
            self.mailbox.shared.notify();
            true
        } else {
            false
        }
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.mailbox.close_job(self.id);
    }
}

#[cfg(test)]
mod tests;
