//! The one loop every node runs: draw the next item off the node's own inbox,
//! take it up, and turn a nudge-worthy outcome into a self-posted nudge.
//! Stepping the model to quiescence over one item is [`super::deliberate`]'s
//! job; nothing here special-cases a node's position, since a reply is
//! deposited on the node's own agent and a non-reply end is delivered up its
//! parent's mailbox by the spawn site.

use crate::agent::deliberate::{Fault, Outcome};
use crate::agent::gauge::{Warning, ration};
use crate::agent::log::QuiesceReason;
use crate::agent::nudge;
use crate::agent::seat::EngineLost;
use crate::agent::{Avatar, panic_msg};
use crate::bus::{
    AgentOutcome, AgentState, Emitter, Item, Next, ParkMode, Post, WORKER_PANIC_PREFIX,
};
use crate::clock;
use crate::fleet::schedule::Trigger;
use crate::provider::{Limit, Provider, ProviderError, Recovery};
use crate::record::Transient;
use crate::shell_eval;
use ral_core::carrier::Severed;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

/// How an empty inbox ends a pass: parked on the verdict [`Avatar::park_mode`]
/// gives, or handed back to the caller at once.
#[derive(Clone, Copy)]
enum Idle {
    Park,
    Return,
}

/// What one drawn item leaves the loop with: the outcome an item settled on,
/// if one did — a command settles nothing — and whether the loop draws again;
/// `/quit` and a root's `reply` end it.
struct Step {
    settled: Option<AgentOutcome>,
    flow: ControlFlow<()>,
}

impl Step {
    fn command(flow: ControlFlow<()>) -> Self {
        Self {
            settled: None,
            flow,
        }
    }

    fn settled(outcome: AgentOutcome, flow: ControlFlow<()>) -> Self {
        Self {
            settled: Some(outcome),
            flow,
        }
    }
}

/// Between disk-ceiling walks: a full scan of the session log dir and
/// scratch, too costly to pay at every tool boundary.
const DISK_WARN_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// The label of the one-shot wakeup armed at a refusal's reset.
const RESUME_LABEL: &str = "provider-reset";

/// A finish without `reply` did not complete a returning agent's contract;
/// there is no scrape of its prose.
const NO_REPLY_REASON: &str = "ended without calling `reply`";

impl Avatar {
    /// The one attend loop, identical for every node: draw the next inbox
    /// item, take it up, and repeat until an empty inbox meets a
    /// [`Self::park_mode`] that does not park, or a cancel ends one that does.
    /// A `reply`'s value is deposited on the agent for its consumer to read.
    pub fn attend(&mut self, emit: &Emitter) -> AgentOutcome {
        self.attend_until(emit, Idle::Park)
    }

    /// [`Self::attend`]'s per-exchange half, for
    /// [`converse`](crate::headless::converse): drain the seeded message and
    /// any nudge continuation it raises, then return instead of blocking for
    /// the next one.  Converse posts no command, so a slash-shaped user
    /// message reaches the model as ordinary text.
    pub(crate) fn attend_backlog(&mut self, emit: &Emitter) -> AgentOutcome {
        self.attend_until(emit, Idle::Return)
    }

    fn attend_until(&mut self, emit: &Emitter, idle: Idle) -> AgentOutcome {
        self.couple(emit);
        let mut settled = AgentOutcome::Failed(NO_REPLY_REASON.into());
        let lost = loop {
            if let Some(s) = self.seat.severed() {
                break Some(s);
            }
            let Some(next) = self.draw(idle) else {
                break None;
            };
            self.agent.set_resting(false);
            match self.take_up(&next, emit) {
                Ok(step) => {
                    if let Some(outcome) = step.settled {
                        settled = outcome;
                    }
                    if step.flow.is_break() {
                        break None;
                    }
                }
                Err(s) => break Some(s),
            }
        };
        // A severance the loop broke on, or one a park quiesced behind, is
        // recorded exactly once: the engine's own account as a durable note,
        // the plain sentence as the failed outcome.
        if let Some(s) = lost.or_else(|| self.seat.severed()) {
            let lost = EngineLost::running(&s, self.agent.run_dir());
            self.note(lost.logged());
            settled = AgentOutcome::Failed(lost.to_string());
        }
        // `take_up` quiesces per item; this catches whichever path broke the
        // loop, so the agent is ReadyForUser however it ends.
        self.abort_unready();
        debug_assert!(
            self.log.lock().context().is_ready(),
            "attend must leave the agent ReadyForUser"
        );
        settled
    }

