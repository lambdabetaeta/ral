//! One owner, one queue, and observed busy periods.

use super::post::Source;
use super::{AgentMessage, Lease, Read, Rewrite, ScheduleId};
use crate::provider::Pick;
use crate::record::{AgentId, ChildStatus, Emitter as Recorder, Forensic, JobId};
use ral_core::data::FOValue;
use ral_core::process::{CancelCause, Scope, ScopeNode};
use ral_core::sync::{CondvarExt, LockExt};
use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

const PARK_POLL: Duration = Duration::from_millis(100);

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;

pub(crate) enum Input {
    Human(String),
    Message(AgentMessage),
    Child {
        id: AgentId,
        name: Arc<str>,
        status: ChildStatus,
        elapsed: Duration,
    },
    Gone(AgentId),
    Wakeup {
        id: ScheduleId,
        epoch: u64,
        text: String,
    },
    Surface {
        generation: u64,
        values: Vec<FOValue>,
    },
}

pub enum Command {
    Read(Read),
    Rewrite(Rewrite),
    Model(Pick),
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(read) => fmt::Display::fmt(read, f),
            Self::Rewrite(rewrite) => fmt::Display::fmt(rewrite, f),
            Self::Model(pick) => write!(f, "/model {}", pick.choice.model),
        }
    }
}

/// The queue's element, in the order posted.
enum Queued {
    Input(Input),
    Command(Command),
}

/// What the mid-exchange drain takes: it holds at a rewrite.
pub(crate) enum Drained {
    Input(Input),
    Read(Read),
    Model(Pick),
}

/// The owner's end; dropping it ends the life.
pub(crate) struct AgentMailbox {
    shared: Arc<Shared>,
    record: Recorder,
}

/// The address others post to and stop through.
#[derive(Clone)]
pub(crate) struct AgentAddress(Arc<Shared>);

#[derive(Clone)]
pub(crate) struct WeakAgentAddress(Weak<Shared>);

struct Shared {
    life: ScopeNode,
    admission: Admission,
    queue: Mutex<Queue>,
    signal: Condvar,
}

struct Queue {
    pick: Pick,
    items: VecDeque<Queued>,
    phase: Phase,
    exchange: Option<ScopeNode>,
    waiting_for_input: bool,
    busy: bool,
    report: Option<ChildStatus>,
    owes_parent: bool,
    last_exchange: Option<Instant>,
}

enum Phase {
    /// `released`: a `Gated` agent may open one job.
    Resting {
        since: Instant,
        released: bool,
    },
    Working(OpenJob),
}

struct OpenJob {
    id: JobId,
    stopped: bool,
}

/// What other threads may read of an agent, built from its queue.
#[derive(Clone)]
pub(crate) struct Status {
    pub(crate) pick: Pick,
    pub(crate) rest: Option<Instant>,
    pub(crate) waiting_for_input: bool,
    pub(crate) busy: bool,
    pub(crate) job: Option<JobId>,
    pub(crate) report: Option<ChildStatus>,
    pub(crate) owes_parent: bool,
    pub(crate) last_exchange: Option<Instant>,
}

impl Status {
    /// For views only: a snapshot may lag.
    pub(crate) fn working(&self) -> bool {
        self.owes_parent || self.job.is_some()
    }
}

/// What keeps the owner from settling (A10).
#[derive(Clone, Copy)]
pub(crate) struct Rest {
    /// Some unjoined helper owes us a report.
    pub pending: bool,
    pub schedules: bool,
}

pub(crate) enum Next {
    Take { input: Input, exchange: Lease },
    Command(Command),
    Settled { stopped: bool },
    Ended,
}

/// Which inputs may take an agent out of rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Free,
    Gated,
    Parent { idle: Duration },
}

/// Why a post was refused.
#[derive(Debug)]
pub(crate) enum Refused {
    Ended,
    Resting,
}

impl Input {
    fn is_from_parent(&self) -> bool {
        matches!(self, Self::Message(message) if message.from_parent)
    }
}

impl Queued {
    fn source(&self) -> Source {
        match self {
            Self::Input(Input::Human(_)) => Source::User,
            Self::Input(Input::Message(_)) => Source::Message,
            Self::Input(Input::Child { .. } | Input::Gone(_)) => Source::Agent,
            Self::Input(Input::Wakeup { .. }) => Source::Schedule,
            Self::Input(Input::Surface { .. }) => Source::Surface,
            Self::Command(Command::Read(_)) => Source::Read,
            Self::Command(Command::Rewrite(_)) => Source::Rewrite,
            Self::Command(Command::Model(_)) => Source::Model,
        }
    }

