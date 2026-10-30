//! The agent's cell: recorded facts and their pending payloads under one lock.

use super::post::Source;
use super::{AgentMessage, Lease, Read, Rewrite, ScheduleId, notice, surface_notice};
use crate::provider::{Pick, Selected};
use crate::record::{
    AgentId, ChildStatus, Control, Emitter as Recorder, Folds, JobId, Mail, Model, NonEmpty, Post,
    PostId, Schedule, Schedules, Unavailable, Voice,
};
use ral_core::data::FOValue;
use ral_core::process::{CancelCause, Scope, ScopeNode};
use ral_core::sync::LockExt;
use std::borrow::Borrow;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
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
    Wakeup {
        id: ScheduleId,
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

pub(crate) enum Queued {
    Input(Input),
    Command(Command),
}

#[allow(
    clippy::large_enum_variant,
    reason = "the queue carries post payloads inline; Gone is only a join notice"
)]
enum Item {
    Posted(PostId, Queued),
    Gone(AgentId),
}

pub(crate) enum Drained {
    Input(Input),
    Gone(AgentId),
    Read(Read),
    Model(Pick),
}

/// Dropping the owner's end ends the life.
pub(crate) struct AgentMailbox {
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub(crate) struct AgentAddress(Arc<Shared>);

#[derive(Clone)]
pub(crate) struct WeakAgentAddress(Weak<Shared>);

struct Shared {
    id: AgentId,
    parent: Option<AgentAddress>,
    life: ScopeNode,
    admission: Admission,
    journal: Recorder,
    // A panic can leave these folds disagreeing with the journal: keep poison.
    cell: Mutex<Cell>,
    signal: Condvar,
    /// At most one `Failed` notice from a failed report reaches the parent.
    failure_noticed: AtomicBool,
    #[cfg(test)]
    after_report_commit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent loop observations and stop/release marks, not a shared phase"
)]
struct Cell {
    mail: Mail,
    schedules: Schedules,
    model: Model,
    queue: VecDeque<Item>,
    stopped: bool,
    released: bool,
    since: Instant,
    exchange: Option<ScopeNode>,
    waiting_for_input: bool,
    busy: bool,
    last_exchange: Option<Instant>,
    rung: BTreeMap<String, ScheduleId>,
}

/// A capability borrowed only inside a transaction's closure.
pub(crate) struct Txn<'a> {
    shared: &'a Arc<Shared>,
    cell: MutexGuard<'a, Cell>,
}

#[derive(Clone)]
pub(crate) struct Status {
    pub(crate) selected: Selected,
    pub(crate) rest: Option<Instant>,
    pub(crate) waiting_for_input: bool,
    pub(crate) busy: bool,
    pub(crate) job: Option<JobId>,
    pub(crate) report: Option<ChildStatus>,
    pub(crate) owes: bool,
    pub(crate) last_exchange: Option<Instant>,
}