    /// The next item, or `None` once an empty inbox ends the pass.  Every
    /// draw is a settled ready boundary.  A lease-chain reap is deliberately
    /// not drained here: core pushes its `` `notice `` on the surface stream
    /// of the run that observes it, so a reap during a long idle surfaces once
    /// an item next runs.
    fn draw(&self, idle: Idle) -> Option<Next> {
        match idle {
            Idle::Return => self.inbox.next_item(),
            Idle::Park => {
                // The state a frontend shows over the coming silence is this
                // park's own verdict.  Only on an empty queue: with an item
                // already in hand the agent is not idle for any observable
                // moment, and the deliberation's own transitions are the truth.
                if self.inbox.is_empty() {
                    let mode = self.park_mode(self.agent.engaged());
                    self.agent.set_resting(mode == ParkMode::Engaged);
                    self.recorder()
                        .transient(Transient::State(idle_state(mode)));
                }
                // The verdict is recomputed on every wake.
                self.inbox
                    .next_or_idle(|engaged| self.park_mode(engaged), &self.agent.token)
            }
        }
    }

    /// Take up one drawn item.  A drawn item is always admissible: staleness
    /// is settled at the inbox's own pop, against that inbox's clear-epoch.
    ///
    /// # Errors
    /// The engine's severance, after which no item can run.
    fn take_up(&mut self, next: &Next, emit: &Emitter) -> Result<Step, Severed> {
        let item = match next {
            Next::Read(cmd) => {
                self.read(cmd, emit);
                return Ok(Step::command(ControlFlow::Continue(())));
            }
            Next::Rewrite(cmd) => return Ok(Step::command(self.rewrite(cmd, emit))),
            Next::Item(item) => item,
        };
        self.heard(item);
        // Only a genuine boundary clears the latches, a prior exchange's Esc,
        // and any nudge still queued behind it — that nudge continued the
        // exchange this one closes.  A self-nudge is the same exchange
        // continuing.
        if item.opens_exchange() {
            self.readings.nudges.reset();
            self.agent.token.reset();
            self.inbox.drop_nudges();
        }
        announce(item, &self.recorder());
        // Read once, so a `/model` swap on the UI thread lands on the next
        // item rather than mid-item.
        let active = self.agent.provider.current();
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.deliberate(&active, Some(item.text()), item.continues(), emit)
        }));
        // A provider error or an unwind can leave the session mid-protocol;
        // quiesce now so the next prompt — nudge or user — is admissible.
        self.abort_unready();
        let outcome: Result<Outcome, ProviderError> = match attempt {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(Fault::Provider(error))) => Err(error),
            Ok(Err(Fault::Severed(s))) => return Err(s),
            Ok(Err(Fault::Log(why))) => {
                self.note_error(&why);
                return Ok(Step::settled(
                    AgentOutcome::Failed(why),
                    ControlFlow::Continue(()),
                ));
            }
            // Host-side only — transport, surface decode, render.  An
            // eval-side panic is caught at `Shell::run`, which rolls the
            // dynamic state back, so it never unwinds this far.  Recording
            // and continuing keeps one crash from sinking the agent.
            Err(payload) => {
                let msg = format!("{WORKER_PANIC_PREFIX}{}", panic_msg(&payload));
                self.note_error(&msg);
                return Ok(Step::settled(
                    AgentOutcome::Failed(msg),
                    ControlFlow::Continue(()),
                ));
            }
        };
        // Before any nudge decision, and whether or not one follows: a chat
        // trunk keeps no registry, and its failures must still reach the human.
        if let Err(error) = &outcome {
            let recorded = self.log.lock().record_provider_error(error);
            if let Err(unrecorded) = recorded {
                self.recorder().report_fault(&unrecorded);
            }
            if let ProviderError::Refused(refusal) = error
                && self.resumes_at_reset()
                && let Recovery::Deferred(at) = refusal.recovery()
            {
                self.resume_at(at, refusal.limit);
            }
        }
        // A boundary read, legal here: the batch has fully drained and no
        // dispatch is in flight.
        let workers_idle = self.seat.read(|t| t.workers())?.is_empty();
        let facts = nudge::Facts {
            must_reply: self.agent.returns,
            pinned: self.pinned_digest(),
            // Nudged only when nothing else is already carrying this agent
            // forward: no reply standing for a consumer to fetch, no detached
            // shell work, no busy children.
            quiet: !self.agent.has_reply() && workers_idle && !self.agent.has_busy_children(),
        };
        let nudged = self
            .readings
            .nudges
            .react(outcome.as_ref(), &facts, &mut self.log.lock());
        if let Some(text) = nudged {
            let prompt = self.log.lock().context().current_prompt();
            match prompt {
                Some(prompt) => self.inbox.push(Post::Nudge { prompt, text }),
                // Unreachable: every outcome `react` answers followed a
                // deliberation whose prompt `append_user` committed.
                None => self.note_error(&format!(
                    "a nudge was decided with no prompt in hand to continue: dropping it: {text}"
                )),
            }
        }
        // Only a parentless returning agent — the headless root — ends on
        // `reply`; a child parks on its deposit, to be messaged again.
        let flow = if self.agent.parent.is_none() && matches!(outcome, Ok(Outcome::Replied)) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        };
        Ok(Step::settled(agent_outcome(&outcome), flow))
    }

    /// Book a drawn item against the park verdict: a direct child's
    /// result means it is no longer awaited.  [`Avatar::settle`] makes
    /// deliver-then-retire structural, so a parked parent never sees "no live
    /// child" without the result already queued.  Every item reaching here
    /// already survived the inbox's own pop-time fence against a `/clear`
    /// ([`crate::bus::inbox`]'s clear-epoch), so there is nothing left to
    /// admit — the `Surface` arm is a routing tripwire, not an admission.
    pub(super) fn heard(&self, item: &Item) {
        match item {
            Item::Agent(r) => self.agent.heard(r.id),
            Item::Surface { id, .. } => debug_assert_eq!(
                *id, self.agent.id,
                "a spawn's surface batch always drains in the session it was stamped with"
            ),
            _ => {}
        }
    }

    /// How an empty inbox should be treated, recomputed on every wake, from
    /// `engaged` — the exchange clock, read by `next_or_idle` under the queue
    /// mutex — and this agent's own status.  A conversing agent with a human
    /// attached parks [`ParkMode::Held`], immune to cancellation; one driven
    /// one exchange at a time waits on nothing but its fleet; a returning
    /// agent a human has exchanged with parks [`ParkMode::Engaged`] — the same
    /// wait, but a terminate-cause cancel still ends it, and the fleet's idle
    /// lease rather than this predicate bounds it.
    fn park_mode(&self, engaged: bool) -> ParkMode {
        // Nothing is left to wait for once the engine is gone.
        if self.seat.severed().is_some() {
            return ParkMode::Quiesce;
        }
        let agent = &self.agent;
        if !agent.returns && self.fleet.launch.attended {
            // `next_or_idle` lets a terminate cause end every park but `Held`,
            // so a conversing agent asks here instead: `/close` stamps this
            // token, and parking Held past it would be a zombie.
            return if agent.token.terminated() {
                ParkMode::Quiesce
            } else {
                ParkMode::Held
            };
        }
        // Only an agent with somewhere to report waits to be messaged; a
        // headless root returns and ends.
        if agent.parent.is_some() && agent.returns && (engaged || agent.has_reply()) {
            return ParkMode::Engaged;
        }
        if agent.has_busy_children() {
            return ParkMode::HeldByChildren;
        }
        if agent.schedules.armed() {
            return ParkMode::UntilCancelled;
        }
        ParkMode::Quiesce
    }

    /// The disk-warn ceiling's verdict, as an operational note once per
    /// excursion — nothing is ever rotated or deleted.  Unconfigured it walks
    /// nothing at all; otherwise it walks at most once per
    /// [`DISK_WARN_CHECK_INTERVAL`].
    ///
    /// # Errors
    /// The engine's severance, from the `EXARCH_SCRATCH` probe.
    fn disk_warning(&mut self) -> Result<Option<Warning>, Severed> {
        let Some(ceiling) = self.fleet.launch.disk_warn_bytes else {
            return Ok(None);
        };
        if self
            .disk_checked
            .is_some_and(|at| at.elapsed() < DISK_WARN_CHECK_INTERVAL)
        {
            return Ok(None);
        }
        self.disk_checked = Some(Instant::now());
        let mut total = crate::agent::resources::dir_size(self.log.lock().dir());
        if let Some((_, bytes)) = self.scratch_bytes()? {
            total += bytes;
        }
        Ok(self.readings.gauges.disk(total, ceiling))
    }

    /// Every standing condition newly climbed, told: the user's lines noted,
    /// the model's reminders returned as the steering that trails a tool
    /// batch's arrivals.
    ///
    /// # Errors
    /// The engine's severance, from the disk probe.
    pub(super) fn warnings(&mut self, provider: &Provider) -> Result<Vec<String>, Severed> {
        let pressure = self.pressure_gauge(provider);
        let mut told: Vec<Warning> = self
            .readings
            .gauges
            .pressure(pressure)
            .into_iter()
            .collect();
        told.extend(self.disk_warning()?);
        let account = provider.account();
        told.extend(ration::told(
            account,
            &provider.climb_allowances(ration::USER_HEARS),
        ));
        told.extend(self.readings.gauges.ration.climb(
            account,
            &provider.allowances(),
            self.resumes_at_reset(),
        ));
        Ok(self.tell(told))
    }

    /// Note each user line; word each model one as a reminder — none for a
    /// trunk that steers nothing.
    fn tell(&self, warnings: Vec<Warning>) -> Vec<String> {
        warnings
            .into_iter()
            .filter_map(|warning| match warning {
                Warning::User(line) => {
                    self.note(line);
                    None
                }
                Warning::Model(reminder) => {
                    self.readings.nudges.remind(&reminder, &mut self.log.lock())
                }
            })
            .collect()
    }

    /// Arm the one-shot wakeup that resumes the task at a refusal's reset,
    /// replacing any earlier one.
    fn resume_at(&self, resets_at: jiff::Timestamp, limit: Limit) {
        let now = jiff::Timestamp::now();
        let prompt = format!(
            "The provider refused requests at {} until {}; that time has passed. \
             Carry on where you left off.",
            clock::local(now),
            clock::local(resets_at)
        );
        let schedules = &self.agent.schedules;
        schedules.unschedule(RESUME_LABEL);
        match schedules.schedule(
            Trigger::At(resets_at),
            prompt,
            RESUME_LABEL.into(),
            &self.agent.mailbox,
        ) {
            Ok(_) => self.note(format!(
                "{}: resuming {}, in {}",
                limit.label(),
                clock::local(resets_at),
                clock::hms(clock::until(resets_at, now).as_secs())
            )),
            Err(refusal) => self.note_error(&format!(
                "{}: the resume could not be scheduled: {refusal}",
                limit.label()
            )),
        }
    }

    /// Quiesce a log a deliberation left mid-protocol.
    fn abort_unready(&self) {
        let mut log = self.log.lock();
        if !log.context().is_ready() {
            log.quiesce(QuiesceReason::Aborted);
        }
    }
}