    fn is_human(&self) -> bool {
        matches!(self, Self::Input(Input::Human(_)))
    }

    /// A6: quiet inputs bypass every admission.
    fn admitted(&self, admission: Admission, released: bool) -> bool {
        match self {
            Self::Command(_) | Self::Input(Input::Child { .. } | Input::Gone(_)) => true,
            Self::Input(input) => match admission {
                Admission::Free => true,
                Admission::Gated => released,
                Admission::Parent { .. } => {
                    matches!(input, Input::Human(_)) || input.is_from_parent()
                }
            },
        }
    }
}

impl Queue {
    fn push(&mut self, item: Queued) {
        self.items.push_back(item);
        self.waiting_for_input = false;
    }

    fn take(&mut self, index: usize) -> Queued {
        match self.items.remove(index).expect("the selected input exists") {
            Queued::Input(input) => Queued::Input(self.coalesce(input, index)),
            command @ Queued::Command(_) => command,
        }
    }

    /// Human text queued at `index` joins a human `input`.
    fn coalesce(&mut self, mut input: Input, index: usize) -> Input {
        if let Input::Human(text) = &mut input {
            while let Some(Queued::Input(Input::Human(_))) = self.items.get(index) {
                if let Some(Queued::Input(Input::Human(next))) = self.items.remove(index) {
                    text.push('\n');
                    text.push_str(&next);
                }
            }
        }
        input
    }

    fn texts(&self) -> Vec<String> {
        let mut texts = Vec::new();
        let mut human_tail = false;
        for item in &self.items {
            match item {
                Queued::Input(Input::Human(text)) => Self::join(&mut texts, human_tail, text),
                Queued::Command(command) => texts.push(command.to_string()),
                Queued::Input(_) => {}
            }
            human_tail = item.is_human();
        }
        texts
    }

    fn recall(&mut self) -> Vec<String> {
        let mut texts = Vec::new();
        let mut human_tail = false;
        self.items.retain(|item| {
            let human = item.is_human();
            if let Queued::Input(Input::Human(text)) = item {
                Self::join(&mut texts, human_tail, text);
            }
            human_tail = human;
            !human
        });
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
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock_ignore_poison()
    }

    /// Under the lock, so a post lands first or is refused.
    fn end(&self, queue: MutexGuard<'_, Queue>, cause: CancelCause) {
        self.life.cancel(cause);
        drop(queue);
        self.notify();
    }

    fn notify(&self) {
        self.signal.notify_all();
    }
}

impl AgentMailbox {
    pub(crate) fn new(life: ScopeNode, record: Recorder, admission: Admission, pick: Pick) -> Self {
        Self {
            shared: Arc::new(Shared {
                life,
                admission,
                queue: Mutex::new(Queue {
                    pick,
                    items: VecDeque::new(),
                    phase: Phase::Resting {
                        since: Instant::now(),
                        released: false,
                    },
                    exchange: None,
                    waiting_for_input: true,
                    busy: false,
                    report: None,
                    owes_parent: matches!(admission, Admission::Parent { .. }),
                    last_exchange: None,
                }),
                signal: Condvar::new(),
            }),
            record,
        }
    }

    pub(crate) fn publish_pick(&self, pick: Pick) {
        self.shared.lock().pick = pick;
    }

    pub(crate) fn address(&self) -> AgentAddress {
        AgentAddress(self.shared.clone())
    }

    pub(crate) fn life(&self) -> Scope {
        (*self.shared.life).clone()
    }

    pub(crate) fn next(&self, rest: Rest) -> Next {
        let mut queue = self.shared.lock();
        loop {
            if self.shared.life.is_cancelled() {
                return Next::Ended;
            }
            queue.busy = rest.pending;
            let index = match &queue.phase {
                Phase::Working(_) => (!queue.items.is_empty()).then_some(0),
                Phase::Resting { released, .. } => queue
                    .items
                    .iter()
                    .position(|item| item.admitted(self.shared.admission, *released)),
            };
            if let Some(index) = index {
                queue.waiting_for_input = false;
                return match queue.take(index) {
                    Queued::Command(command) => Next::Command(command),
                    Queued::Input(input) => {
                        let exchange = Lease::under(&self.shared.life);
                        queue.exchange = Some(exchange.node());
                        Next::Take { input, exchange }
                    }
                };
            }
            let wait = match (&queue.phase, self.shared.admission) {
                (Phase::Working(job), _) if !rest.pending && !rest.schedules => {
                    let stopped = job.stopped;
                    let settled = Forensic::JobSettled {
                        job: job.id,
                        stopped,
                    };
                    if self.record.emit(settled).is_err() {
                        return Next::Ended;
                    }
                    queue.phase = Phase::Resting {
                        since: Instant::now(),
                        released: false,
                    };
                    queue.waiting_for_input = false;
                    return Next::Settled { stopped };
                }
                (Phase::Resting { since, .. }, Admission::Parent { idle }) => {
                    let remaining = idle.saturating_sub(since.elapsed());
                    if remaining.is_zero() {
                        self.shared.end(queue, CancelCause::TimedOut);
                        return Next::Ended;
                    }
                    PARK_POLL.min(remaining)
                }
                _ => PARK_POLL,
            };
            queue.waiting_for_input = !rest.pending && !rest.schedules;
            (queue, _) = self.shared.signal.wait_timeout_ignore_poison(queue, wait);
        }
    }