impl Status {
    pub(crate) fn working(&self) -> bool {
        self.owes || self.job.is_some()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Rest {
    pub pending: bool,
}

pub(crate) enum Next {
    Take { input: Input, exchange: Lease },
    Command(Command),
    Rung,
    Gone(AgentId),
    Settled { stopped: bool },
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Free,
    Gated,
    Parent { idle: Duration },
}

#[derive(Debug)]
pub(crate) enum Refused {
    Ended,
    Resting,
}

impl Queued {
    pub(crate) fn posted(&self) -> Post {
        let (voice, text, from) = match self {
            Self::Input(Input::Human(text)) => (Some(Voice::Human), text.clone(), None),
            Self::Input(Input::Message(message)) => {
                (Some(Voice::Peer), message.render(), Some(message.from))
            }
            Self::Input(Input::Child {
                id,
                name,
                status,
                elapsed,
            }) => (
                Some(Voice::Harness),
                notice(name, &status.settlement(), *elapsed),
                Some(*id),
            ),
            Self::Input(Input::Wakeup { text, .. }) => (Some(Voice::Wakeup), text.clone(), None),
            Self::Input(Input::Surface { values, .. }) => {
                (Some(Voice::Harness), surface_notice(values), None)
            }
            Self::Command(command) => (None, command.to_string(), None),
        };
        Post { voice, text, from }
    }

    fn source(&self) -> Source {
        match self {
            Self::Input(Input::Human(_)) => Source::User,
            Self::Input(Input::Message(_)) => Source::Message,
            Self::Input(Input::Child { .. }) => Source::Agent,
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
}

impl Cell {
    fn admitted(&self, item: &Item, admission: Admission) -> bool {
        let Item::Posted(id, queued) = item else {
            return true;
        };
        if self.mail.job().is_some() {
            return true;
        }
        match queued {
            Queued::Command(_) | Queued::Input(Input::Child { .. }) => true,
            Queued::Input(input) => match admission {
                Admission::Free => true,
                Admission::Gated => self.released,
                Admission::Parent { .. } => {
                    matches!(input, Input::Human(_))
                        || self
                            .mail
                            .by_parent(self.mail.post(*id).expect("queued post is pending"))
                }
            },
        }
    }

    fn next_index(&self, admission: Admission) -> Option<usize> {
        self.queue
            .iter()
            .position(|item| self.admitted(item, admission))
    }

    fn schedules_pending(&self) -> bool {
        self.mail.returns() && !self.schedules.is_empty()
    }
}

/// How an item stands to a run of human posts: a run is maximal over posts,
/// so an `Aside` (not a post: a `Gone`) neither joins nor breaks one.
enum Part<T, H> {
    Human(H),
    Post(T),
    Aside(T),
}

/// A run of human posts, or one other item. An aside met inside a run follows it.
enum Group<T, H> {
    Run(NonEmpty<H>),
    One(T),
}

fn runs<I, T, H>(
    items: impl IntoIterator<Item = I>,
    part: impl Fn(I) -> Part<T, H>,
) -> Vec<Group<T, H>> {
    fn close<T, H>(groups: &mut Vec<Group<T, H>>, open: Option<(NonEmpty<H>, Vec<T>)>) {
        if let Some((run, asides)) = open {
            groups.push(Group::Run(run));
            groups.extend(asides.into_iter().map(Group::One));
        }
    }
    let mut groups = Vec::new();
    let mut open: Option<(NonEmpty<H>, Vec<T>)> = None;
    for item in items {
        match (part(item), open.as_mut()) {
            (Part::Human(human), Some((run, _))) => run.rest.push(human),
            (Part::Human(human), None) => open = Some((NonEmpty::one(human), Vec::new())),
            (Part::Aside(aside), Some((_, asides))) => asides.push(aside),
            (Part::Aside(item), None) => groups.push(Group::One(item)),
            (Part::Post(item), _) => {
                close(&mut groups, open.take());
                groups.push(Group::One(item));
            }
        }
    }
    close(&mut groups, open);
    groups
}

fn joined<S: Borrow<str>>(texts: impl IntoIterator<Item = S>) -> String {
    texts.into_iter().collect::<Vec<_>>().join("\n")
}

fn pending(mail: &Mail) -> Vec<Group<&Post, (PostId, &Post)>> {
    runs(mail.pending(), |(id, post)| {
        if post.voice == Some(Voice::Human) {
            Part::Human((id, post))
        } else {
            Part::Post(post)
        }
    })
}

impl Item {
    fn part(&self) -> Part<&Self, PostId> {
        match self {
            Self::Posted(id, queued) if queued.is_human() => Part::Human(*id),
            Self::Posted(..) => Part::Post(self),
            Self::Gone(_) => Part::Aside(self),
        }
    }

    fn source(&self) -> Source {
        match self {
            Self::Posted(_, queued) => queued.source(),
            Self::Gone(_) => Source::Agent,
        }
    }
}

impl Txn<'_> {
    pub(crate) fn mail(&self) -> &Mail {
        &self.cell.mail
    }
    pub(crate) fn schedules(&self) -> &Schedules {
        &self.cell.schedules
    }
    pub(crate) fn model(&self) -> &Model {
        &self.cell.model
    }
    pub(crate) fn is_live(&self) -> bool {
        !self.shared.life.is_cancelled()
    }
    pub(crate) fn weak(&self) -> WeakAgentAddress {
        WeakAgentAddress(Arc::downgrade(self.shared))
    }

    /// Only a post can append a `Posted` and install its payload together;
    /// a post's line removes no post, so nothing is pruned.
    fn post_with(&mut self, item: Queued, change: Option<Control>) -> Result<PostId, Unavailable> {
        let post = item.posted();
        let cell = &mut *self.cell;
        let commit = self.shared.journal.commit_to(
            Folds {
                mail: &mut cell.mail,
                schedules: &mut cell.schedules,
                model: &mut cell.model,
            },
            NonEmpty {
                first: Control::Posted(post),
                rest: change.into_iter().collect(),
            },
            Vec::new(),
        )?;
        let id = PostId::of(&commit);
        self.cell.queue.push_back(Item::Posted(id, item));
        self.cell.waiting_for_input = false;
        Ok(id)
    }

    pub(crate) fn post(&mut self, item: Queued) -> Result<PostId, Refused> {
        if !self.is_live() {
            return Err(Refused::Ended);
        }
        if matches!(self.shared.admission, Admission::Parent { .. })
            && self.mail().job().is_none()
            && matches!(&item, Queued::Input(Input::Message(_)))
            && !self.mail().by_parent(&item.posted())
        {
            return Err(Refused::Resting);
        }
        self.post_with(item, None).map_err(|_| Refused::Ended)
    }

    /// Every non-post write removes exactly the payloads its line withdrew or took.
    fn commit(&mut self, line: NonEmpty<Control>) -> Result<Vec<Queued>, Unavailable> {
        assert!(
            !line
                .iter()
                .any(|record| matches!(record, Control::Posted(_))),
            "only post may append Posted with its payload"
        );
        let cell = &mut *self.cell;
        drop(self.shared.journal.commit_to(
            Folds {
                mail: &mut cell.mail,
                schedules: &mut cell.schedules,
                model: &mut cell.model,
            },
            line,
            Vec::new(),
        )?);
        Ok(self.prune())
    }

    fn prune(&mut self) -> Vec<Queued> {
        let cell = &mut *self.cell;
        let mut pruned = Vec::new();
        cell.queue = std::mem::take(&mut cell.queue)
            .into_iter()
            .filter_map(|item| match item {
                Item::Posted(id, queued) if cell.mail.post(id).is_none() => {
                    pruned.push(queued);
                    None
                }
                item => Some(item),
            })
            .collect();
        pruned
    }

    fn retire_schedules(&self) -> Vec<Control> {
        self.schedules()
            .iter()
            .map(|(label, _)| Control::Unscheduled {
                label: label.to_string(),
            })
            .collect()
    }

    pub(crate) fn report(
        &mut self,
        status: ChildStatus,
        notice: impl FnOnce(ChildStatus) -> Input,
    ) -> Result<(), Unavailable> {
        let owed = self.mail().owes();
        let mut rest = self.retire_schedules();
        let withdrawn = if self.is_live() {
            self.cell
                .queue
                .iter()
                .filter_map(|item| match item {
                    Item::Posted(id, Queued::Input(Input::Wakeup { .. })) => Some(*id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        } else {
            self.mail().pending().map(|(id, _)| id).collect()
        };
        if let Some(posts) = NonEmpty::collect(withdrawn) {
            rest.push(Control::Withdrawn { posts });
        }
        if let Err(failure) = self.commit(NonEmpty {
            first: Control::Status {
                status: status.clone(),
            },
            rest,
        }) {
            if owed {
                self.shared
                    .report_failure(notice(ChildStatus::Failed(failure.to_string())));
            }
            return Err(failure);
        }
        #[cfg(test)]
        {
            let hook = self
                .shared
                .after_report_commit
                .lock_strict()
                .unwrap()
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
        if owed && let Some(parent) = &self.shared.parent {
            let _posted = parent.post(notice(status));
        }
        Ok(())
    }

    pub(crate) fn schedule(
        &mut self,
        label: String,
        schedule: Schedule,
    ) -> Result<bool, Unavailable> {
        if self.schedules().get(&label).is_some() {
            return Ok(false);
        }
        drop(self.commit(NonEmpty::one(Control::Scheduled { label, schedule }))?);
        Ok(true)
    }

    pub(crate) fn unschedule(&mut self, label: &str) -> Result<bool, Unavailable> {
        if self.schedules().get(label).is_none() {
            return Ok(false);
        }
        drop(self.commit(NonEmpty::one(Control::Unscheduled {
            label: label.to_string(),
        }))?);
        Ok(true)
    }

    pub(crate) fn fire(
        &mut self,
        label: &str,
        id: ScheduleId,
        next_unix_ms: Option<u64>,
        text: String,
    ) -> Result<bool, Unavailable> {
        let Some(schedule) = self.schedules().get(label) else {
            return Ok(false);
        };
        let posted = !self.cell.queue.iter().any(|item| {
            matches!(item,
            Item::Posted(_, Queued::Input(Input::Wakeup { id: queued, .. })) if *queued == id)
        });
        let change = match next_unix_ms {
            Some(next_unix_ms) => Control::Scheduled {
                label: label.to_string(),
                schedule: Schedule {
                    next_unix_ms,
                    fires: schedule.fires + u64::from(posted),
                    ..schedule.clone()
                },
            },
            None => Control::Unscheduled {
                label: label.to_string(),
            },
        };
        if posted {
            self.post_with(Queued::Input(Input::Wakeup { id, text }), Some(change))?;
        } else {
            drop(self.commit(NonEmpty::one(change))?);
        }
        Ok(true)
    }

    pub(crate) fn rung(&mut self) -> BTreeMap<String, ScheduleId> {
        std::mem::take(&mut self.cell.rung)
    }

    pub(crate) fn change_model(&mut self, selected: Selected) -> Result<(), Unavailable> {
        drop(self.commit(NonEmpty::one(Control::ModelChanged { selected }))?);
        Ok(())
    }

    pub(crate) fn narrow(&mut self, model: &str, limit: u64) -> Result<bool, Unavailable> {
        if !self.model().narrows(model, limit) {
            return Ok(false);
        }
        drop(self.commit(NonEmpty::one(Control::WindowNarrowed {
            model: model.to_string(),
            context_window: limit,
        }))?);
        Ok(true)
    }

    pub(crate) fn clear(&mut self) -> Result<(), Unavailable> {
        let posts = NonEmpty::collect(self.cell.queue.iter().filter_map(|item| match item {
            Item::Posted(
                id,
                Queued::Input(Input::Child { .. } | Input::Wakeup { .. } | Input::Surface { .. }),
            ) => Some(*id),
            _ => None,
        }));
        let line = posts
            .map(|posts| Control::Withdrawn { posts })
            .into_iter()
            .chain(self.retire_schedules());
        if let Some(line) = NonEmpty::collect(line) {
            drop(self.commit(line)?);
        }
        self.cell
            .queue
            .retain(|item| !matches!(item, Item::Gone(_)));
        Ok(())
    }

    fn recall(&mut self) -> Result<Vec<String>, Unavailable> {
        let runs: Vec<_> = pending(self.mail())
            .into_iter()
            .filter_map(|group| match group {
                Group::Run(run) => Some(run),
                Group::One(_) => None,
            })
            .collect();
        let texts = runs
            .iter()
            .map(|run| joined(run.iter().map(|(_, post)| post.text.as_str())))
            .collect();
        if let Some(posts) = NonEmpty::collect(runs.iter().flatten().map(|(id, _)| *id)) {
            drop(self.commit(NonEmpty::one(Control::Withdrawn { posts }))?);
        }
        Ok(texts)
    }

    fn take(&mut self, index: usize) -> Result<Queued, Unavailable> {
        let posts = match runs(self.cell.queue.range(index..), Item::part)
            .into_iter()
            .next()
        {
            Some(Group::Run(posts)) => posts,
            Some(Group::One(Item::Posted(id, _))) => NonEmpty::one(*id),
            Some(Group::One(Item::Gone(_))) | None => unreachable!("Gone is popped separately"),
        };
        let taken = self.commit(NonEmpty::one(Control::Taken { posts }))?;
        let first = runs(taken, |queued| match queued {
            Queued::Input(Input::Human(text)) => Part::Human(text),
            queued => Part::Post(queued),
        })
        .into_iter()
        .next()
        .expect("a taken post has its payload");
        Ok(match first {
            Group::Run(texts) => Queued::Input(Input::Human(joined(texts))),
            Group::One(queued) => queued,
        })
    }

    /// Stops the open job: `job`, or whichever is open when `None`.
    fn stop(&mut self, job: Option<JobId>) -> bool {
        let matches = self.is_live()
            && self
                .mail()
                .job()
                .is_some_and(|open| job.is_none_or(|job| job == open));
        if matches {
            self.cell.stopped = true;
            if let Some(exchange) = &self.cell.exchange {
                exchange.cancel(CancelCause::Interrupted);
            }
        }
        matches
    }

    fn drain(&mut self) -> Result<Vec<Drained>, Unavailable> {
        if self
            .cell
            .exchange
            .as_ref()
            .is_some_and(|exchange| exchange.is_cancelled())
        {
            return Ok(Vec::new());
        }
        let prefix: Vec<_> = self
            .cell
            .queue
            .iter()
            .take_while(|item| {
                !matches!(item, Item::Posted(_, Queued::Command(Command::Rewrite(_))))
            })
            .map(|item| match item {
                Item::Posted(id, _) => Ok(*id),
                Item::Gone(id) => Err(*id),
            })
            .collect();
        let posts = NonEmpty::collect(prefix.iter().filter_map(|slot| slot.ok()));
        let mut taken = match posts {
            Some(posts) => self.commit(NonEmpty::one(Control::Taken { posts }))?,
            None => Vec::new(),
        }
        .into_iter();
        let gones = prefix.iter().filter(|slot| slot.is_err()).count();
        debug_assert!(
            self.cell
                .queue
                .range(..gones)
                .all(|item| matches!(item, Item::Gone(_)))
        );
        drop(self.cell.queue.drain(..gones));
        let slots = prefix
            .into_iter()
            .map(|slot| slot.map(|_| taken.next().expect("a taken post has its payload")));
        Ok(runs(slots, |slot| match slot {
            Ok(Queued::Input(Input::Human(text))) => Part::Human(text),
            Ok(Queued::Input(input)) => Part::Post(Drained::Input(input)),
            Ok(Queued::Command(Command::Read(read))) => Part::Post(Drained::Read(read)),
            Ok(Queued::Command(Command::Model(pick))) => Part::Post(Drained::Model(pick)),
            Ok(Queued::Command(Command::Rewrite(_))) => unreachable!("drain holds at rewrites"),
            Err(id) => Part::Aside(Drained::Gone(id)),
        })
        .into_iter()
        .map(|group| match group {
            Group::Run(texts) => Drained::Input(Input::Human(joined(texts))),
            Group::One(drained) => drained,
        })
        .collect())
    }
}

impl Shared {
    fn report_failure(&self, notice: Input) {
        if self.failure_noticed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(parent) = &self.parent {
            let _posted = parent.post(notice);
        }
    }

    fn poisoned(&self) -> Unavailable {
        let failure = self.journal.cell_poisoned(format!(
            "a thread panicked inside agent {}'s transaction, so its state may no longer match its log; it has ended, and resuming the run rebuilds its state from the log", self.id));
        self.life.cancel(CancelCause::Cancelled);
        self.notify();
        failure
    }

    fn lock(&self) -> Result<MutexGuard<'_, Cell>, Unavailable> {
        self.cell.lock_strict().map_err(|_| self.poisoned())
    }

    fn notify(&self) {
        self.signal.notify_all();
    }
}

impl AgentMailbox {
    pub(crate) fn new(
        id: AgentId,
        life: ScopeNode,
        journal: Recorder,
        admission: Admission,
        parent: Option<AgentAddress>,
        (mail, schedules, model): (Mail, Schedules, Model),
    ) -> Self {
        assert!(
            mail.pending().next().is_none() && mail.job().is_none() && schedules.is_empty(),
            "an agent's cell must start with reconciled mail and schedules"
        );
        Self {
            shared: Arc::new(Shared {
                id,
                parent,
                life,
                admission,
                journal,
                cell: Mutex::new(Cell {
                    mail,
                    schedules,
                    model,
                    queue: VecDeque::new(),
                    stopped: false,
                    released: false,
                    since: Instant::now(),
                    exchange: None,
                    waiting_for_input: true,
                    busy: false,
                    last_exchange: None,
                    rung: BTreeMap::new(),
                }),
                signal: Condvar::new(),
                failure_noticed: AtomicBool::new(false),
                #[cfg(test)]
                after_report_commit: Mutex::new(None),
            }),
        }
    }

    pub(crate) fn address(&self) -> AgentAddress {
        AgentAddress(self.shared.clone())
    }
    pub(crate) fn life(&self) -> Scope {
        (*self.shared.life).clone()
    }
    pub(crate) fn transact<T>(&self, f: impl FnOnce(&mut Txn<'_>) -> T) -> Result<T, Unavailable> {
        self.address().transact(f)
    }
    /// Merges a poisoned cell and a failed operation into one error; see [`AgentAddress::report`].
    pub(crate) fn try_transact<T>(
        &self,
        f: impl FnOnce(&mut Txn<'_>) -> Result<T, Unavailable>,
    ) -> Result<T, Unavailable> {
        self.address().try_transact(f)
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the transaction guard is handed to the condvar and reacquired for the next iteration"
    )]
    pub(crate) fn next(&self, rest: Rest) -> Next {
        let _entered = super::transaction::Entered::new(&self.shared, self.shared.id);
        let Ok(cell) = self.shared.lock() else {
            return Next::Ended;
        };
        let mut txn = Txn {
            shared: &self.shared,
            cell,
        };
        loop {
            if !txn.is_live() {
                return Next::Ended;
            }
            if !txn.cell.rung.is_empty() {
                return Next::Rung;
            }
            txn.cell.busy = rest.pending;
            if let Some(index) = txn.cell.next_index(self.shared.admission) {
                txn.cell.waiting_for_input = false;
                if let Item::Gone(id) = txn.cell.queue[index] {
                    txn.cell.queue.remove(index);
                    return Next::Gone(id);
                }
                return match txn.take(index) {
                    Err(_) => Next::Ended,
                    Ok(Queued::Command(command)) => Next::Command(command),
                    Ok(Queued::Input(input)) => {
                        let exchange = Lease::under(&self.shared.life);
                        txn.cell.exchange = Some(exchange.node());
                        Next::Take { input, exchange }
                    }
                };
            }
            let wait = match (txn.mail().job(), self.shared.admission) {
                (Some(job), _) if !rest.pending && !txn.cell.schedules_pending() => {
                    let stopped = txn.cell.stopped;
                    if txn
                        .commit(NonEmpty::one(Control::JobSettled { job, stopped }))
                        .is_err()
                    {
                        return Next::Ended;
                    }
                    txn.cell.since = Instant::now();
                    txn.cell.waiting_for_input = false;
                    return Next::Settled { stopped };
                }
                (None, Admission::Parent { idle }) => {
                    let remaining = idle.saturating_sub(txn.cell.since.elapsed());
                    if remaining.is_zero() {
                        self.shared.life.cancel(CancelCause::TimedOut);
                        self.shared.notify();
                        return Next::Ended;
                    }
                    PARK_POLL.min(remaining)
                }
                _ => PARK_POLL,
            };
            txn.cell.waiting_for_input = !rest.pending && !txn.cell.schedules_pending();
            let waited = crate::site::site!(super::transaction::Silent::CellWait,
            &self.shared.signal => wait_timeout_while(txn.cell, wait, |cell: &mut Cell| {
                !self.shared.life.is_cancelled() && cell.rung.is_empty()
                    && cell.next_index(self.shared.admission).is_none()
            }));
            if let Ok((cell, _)) = waited {
                txn.cell = cell;
            } else {
                self.shared.poisoned();
                return Next::Ended;
            }
        }
    }

    pub(crate) fn begin(&self) -> bool {
        self.transact(|txn| {
            if !txn.is_live() {
                return false;
            }
            if txn.mail().job().is_none() {
                if txn.commit(NonEmpty::one(Control::JobOpened)).is_err() {
                    return false;
                }
                txn.cell.stopped = false;
                txn.cell.released = false;
            }
            txn.cell.last_exchange = Some(Instant::now());
            true
        })
        .unwrap_or(false)
    }

    pub(crate) fn drain(&self) -> Vec<Drained> {
        #[allow(
            clippy::redundant_closure_for_method_calls,
            reason = "the method item is not general enough for transact's higher-ranked lifetime"
        )]
        self.try_transact(|txn| txn.drain()).unwrap_or_default()
    }

    pub(crate) fn has_input(&self) -> bool {
        self.transact(|txn| {
            txn.cell.queue.iter().any(|item| {
                matches!(
                    item,
                    Item::Posted(
                        _,
                        Queued::Input(Input::Human(_) | Input::Message(_) | Input::Child { .. })
                    )
                )
            })
        })
        .unwrap_or(false)
    }

    pub(crate) fn source_depths(&self) -> Vec<(Source, u64)> {
        self.transact(|txn| {
            let sources: Vec<_> = runs(&txn.cell.queue, Item::part)
                .into_iter()
                .map(|group| match group {
                    Group::Run(_) => Source::User,
                    Group::One(item) => item.source(),
                })
                .collect();
            Source::ALL
                .into_iter()
                .map(|source| {
                    let depth = sources.iter().filter(|queued| **queued == source).count();
                    (source, u64::try_from(depth).unwrap_or(u64::MAX))
                })
                .collect()
        })
        .unwrap_or_else(|_| Source::ALL.into_iter().map(|source| (source, 0)).collect())
    }
}

impl Drop for AgentMailbox {
    fn drop(&mut self) {
        let _ended = self.transact(|txn| {
            txn.shared.life.cancel(CancelCause::Cancelled);
            txn.cell.queue.clear();
        });
    }
}

impl WeakAgentAddress {
    pub(crate) fn upgrade(&self) -> Option<AgentAddress> {
        self.0.upgrade().map(AgentAddress)
    }
}

impl AgentAddress {
    /// Delivers the terminal `status`; a poisoned cell still owes the parent one failure.
    pub(crate) fn report(
        &self,
        status: ChildStatus,
        notice: impl Fn(ChildStatus) -> Input,
    ) -> Result<(), Unavailable> {
        // Not `try_transact`: only a poisoned cell falls back here; a failed commit is `Txn::report`'s.
        self.transact(|txn| txn.report(status, &notice))
            .inspect_err(|poisoned| {
                self.0
                    .report_failure(notice(ChildStatus::Failed(poisoned.to_string())));
            })?
    }

    /// Merges a poisoned cell and a failed operation into one error; see [`Self::report`].
    pub(crate) fn try_transact<T>(
        &self,
        f: impl FnOnce(&mut Txn<'_>) -> Result<T, Unavailable>,
    ) -> Result<T, Unavailable> {
        self.transact(f)?
    }

    pub(crate) fn transact<T>(&self, f: impl FnOnce(&mut Txn<'_>) -> T) -> Result<T, Unavailable> {
        let _entered = super::transaction::Entered::new(&self.0, self.0.id);
        let value = {
            let cell = self.0.lock()?;
            f(&mut Txn {
                shared: &self.0,
                cell,
            })
        };
        self.0.notify();
        Ok(value)
    }

    pub(crate) fn ring(&self, label: String, id: ScheduleId) {
        let _entered = super::transaction::Entered::new(&self.0, self.0.id);
        if let Ok(mut cell) = self.0.cell.lock_strict()
            && !self.0.life.is_cancelled()
        {
            cell.rung
                .entry(label)
                .and_modify(|rung| {
                    if rung.get() < id.get() {
                        *rung = id;
                    }
                })
                .or_insert(id);
        }
        self.0.notify();
    }

    pub(crate) fn gone(&self, id: AgentId) {
        let _gone = self.transact(|txn| {
            if txn.is_live() {
                txn.cell.queue.push_back(Item::Gone(id));
                txn.cell.waiting_for_input = false;
            }
        });
    }

    pub(crate) fn post(&self, input: Input) -> Result<(), Refused> {
        self.transact(|txn| txn.post(Queued::Input(input)))
            .map_err(|_| Refused::Ended)?
            .map(|_| ())
    }

    pub(crate) fn command(&self, command: Command) -> Result<(), Refused> {
        self.transact(|txn| txn.post(Queued::Command(command)))
            .map_err(|_| Refused::Ended)?
            .map(|_| ())
    }

    pub(crate) fn steer(&self, text: String) -> Result<(), Refused> {
        self.post(Input::Human(text))
    }

    pub(crate) fn queued(&self) -> Vec<String> {
        self.transact(|txn| {
            if !txn.is_live() {
                return Vec::new();
            }
            pending(txn.mail())
                .into_iter()
                .filter_map(|group| match group {
                    Group::Run(run) => Some(joined(run.iter().map(|(_, post)| post.text.as_str()))),
                    Group::One(post) => post.voice.is_none().then(|| post.text.clone()),
                })
                .collect()
        })
        .unwrap_or_default()
    }

    pub(crate) fn recall(&self) -> Vec<String> {
        self.try_transact(|txn| {
            if txn.is_live() {
                txn.recall()
            } else {
                Ok(Vec::new())
            }
        })
        .unwrap_or_default()
    }

    pub(crate) fn stop_current(&self) -> bool {
        self.transact(|txn| txn.stop(None)).unwrap_or(false)
    }
    pub(crate) fn stop(&self, job: JobId) -> Option<Vec<String>> {
        self.transact(|txn| {
            txn.stop(Some(job))
                .then(|| txn.recall().unwrap_or_default())
        })
        .ok()
        .flatten()
    }

    pub(crate) fn release(&self) -> bool {
        self.transact(|txn| {
            if !txn.is_live() {
                return false;
            }
            if self.0.admission == Admission::Gated && txn.mail().job().is_none() {
                txn.cell.released = true;
            }
            true
        })
        .unwrap_or(false)
    }

    pub(crate) fn end(&self, cause: CancelCause) {
        let _ended = self.transact(|txn| txn.shared.life.cancel(cause));
    }

    pub(crate) fn is_live(&self) -> bool {
        self.transact(|txn| txn.is_live()).unwrap_or(false)
    }

    pub(crate) fn status(&self) -> Result<Status, Unavailable> {
        self.transact(|txn| Status {
            selected: txn.model().selected().clone(),
            rest: txn.mail().job().is_none().then_some(txn.cell.since),
            waiting_for_input: txn.cell.waiting_for_input,
            busy: txn.cell.busy,
            job: txn.mail().job(),
            report: txn.mail().status().cloned(),
            owes: txn.mail().owes(),
            last_exchange: txn.cell.last_exchange,
        })
    }

    /// Pause with this cell locked, after Status and before the parent's post.
    #[cfg(test)]
    pub(crate) fn after_report_commit(&self, hook: impl FnOnce() + Send + 'static) {
        *self.0.after_report_commit.lock_strict().unwrap() = Some(Box::new(hook));
    }

    #[cfg(test)]
    pub(crate) fn queued_posts(&self) -> Vec<PostId> {
        self.transact(|txn| {
            txn.cell
                .queue
                .iter()
                .filter_map(|item| match item {
                    Item::Posted(id, _) => Some(*id),
                    Item::Gone(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
    }
}