/// Emit the chrome an item's source shows as it enters context.  A nudge is an
/// internal continuation and a command never reaches the model, so both are quiet.
pub(super) fn announce(item: &Item, recorder: &crate::record::Emitter) {
    match item {
        Item::Human(_) | Item::Wakeup(_) | Item::Message(_) => {
            // The live row derives from the published `Display::Prompt`
            // record, so this is the one authoring site.
            record_commit(
                recorder,
                crate::record::Display::Prompt { text: item.text() },
            );
        }
        Item::Agent(r) => {
            // The record carries the reduced fault the scrollback block is
            // built from, not the raw outcome enum.
            record_commit(
                recorder,
                crate::record::Display::SubagentDone {
                    name: r.name.clone(),
                    error: r.outcome.fault(),
                    elapsed_ms: u64::try_from(r.elapsed.as_millis()).unwrap_or(u64::MAX),
                },
            );
        }
        // A detached `spawn`'s deferred batch, decoded as the live foreground
        // decode would.  It was stamped with and posted to this same session,
        // so the emitter's id already routes its cards to the right scrollback.
        Item::Surface { values, .. } => {
            for v in values {
                match shell_eval::decode_surface(v) {
                    shell_eval::Decoded::Surface(surface) => {
                        if let Err(error) = crate::fleet::desk::absorb_surface(recorder, &surface) {
                            recorder.report_fault(&error);
                        }
                    }
                    shell_eval::Decoded::Landed => {}
                    shell_eval::Decoded::Unknown => {
                        if let Err(error) =
                            recorder.emit(shell_eval::unknown_surface_note(v.shape()))
                        {
                            recorder.report_fault(&error);
                        }
                    }
                }
            }
        }
        Item::Nudge { .. } => {}
    }
}