    /// Open a job unless one is open, only for an input the owner accepted.
    pub(crate) fn begin(&self) -> bool {
        let mut queue = self.shared.lock();
        if self.shared.life.is_cancelled() {
            return false;
        }
        if let Phase::Resting { .. } = queue.phase {
            let Ok(recorded) = self.record.emit(Forensic::JobOpened) else {
                return false;
            };
            queue.phase = Phase::Working(OpenJob {
                id: JobId::from(recorded.locus().seq().commit()),
                stopped: false,
            });
        }
        queue.last_exchange = Some(Instant::now());
        true
    }

    /// Inputs and reads up to the first rewrite, unless the current exchange is cancelled.
    pub(crate) fn drain(&self) -> Vec<Drained> {
        let mut queue = self.shared.lock();
        let mut drained = Vec::new();
        if queue
            .exchange
            .as_ref()
            .is_some_and(|exchange| exchange.is_cancelled())
        {
            return drained;
        }
        loop {
            drained.push(match queue.items.pop_front() {
                Some(Queued::Input(input)) => Drained::Input(queue.coalesce(input, 0)),
                Some(Queued::Command(Command::Read(read))) => Drained::Read(read),
                Some(Queued::Command(Command::Model(pick))) => Drained::Model(pick),
                Some(rewrite @ Queued::Command(Command::Rewrite(_))) => {
                    queue.items.push_front(rewrite);
                    break;
                }
                None => break,
            });
        }
        drop(queue);
        drained
    }

    /// A queued input that will prompt the model (A12).
    pub(crate) fn has_input(&self) -> bool {
        self.shared.lock().items.iter().any(|item| {
            matches!(
                item,
                Queued::Input(Input::Human(_) | Input::Message(_) | Input::Child { .. })
            )
        })
    }

    /// A non-renewing snapshot; adjacent human fragments count as one input.
    pub(crate) fn source_depths(&self) -> Vec<(Source, u64)> {
        let queue = self.shared.lock();
        let mut depths: Vec<_> = Source::ALL.into_iter().map(|source| (source, 0)).collect();
        let mut human_tail = false;
        for item in &queue.items {
            let human = item.is_human();
            if !(human && human_tail)
                && let Some((_, count)) = depths
                    .iter_mut()
                    .find(|(source, _)| *source == item.source())
            {
                *count += 1;
            }
            human_tail = human;
        }
        drop(queue);
        depths
    }

    /// `/clear` preserves human text, peer messages and commands.
    pub(crate) fn clear(&self) {
        self.shared.lock().items.retain(|item| {
            matches!(
                item,
                Queued::Input(Input::Human(_) | Input::Message(_)) | Queued::Command(_)
            )
        });
    }
}

impl Drop for AgentMailbox {
    fn drop(&mut self) {
        let mut queue = self.shared.lock();
        queue.items.clear();
        self.shared.end(queue, CancelCause::Cancelled);
    }
}

impl WeakAgentAddress {
    pub(crate) fn upgrade(&self) -> Option<AgentAddress> {
        self.0.upgrade().map(AgentAddress)
    }
}

impl AgentAddress {
    pub(crate) fn downgrade(&self) -> WeakAgentAddress {
        WeakAgentAddress(Arc::downgrade(&self.0))
    }

    pub(crate) fn post(&self, input: Input) -> Result<(), Refused> {
        let mut queue = self.0.lock();
        if self.0.life.is_cancelled() {
            return Err(Refused::Ended);
        }
        if matches!(self.0.admission, Admission::Parent { .. })
            && matches!(queue.phase, Phase::Resting { .. })
            && matches!(&input, Input::Message(message) if !message.from_parent)
        {
            return Err(Refused::Resting);
        }
        if input.is_from_parent() {
            queue.owes_parent = true;
        }
        queue.push(Queued::Input(input));
        drop(queue);
        self.0.notify();
        Ok(())
    }