/// Record one display commit, surfacing a failed append as a
/// `Transient::Fault` exactly as the surface path does.
fn record_commit(recorder: &crate::record::Emitter, commit: crate::record::Display) {
    if let Err(error) = recorder.emit(commit) {
        recorder.report_fault(&error);
    }
}

/// The state an idle agent is in, read off the park verdict that is about to
/// hold it there: a wait on the fleet is the one idleness that is not the
/// human's turn, and every other park — including the [`ParkMode::Quiesce`]
/// that ends the loop — leaves nothing outstanding.
fn idle_state(mode: ParkMode) -> AgentState {
    match mode {
        ParkMode::HeldByChildren => AgentState::WaitingOnAgents,
        ParkMode::Held | ParkMode::Engaged | ParkMode::UntilCancelled | ParkMode::Quiesce => {
            AgentState::Ready
        }
    }
}

/// Reduce a finished deliberation to the result a returning agent's consumer
/// is told.  Only `reply` completes the contract — its value is already
/// deposited on the agent — so a finish without one settles
/// [`AgentOutcome::Failed`].
fn agent_outcome(r: &Result<Outcome, ProviderError>) -> AgentOutcome {
    match r {
        Ok(Outcome::Replied) => AgentOutcome::Replied,
        // Re-nudged within budget by `nudge` before it ever reaches here.
        Ok(Outcome::Complete | Outcome::Empty) => AgentOutcome::Failed(NO_REPLY_REASON.into()),
        Ok(Outcome::Stopped { reason }) => AgentOutcome::Stopped(reason.clone()),
        Ok(Outcome::Cancelled) => AgentOutcome::Cancelled,
        Err(e) => AgentOutcome::Failed(e.summary()),
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::TestTrunk;
    use crate::agent::testkit::*;
    use crate::provider::Refusal;
    use crate::provider::scripted::{Reply, Script};
    use crate::record::{Display, Forensic, Record};
    use ral_core::first_order::FOValue;

    /// Every item `announce` draws records its display commit: a prompt
    /// commits `Display::Prompt`, and a subagent's breadcrumb commits
    /// `Display::SubagentDone`.
    #[test]
    fn announce_records_display_facts() {
        use crate::bus::AgentResult;

        let (tx, rx) = crate::bus::channel();
        let recorder = crate::record::Emitter::none();
        recorder.attach(crate::record::FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });

        announce(&Item::Human("hello".into()), &recorder);
        announce(
            &Item::Agent(AgentResult {
                id: 7,
                name: "helper".into(),
                outcome: AgentOutcome::Failed("boom".into()),
                elapsed: std::time::Duration::from_millis(1500),
            }),
            &recorder,
        );

        let mut facts: Vec<&'static str> = Vec::new();
        for record in crate::bus::drain_records(&rx) {
            match record {
                Record::Display(Display::Prompt { text }) => {
                    assert_eq!(text, "hello");
                    facts.push("prompt");
                }
                Record::Display(Display::SubagentDone {
                    name,
                    error,
                    elapsed_ms,
                    ..
                }) => {
                    assert_eq!(name, "helper");
                    assert_eq!(error.as_deref(), Some("boom"));
                    assert_eq!(elapsed_ms, 1500);
                    facts.push("subagent");
                }
                _ => {}
            }
        }
        assert_eq!(facts, ["prompt", "subagent"], "both commits land");
    }

    /// The park verdict reads engagement off the agent's own exchange clock,
    /// never off the TUI's focus cursor.
    #[test]
    fn park_mode_reads_engagement_from_the_exchange_clock() {
        let held = trunk(true);
        assert_eq!(held.park_mode(held.agent.engaged()), ParkMode::Held);

        let parent = Avatar::for_test("system").unwrap();
        let child = parent.fork().expect("fork child");
        assert_eq!(
            child.park_mode(child.agent.engaged()),
            ParkMode::Quiesce,
            "un-engaged, no live children, no schedule: idle quiesce delivers the outcome"
        );

        child.agent.mailbox.steer("hi".into());
        assert_eq!(
            child.park_mode(child.agent.engaged()),
            ParkMode::Engaged,
            "a human exchange engages the child, which now parks messageable"
        );
    }

    /// A conversing trunk with no human attached — the embedded one synod
    /// drives — holds for live children and quiesces once they settle, where
    /// an attended one would park on the human who is not there to type.
    #[test]
    fn an_unattended_conversing_trunk_waits_on_its_fleet_alone() {
        let embedded = trunk(false);
        assert!(
            !embedded.agent.returns && !embedded.fleet.launch.attended,
            "the fixture is a conversing trunk nobody types into"
        );
        assert_eq!(
            embedded.park_mode(embedded.agent.engaged()),
            ParkMode::Quiesce
        );
        let mut spec = TestAgentSpec::new("helper");
        spec.parent = Some(embedded.agent.clone());
        spec.returns = true;
        let _helper = test_agent(&embedded.fleet, spec).expect("a live child");
        assert_eq!(
            embedded.park_mode(embedded.agent.engaged()),
            ParkMode::HeldByChildren
        );
    }

    /// The `reply` refusal keys on the captured `returns` bit, not on
    /// trunk-ness, and is an ordinary call error rather than a termination.
    #[test]
    fn reply_refused_identically_for_trunk_and_branch_conversing_agents() {
        let root = trunk(true);
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, root.agent.id);
        let root_result = root.ral("exarch-agents `reply 1", 5, &emit).text;
        let refusal = "you converse with the user; you do not return";
        assert!(root_result.contains(refusal), "got: {root_result}");

        // A distinct name: `root` already holds `TRUNK_NAME` in this same
        // fleet, and names are unique among the live.
        let branch = root
            .branch("branch".into(), &crate::bus::dummy_emitter().0)
            .expect("branch a conversing child");
        let branch_result = branch.ral("exarch-agents `reply 1", 5, &emit).text;
        assert!(
            branch_result.contains(refusal),
            "a /branch child must be refused with the same text, got: {branch_result}"
        );
        assert_eq!(
            branch.park_mode(branch.agent.engaged()),
            ParkMode::Held,
            "a /branch child still parks Held after a refused reply"
        );
    }

    /// An un-replied finish is re-nudged within budget, then settles `Failed`
    /// — the final prose is never scraped as the answer.
    #[test]
    fn sub_agent_without_reply_is_re_nudged_then_fails() {
        let parent = Avatar::for_test("system").unwrap();
        let mut child = parent.fork().expect("fork child");
        child.seed("do the thing".into());
        // More prose-only replies than the budget will consume, so the test
        // does not couple to the exact budget.
        let mut script = Script::new();
        for _ in 0..8 {
            script = script.then(Reply::text("here is prose, but no reply"));
        }
        let outcome = drive_peer(&mut child, scripted("test-model", script));
        assert!(
            matches!(outcome, AgentOutcome::Failed(_)),
            "an un-replied finish settles Failed, got {outcome:?}"
        );
        assert!(
            child.agent.reply().is_none(),
            "the final prose must not be scraped as a reply"
        );
        assert!(child.is_ready());
    }

    /// A standing deposited reply gates every nudge kind at once — the single
    /// `quiet` test, not three separate flags.  A registered child holding a
    /// reply from an earlier turn draws no empty-turn nudge on this one.
    #[test]
    fn deposited_reply_suppresses_every_nudge() {
        let parent = Avatar::for_test("system").unwrap();
        let mut child = parent.fork().expect("fork child");
        child.agent.deposit_reply(FOValue::String {
            value: "already replied".into(),
        });

        child
            .agent
            .provider
            .swap(scripted("test-model", Script::new().then(Reply::empty())));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, child.agent.id);
        child.couple(&emit);
        child.seed("do more".into());
        let item = child.inbox.next_item().expect("the seeded item");
        child
            .take_up(&item, &emit)
            .expect("an identity seat never severs");

        assert!(
            child.inbox.next_item().is_none(),
            "a standing reply must suppress even the empty-turn nudge, budget-free rules included"
        );
    }

    /// A host-side unwind — transport, decode, render — is recorded, fails the
    /// item, and leaves the log ready, so the next prompt still deliberates.
    /// The eval-side panics in `agent/shell.rs` never reach this arm:
    /// `Shell::run` rolls those back long before the loop sees them.
    #[test]
    fn host_panic_is_recorded_and_the_next_prompt_still_deliberates() {
        let mut session = Avatar::for_test("system").unwrap();
        // The first prompt unwinds; the rest answer the second and the no-reply
        // nudges it draws.
        let mut script = Script::new().then(Reply::panicking());
        for _ in 0..8 {
            script = script.then(Reply::text("recovered"));
        }
        session.agent.provider.swap(scripted("test-model", script));

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);
        session.seed("crash on this one".into());
        let panicked = session.attend(&emit);
        assert!(
            matches!(&panicked, AgentOutcome::Failed(m) if m.starts_with(WORKER_PANIC_PREFIX)),
            "the unwind must fail the item, not sink the attend thread: {panicked:?}"
        );
        assert!(
            session.is_ready(),
            "the panicked exchange must be wound back"
        );

        // Seeded only now: consecutive prompts coalesce into one inbox entry.
        session.seed("but answer this one".into());
        let outcome = session.attend(&emit);

        let signals: Vec<crate::bus::Signal> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            signals.iter().any(|s| matches!(
                s,
                crate::bus::Signal::Fact(_, fact)
                    if matches!(fact.value(), Record::Forensic(Forensic::Error { text }) if text.starts_with(WORKER_PANIC_PREFIX))
            )),
            "the unwind must reach the user as an error too"
        );
        assert!(
            signals.iter().any(|s| matches!(
                s,
                crate::bus::Signal::Transient(_, crate::record::Transient::Token(t)) if t == "recovered"
            )),
            "the prompt after the panic must still deliberate"
        );
        assert!(
            matches!(outcome, AgentOutcome::Failed(_)),
            "an un-replied run settles Failed, got {outcome:?}"
        );
        assert!(session.is_ready());

        let parsed = crate::record::read_records(&session.log_dir().join("record.jsonl"))
            .expect("the panicked exchange must leave a parseable log");
        assert!(
            parsed.iter().any(|r| matches!(
                r,
                Record::Forensic(Forensic::Error { text }) if text.starts_with(WORKER_PANIC_PREFIX)
            )),
            "the panic is recorded in the log too"
        );
    }

    /// The reminder is built from the register the model actually wrote, and it
    /// becomes the next committed prompt: `pinned_digest` → `Facts` →
    /// `Post::Nudge` → the user turn the next deliberation sends.  The nudge
    /// unit tests hand `react` a hand-written digest; only this joins it to the
    /// live register.
    #[test]
    fn pinned_state_reminder_reads_the_live_register_and_becomes_the_next_prompt() {
        let mut session = Avatar::for_test("system").unwrap();
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        session.ral(
            r#"exarch-pins `set [key: "goal", body: `text [spans: [[text: "ship the reminder"]]]]"#,
            5,
            &emit,
        );
        let digest = session
            .pinned_digest()
            .expect("the model's own pin must reach the register");
        assert!(digest.contains("ship the reminder"), "got: {digest}");

        // A finish with nothing returned draws the reminder; the single
        // `reply` that follows ends the loop, with no verification round.
        session.agent.provider.swap(scripted(
            "test-model",
            Script::new()
                .then(Reply::text("done"))
                .then(Reply::tool_calls(vec![ral_call(
                    "r1",
                    "exarch-agents `reply 'x'",
                )])),
        ));
        session.seed("get on with it".into());
        let outcome = session.attend(&emit);

        assert!(
            matches!(outcome, AgentOutcome::Replied),
            "the loop must terminate on the first reply, got {outcome:?}"
        );
        assert!(
            crate::bus::drain_records(&rx).into_iter().any(|record| matches!(
                record,
                Record::Forensic(Forensic::Nudge { cause, .. }) if cause == "pinned-state reminder"
            )),
            "the live register must raise a pinned-state nudge"
        );
        let view = serde_json::to_string(&session.rendered_messages()).unwrap();
        assert!(
            view.contains("There is pinned state: ship the reminder"),
            "the reminder must be committed as the next prompt, not dropped: {view}"
        );
    }

    /// A returning agent has no reply-triggered nudge round to relive; the
    /// livelock regression instead lives here, for the interactive root: a
    /// stationary model-written pin queues at most one nudge, never a
    /// perpetual `Complete → nudge → Complete` cycle that would never let the
    /// loop park.
    #[test]
    fn pin_reminder_does_not_relivelock_the_interactive_root() {
        let mut session = trunk(true);
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        session.ral(
            r#"exarch-pins `set [key: "goal", body: `text [spans: [[text: "keep going"]]]]"#,
            5,
            &emit,
        );

        session.agent.provider.swap(scripted(
            "test-model",
            Script::new().then(Reply::text("working on it")),
        ));
        session.seed("go".into());
        let item = session.inbox.next_item().expect("the seeded item");
        session
            .take_up(&item, &emit)
            .expect("an identity seat never severs");

        let nudge = session
            .inbox
            .next_item()
            .expect("the first quiet completion must queue one pin reminder");
        assert!(
            matches!(&nudge, Next::Item(Item::Nudge { text, .. }) if text.contains("There is pinned state")),
            "expected a pin reminder, got {nudge:?}"
        );

        session.agent.provider.swap(scripted(
            "test-model",
            Script::new().then(Reply::text("still working")),
        ));
        session
            .take_up(&nudge, &emit)
            .expect("an identity seat never severs");

        assert!(
            session.inbox.next_item().is_none(),
            "a stationary pin must queue no second nudge: the loop would park, not relivelock"
        );
    }

    /// `--chat` withholds the tool, so nothing is left to steer the model
    /// toward: the empty turn that
    /// `nudge::tests::empty_turn_nudges_and_consumes_budget` nudges over here
    /// stands as the model left it, and no synthetic prompt is committed.
    #[test]
    fn chat_trunk_never_nudges() {
        let mut session = chat_trunk();
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        session
            .agent
            .provider
            .swap(scripted("test-model", Script::new().then(Reply::empty())));
        session.seed("hello".into());
        session.attend_backlog(&emit);

        assert!(
            !crate::bus::drain_records(&rx)
                .into_iter()
                .any(|record| matches!(record, Record::Forensic(Forensic::Nudge { .. }))),
            "a chat trunk raises no nudge"
        );
        let view = serde_json::to_string(&session.rendered_messages()).unwrap();
        assert!(
            view.contains("hello"),
            "the exchange must have happened at all"
        );
        assert!(
            !view.contains("EXARCH_REMINDER"),
            "nothing synthetic may join a chat conversation: {view}"
        );
    }

    /// Unconfigured, the check returns before any bookkeeping: no walk, no
    /// warning, no cost.
    #[test]
    fn disk_warning_unconfigured_never_walks_or_warns() {
        let mut session = Avatar::for_test("system").unwrap();
        assert!(session.fleet.launch.disk_warn_bytes.is_none());

        assert!(
            session
                .disk_warning()
                .expect("an identity seat never severs")
                .is_none(),
            "unconfigured: never warns, ever"
        );
        assert!(
            session.disk_checked.is_none(),
            "the early return never stamps a walk"
        );
    }

    /// The walk is real: 64 KiB sits above a fresh session's own footprint,
    /// and the file alone crosses it.
    #[test]
    fn disk_warning_walks_the_log_dir() {
        let mut session = Avatar::for_test_with(TestTrunk {
            disk_warn_bytes: Some(64 * 1024),
            ..TestTrunk::new("system")
        })
        .expect("a trunk under a disk-warn ceiling");
        std::fs::write(session.log_dir().join("big.txt"), vec![0u8; 1024 * 1024]).unwrap();
        assert!(
            session
                .disk_warning()
                .expect("an identity seat never severs")
                .is_some()
        );
    }

    /// A trunk refused twice, until a reset `resets_in` after each refusal.
    fn refused_twice(resume_on_reset: bool, resets_in: Duration) -> Avatar {
        let mut session = Avatar::for_test_with(TestTrunk {
            resume_on_reset,
            ..TestTrunk::new("system")
        })
        .unwrap();
        let refusal = || {
            Reply::error(ProviderError::Refused(Refusal::for_test(
                Limit::Allowance,
                Some(resets_in),
            )))
        };
        let script = Script::new().then(refusal()).then(refusal());
        session.agent.provider.swap(scripted("test-model", script));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        session.couple(&emit);
        for prompt in ["first", "again"] {
            session.seed(prompt.into());
            let item = session.inbox.next_item().expect("the seeded item");
            session
                .take_up(&item, &emit)
                .expect("an identity seat never severs");
        }
        session
    }

    /// A deferred refusal leaves exactly one wakeup at its reset, however many
    /// times it is refused.
    #[test]
    fn a_deferred_refusal_arms_one_resume_wakeup() {
        let session = refused_twice(true, Duration::from_hours(5));
        assert!(
            session.agent.schedules.unschedule(RESUME_LABEL),
            "the wakeup bears the provider-reset label"
        );
        assert!(
            !session.agent.schedules.armed(),
            "the second refusal replaced the first rather than adding to it"
        );
    }

    #[test]
    fn only_a_resuming_trunk_arms_a_resume_and_only_past_the_in_place_wait() {
        let armed = |resumes, resets_in| refused_twice(resumes, resets_in).agent.schedules.armed();
        assert!(!armed(false, Duration::from_hours(5)));
        assert!(!armed(true, Duration::from_secs(5)));
    }
}