    pub(crate) fn command(&self, command: Command) -> Result<(), Refused> {
        let mut queue = self.0.lock();
        if self.0.life.is_cancelled() {
            return Err(Refused::Ended);
        }
        queue.push(Queued::Command(command));
        drop(queue);
        self.0.notify();
        Ok(())
    }

    /// `prepare` may lock the registry leaf, but must not call a mailbox method.
    pub(crate) fn post_wakeup_with(
        &self,
        id: ScheduleId,
        prepare: impl FnOnce(bool) -> Option<(u64, String)>,
    ) -> Result<(), Refused> {
        let mut queue = self.0.lock();
        if self.0.life.is_cancelled() {
            return Err(Refused::Ended);
        }
        let already_queued = queue.items.iter().any(
            |item| matches!(item, Queued::Input(Input::Wakeup { id: queued, .. }) if *queued == id),
        );
        if let Some((epoch, text)) = prepare(already_queued) {
            queue.push(Queued::Input(Input::Wakeup { id, epoch, text }));
            self.0.notify();
        }
        drop(queue);
        Ok(())
    }

    pub(crate) fn steer(&self, text: String) -> Result<(), Refused> {
        self.post(Input::Human(text))
    }

    pub(crate) fn queued(&self) -> Vec<String> {
        let queue = self.0.lock();
        if self.0.life.is_cancelled() {
            Vec::new()
        } else {
            queue.texts()
        }
    }

    pub(crate) fn recall(&self) -> Vec<String> {
        let mut queue = self.0.lock();
        if self.0.life.is_cancelled() {
            Vec::new()
        } else {
            queue.recall()
        }
    }

    /// Stop the open job, leaving the queue in place.
    pub(crate) fn stop_current(&self) -> bool {
        self.stop_with(None, |_| ()).is_some()
    }

    /// Stop `job` if it is open, taking back its queued human text.
    pub(crate) fn stop(&self, job: JobId) -> Option<Vec<String>> {
        self.stop_with(Some(job), Queue::recall)
    }

    fn stop_with<T>(&self, job: Option<JobId>, take: impl FnOnce(&mut Queue) -> T) -> Option<T> {
        let mut queue = self.0.lock();
        if self.0.life.is_cancelled() {
            return None;
        }
        let Phase::Working(open) = &mut queue.phase else {
            return None;
        };
        if job.is_some_and(|id| open.id != id) {
            return None;
        }
        open.stopped = true;
        if let Some(exchange) = &queue.exchange {
            exchange.cancel(CancelCause::Interrupted);
        }
        let taken = take(&mut queue);
        drop(queue);
        self.0.notify();
        Some(taken)
    }

    /// Admit one job on a gated root at rest; `false` once ended.
    pub(crate) fn release(&self) -> bool {
        let mut queue = self.0.lock();
        if self.0.life.is_cancelled() {
            return false;
        }
        if self.0.admission == Admission::Gated
            && let Phase::Resting { released, .. } = &mut queue.phase
        {
            *released = true;
            drop(queue);
            self.0.notify();
        }
        true
    }

    pub(crate) fn end(&self, cause: CancelCause) {
        self.0.end(self.0.lock(), cause);
    }

    pub(crate) fn is_live(&self) -> bool {
        !self.0.life.is_cancelled()
    }

    pub(crate) fn status(&self) -> Status {
        let queue = self.0.lock();
        let (rest, job) = match &queue.phase {
            Phase::Resting { since, .. } => (Some(*since), None),
            Phase::Working(job) => (None, Some(job.id)),
        };
        Status {
            pick: queue.pick.clone(),
            rest,
            waiting_for_input: queue.waiting_for_input,
            busy: queue.busy,
            job,
            report: queue.report.clone(),
            owes_parent: queue.owes_parent,
            last_exchange: queue.last_exchange,
        }
    }

    /// A5: publish `status`; if owed, post `child` to `parent` under this
    /// lock. Reports climb from the helper's lock to its parent's.
    pub(crate) fn report(&self, status: ChildStatus, parent: Option<(&Self, Input)>) {
        let mut queue = self.0.lock();
        queue.report = Some(status);
        if queue.owes_parent {
            if let Some((parent, child)) = parent {
                let _posted = parent.post(child);
            }
            queue.owes_parent = queue
                .items
                .iter()
                .any(|item| matches!(item, Queued::Input(input) if input.is_from_parent()));
        }
    }
}
