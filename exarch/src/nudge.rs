//! Single home for the inter-attempt nudge facility.
//!
//! Owns the rule set, the budget counter, and the post-attempt decision:
//! walk the rules and either accept the turn (surfacing any attached provider
//! error on the way out) or hand back a synthetic next prompt for the drive
//! loop to post to itself as an [`InboxMsg::Nudge`](crate::bus::InboxMsg).
//! Records each nudge to both the `events.json` breadcrumb and the operational
//! trace (`Kind::Nudge`); the display decides whether to surface it.
//!
//! The registry is **per-session** ([`Agent::nudges`]): the drive loop runs
//! one [`Agent::apply`] per inbox message, not one whole turn, so the
//! per-turn state must outlive a single `apply`.  It resets on a genuine
//! turn-boundary message via [`Registry::reset`], never on a self-nudge.
//!
//! [`Agent::apply`]: crate::agent::Agent::apply
//! [`Agent::nudges`]: crate::agent::Agent

use crate::agent::TurnOutcome;
use crate::bus::{Emitter, Kind};
use crate::event::AgentLog;
use crate::provider::ProviderError;

/// Outer-attempt budget per user turn.  Independent of, and stacked
/// on top of, the provider's inner retry budget.
const BUDGET: u32 = 3;
/// How many times to nudge the model when it yields with pinned state
/// before accepting the turn.  Resets whenever the pinned digest changes,
/// so progress (a task completed, a goal updated) restores the budget.
const OPEN_TASKS_BUDGET: u32 = 5;
/// How many genuine turns elapse between pinned-state reminders.  A gentle
/// cadence: often enough that a stale gauge is caught, rare enough not to nag.
const REMIND_EVERY: u32 = 12;
/// A shared bracket for every synthetic nudge message, so the model can
/// distinguish a system reminder from genuine user input.  The opening
/// bracket carries the "do not mention" instruction; the closing bracket
/// delimits where the system text ends.
const EXARCH_REMINDER_OPEN: &str = "[EXARCH_REMINDER // Do not mention to user.] ";
const EXARCH_REMINDER_CLOSE: &str = " [/EXARCH_REMINDER]";
type Rule = fn(&Result<TurnOutcome, ProviderError>) -> Option<(String, &'static str)>;

/// The rule set the binary ships with.
///
/// [`Agent::apply`]: crate::agent::Agent::apply
const RULES: &[Rule] = &[
    on_empty_turn,
    on_early_stop,
    on_truncated,
    on_provider_recoverable,
];

/// Per-attempt expectations that depend on the agent rather than the
/// attempt outcome: whether this agent returns through `reply`
/// ([`must_reply`](Self::must_reply)) and its current pinned state
/// ([`pinned`](Self::pinned)).  Threaded in by [`Agent::drive`].
///
/// [`Agent::drive`]: crate::agent::Agent::drive
pub(crate) struct NudgeCtx {
    /// Whether this session returns through `reply` — true for a returning
    /// agent (a peer or a headless root), false for the interactive root.  A
    /// returning agent that finishes a tool-call-free step without having
    /// replied is re-nudged to call `reply` within the [`BUDGET`], then fails.
    pub must_reply: bool,
    /// The model's current pinned state as a one-line description, or `None`
    /// when nothing is pinned.  Drives the periodic pinned-state reminder so a
    /// long-lived rail gauge does not drift out of the model's attention.
    pub pinned: Option<String>,
}

/// Per-session nudge state.  Lives on the [`Agent`](crate::agent::Agent)
/// and is reset at each genuine turn boundary by [`Self::reset`];
/// [`Self::react`] is the only post-attempt entry point.
pub(crate) struct Registry {
    used: u32,
    /// Genuine turns since the last pinned-state reminder, bumped once per
    /// turn-boundary message by [`Self::reset`].  Unlike the latch it
    /// accumulates *across* turns; the reminder fires once it reaches
    /// [`REMIND_EVERY`], then it returns to zero.
    turns_since_pin_reminder: u32,
    /// One-shot latch for the pinned-state reminder: at most once per turn,
    /// budget-free.
    pin_reminded: bool,
    /// Open-tasks nudge counter: how many times the model has been nudged
    /// for yielding with pinned state.  Resets when the digest changes.
    open_tasks_nudges: u32,

    /// Snapshot of the pinned digest at the last open-tasks nudge, to detect
    /// changes that reset the budget.
    last_pinned_digest: Option<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            used: 0,
            turns_since_pin_reminder: 0,
            pin_reminded: false,
            open_tasks_nudges: 0,
            last_pinned_digest: None,
        }
    }

    /// Reset the per-turn state — the retry budget and the one-shot latch —
    /// and count this turn toward the periodic pinned-state reminder.  Called
    /// by [`Agent::drive`] on a genuine turn-boundary message, never on a
    /// self-nudge (which is the same turn continuing), so the turn counter
    /// advances once per real turn.
    ///
    /// [`Agent::drive`]: crate::agent::Agent::drive
    pub fn reset(&mut self) {
        self.used = 0;
        self.turns_since_pin_reminder += 1;
        self.pin_reminded = false;
    }

    /// The one decider.  Walks [`RULES`] against `attempt`, optionally
    /// records the nudge to `log`, emits any user-facing error breadcrumbs
    /// through `emit`, and returns the synthetic next prompt to re-drive
    /// with — `Some(message)` for the drive loop to self-post as an
    /// [`InboxMsg::Nudge`](crate::bus::InboxMsg) — or `None` to accept the
    /// turn and stop.
    pub fn react(
        &mut self,
        attempt: &Result<TurnOutcome, ProviderError>,
        ctx: NudgeCtx,
        emit: &Emitter,
        log: &mut AgentLog,
    ) -> Option<String> {
        let Some((cause, message)) = RULES.iter().find_map(|&r| r(attempt)) else {
            // (No RULE matched.)
            //
            // A returning agent reached a tool-call-free `Complete` without
            // calling `reply` — its sole return path, no scrape — so insist on
            // it.  Re-nudge within the per-turn budget, then give up honestly:
            // once the budget is spent the un-replied `Complete` is accepted
            // here and `agent_digest` maps it to `Failed`.  A `Replied`
            // outcome never lands here, so an agent that already replied is not
            // re-nudged.
            if ctx.must_reply && matches!(attempt, Ok(TurnOutcome::Complete(_))) {
                if self.used < BUDGET {
                    self.used += 1;
                    record_nudge(
                        emit,
                        log,
                        self.used,
                        "no-reply finish (returning agent)".into(),
                    );
                    return Some(format!("{EXARCH_REMINDER_OPEN}{REPLY_MESSAGE}{EXARCH_REMINDER_CLOSE}"));
                }
                let msg = "agent finished without calling `reply` after the nudge budget; \
                           returning a failure"
                    .to_string();
                let _ = log.record_error(msg.clone());
                emit.emit(Kind::Error(msg));
                return None;
            }
            // Periodic, budget-free reminders on the pin register —
            // at most once per turn, only on a clean completion, every
            // `REMIND_EVERY` turns.  When something is pinned the model is
            // reminded what; when nothing is pinned it is nudged to set a
            // goal or track tasks.
            if ctx.pinned.is_some()
                && matches!(attempt, Ok(TurnOutcome::Complete(_)))
                && !self.pin_reminded
                && self.turns_since_pin_reminder >= REMIND_EVERY
            {
                self.pin_reminded = true;
                self.turns_since_pin_reminder = 0;
                record_nudge(emit, log, self.used, "pinned-state reminder".into());
                return Some(format!("{EXARCH_REMINDER_OPEN}There is pinned state: {}{EXARCH_REMINDER_CLOSE}", ctx.pinned.as_deref().unwrap_or("none")));
            } else if matches!(attempt, Ok(TurnOutcome::Complete(_)))
                && !self.pin_reminded
                && self.turns_since_pin_reminder >= REMIND_EVERY
            {
                self.pin_reminded = true;
                self.turns_since_pin_reminder = 0;
                record_nudge(emit, log, self.used, "no-pins reminder".into());
                return Some(format!("{EXARCH_REMINDER_OPEN}Nothing is pinned — consider calling `set-goal` to remember what you are working on, or tracking some tasks with `add-task`.{EXARCH_REMINDER_CLOSE}"));
            } else if ctx.pinned.is_some()
                && matches!(attempt, Ok(TurnOutcome::Complete(_)))
            {
                if self.last_pinned_digest.as_deref() != ctx.pinned.as_deref() {
                    self.open_tasks_nudges = 0;
                    self.last_pinned_digest = ctx.pinned.clone();
                }
                if self.open_tasks_nudges < OPEN_TASKS_BUDGET {
                    self.open_tasks_nudges += 1;
                    record_nudge(emit, log, self.used, "open-tasks nudge".into());
                    return Some(format!("{EXARCH_REMINDER_OPEN}there is pinned state: {}{EXARCH_REMINDER_CLOSE}", ctx.pinned.as_deref().unwrap_or("none")));
                }
            }
            surface_provider_error(attempt, emit, log);
            return None;
        };
        if self.used >= BUDGET {
            let msg = format!("nudge budget exhausted ({BUDGET} attempts; last cause: {cause})");
            let _ = log.record_error(msg.clone());
            emit.emit(Kind::Error(msg));
            surface_provider_error(attempt, emit, log);
            return None;
        }
        self.used += 1;
        record_nudge(emit, log, self.used, cause);
        Some(format!("{EXARCH_REMINDER_OPEN}{message}{EXARCH_REMINDER_CLOSE}"))
    }
}

/// Record a nudge to both views: the `events.json` forensic breadcrumb and the
/// operational trace (`Kind::Nudge`, which the display surfaces as it sees fit).
fn record_nudge(emit: &Emitter, log: &mut AgentLog, used: u32, cause: String) {
    let _ = log.record_nudge(used, BUDGET, cause.clone());
    emit.emit(Kind::Nudge {
        used,
        max: BUDGET,
        cause,
    });
}

fn surface_provider_error(
    attempt: &Result<TurnOutcome, ProviderError>,
    emit: &Emitter,
    log: &mut AgentLog,
) {
    if let Err(e) = attempt {
        let _ = log.record_provider_error(e);
        emit.emit(Kind::ProviderError(e.into()));
    }
}

// ── Rules ────────────────────────────────────────────────────────────

/// The no-reply reminder (gated by [`NudgeCtx::must_reply`], so it fires for a
/// returning agent — a peer or a headless root).  The agent finished with prose
/// but never called `reply`, its sole return path, so as things stand it
/// returns nothing; this asks it to return through `reply` before the run ends.
/// It is *budgeted*: re-issued each un-replied finish until the [`BUDGET`] is
/// spent, then the run fails.
const REPLY_MESSAGE: &str = "You ended your turn without calling `reply`, so your parent will \
    receive nothing. Return your result now by calling `reply` — pass a markdown report as \
    `result`, or a JSON object/array for structured findings. This is the only way to hand \
    your work back; a final message on its own is not delivered.";

fn on_empty_turn(r: &Result<TurnOutcome, ProviderError>) -> Option<(String, &'static str)> {
    match r {
        Ok(TurnOutcome::Empty) => Some((
            "empty turn".into(),
            "Your previous turn produced no text and no tool calls. \
             If you are finished, say so explicitly; otherwise continue.",
        )),
        _ => None,
    }
}

fn on_early_stop(r: &Result<TurnOutcome, ProviderError>) -> Option<(String, &'static str)> {
    match r {
        Ok(TurnOutcome::Stopped { reason }) => Some((
            format!("stop={reason}"),
            "Your previous turn ended early on a provider-side stop. \
             Reformulate the previous reply or take a different approach.",
        )),
        _ => None,
    }
}

fn on_truncated(r: &Result<TurnOutcome, ProviderError>) -> Option<(String, &'static str)> {
    match r {
        Err(ProviderError::Truncated { .. }) => Some((
            "truncated".into(),
            "Your previous reply was cut off before it completed. \
             Continue concisely from where it stopped.",
        )),
        _ => None,
    }
}

fn on_provider_recoverable(
    r: &Result<TurnOutcome, ProviderError>,
) -> Option<(String, &'static str)> {
    match r {
        Err(ProviderError::Transient { .. } | ProviderError::RateLimited { .. }) => Some((
            "provider retry budget exhausted".into(),
            "The previous provider request failed before producing output. \
             Continue from the current workspace state.",
        )),
        _ => None,
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    /// An emitter onto a dropped receiver — `react` records to the log and
    /// emits only error breadcrumbs, and the tests assert on the returned
    /// `Option` and the budget counter, so the events go nowhere.
    fn emit() -> Emitter {
        let (tx, _rx) = channel();
        Emitter::new(tx, 0)
    }

    fn fresh_log(tag: &str) -> AgentLog {
        let root = std::env::temp_dir().join(format!("exarch-nudge-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        AgentLog::root(&root, 0, "test", "test", 0).expect("session log")
    }

    /// A surfaced rate-limit already exhausted the provider loop's backoff
    /// budget; the outer nudge gives the model one fresh turn.
    #[test]
    fn rate_limit_nudges_after_provider_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log("ratelimit");
        let attempt = Err(ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(5)),
            cause: "429".into(),
            body: None,
        });
        let msg = reg
            .react(
                &attempt,
                NudgeCtx {
                    must_reply: false,
                    pinned: None,
                },
                &emit(),
                &mut log,
            )
            .expect("rate limit should nudge after provider budget");
        assert!(msg.contains("provider request failed"));
        assert_eq!(reg.used, 1);
    }

    /// Same contract for a generic transient failure.
    #[test]
    fn transient_nudges_after_provider_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log("transient");
        let attempt = Err(ProviderError::Transient {
            cause: "web stream error".into(),
            attempts: 3,
            body: None,
        });
        let msg = reg
            .react(
                &attempt,
                NudgeCtx {
                    must_reply: false,
                    pinned: None,
                },
                &emit(),
                &mut log,
            )
            .expect("transient should nudge after provider budget");
        assert!(msg.contains("provider request failed"));
        assert_eq!(reg.used, 1);
    }

    /// A model-behaviour outcome still nudges and spends one unit of budget.
    #[test]
    fn empty_turn_nudges_and_consumes_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log("empty");
        match reg.react(
            &Ok(TurnOutcome::Empty),
            NudgeCtx {
                must_reply: false,
                pinned: None,
            },
            &emit(),
            &mut log,
        ) {
            Some(msg) => assert!(msg.contains("no text and no tool calls")),
            None => panic!("empty turn must nudge, not stop"),
        }
        assert_eq!(reg.used, 1);
    }

    /// Past the budget, even a nudgeable outcome stops the turn.
    #[test]
    fn exhausted_budget_stops() {
        let mut reg = Registry::new();
        let mut log = fresh_log("budget");
        for _ in 0..BUDGET {
            assert!(
                reg.react(
                    &Ok(TurnOutcome::Empty),
                    NudgeCtx {
                        must_reply: false,
                        pinned: None,
                    },
                    &emit(),
                    &mut log,
                )
                .is_some()
            );
        }
        assert!(
            reg.react(
                &Ok(TurnOutcome::Empty),
                NudgeCtx {
                    must_reply: false,
                    pinned: None,
                },
                &emit(),
                &mut log,
            )
            .is_none()
        );
    }

    /// A clean `Complete` ends the turn.
    #[test]
    fn completion_stops() {
        let mut reg = Registry::new();
        let mut log = fresh_log("complete");
        assert!(
            reg.react(
                &Ok(TurnOutcome::Complete("done".into())),
                NudgeCtx {
                    must_reply: false,
                    pinned: None,
                },
                &emit(),
                &mut log,
            )
            .is_none()
        );
    }

    /// A returning agent (`must_reply`) that finishes a tool-call-free
    /// `Complete` without replying is re-nudged to call `reply` while budget
    /// remains — drawing on the shared per-turn budget — then, once it is
    /// spent, the un-replied finish is accepted (`None`) so the drive loop ends
    /// and `agent_digest` maps the `Complete` to `Failed`.
    #[test]
    fn no_reply_finish_re_nudges_up_to_budget_then_fails() {
        let mut reg = Registry::new();
        let mut log = fresh_log("no-reply");
        let ctx = || NudgeCtx {
            must_reply: true,
            pinned: None,
        };
        // Each un-replied finish re-nudges and spends one unit of budget.
        for _ in 0..BUDGET {
            match reg.react(
                &Ok(TurnOutcome::Complete("prose, no reply".into())),
                ctx(),
                &emit(),
                &mut log,
            ) {
                Some(msg) => assert!(msg.contains("`reply`")),
                None => {
                    panic!(
                        "a returning agent that did not reply must re-nudge while budget remains"
                    )
                }
            }
        }
        assert_eq!(
            reg.used, BUDGET,
            "the no-reply nudge now draws on the budget"
        );
        // Past the budget the un-replied finish is accepted so the run ends.
        assert!(
            reg.react(
                &Ok(TurnOutcome::Complete("still no reply".into())),
                ctx(),
                &emit(),
                &mut log,
            )
            .is_none(),
            "past the budget the un-replied finish is accepted (mapped to Failed downstream)"
        );
    }

    /// The interactive root (`must_reply` false) never gets the reply reminder
    /// — a clean `Complete` ends its turn at once.
    #[test]
    fn root_completion_is_never_reply_nudged() {
        let mut reg = Registry::new();
        let mut log = fresh_log("root-complete");
        assert!(
            reg.react(
                &Ok(TurnOutcome::Complete("answer to the user".into())),
                NudgeCtx {
                    must_reply: false,
                    pinned: None,
                },
                &emit(),
                &mut log,
            )
            .is_none()
        );
    }

    /// [`Registry::reset`] clears the per-turn budget, so a fresh
    /// turn-boundary message starts with a full budget.
    #[test]
    fn reset_clears_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log("reset");
        let ctx = || NudgeCtx {
            must_reply: false,
            pinned: None,
        };
        // Spend a unit of budget.
        let _ = reg.react(&Ok(TurnOutcome::Empty), ctx(), &emit(), &mut log);
        assert!(reg.used >= 1);
        reg.reset();
        assert_eq!(reg.used, 0, "reset clears the budget");
    }

    /// With nothing pinned, the periodic reminder nudges toward `set-goal` /
    /// `add-task` every `REMIND_EVERY` turns instead of the pinned-state
    /// message — firing exactly once per `REMIND_EVERY`-turn window, not on
    /// every turn in between.
    #[test]
    fn no_pins_reminder_fires_every_remind_every_turns() {
        let mut reg = Registry::new();
        let mut log = fresh_log("no-pin-reminder");
        let ctx = || NudgeCtx {
            must_reply: false,
            pinned: None,
        };
        let mut fires = 0;
        for turn in 1..=(REMIND_EVERY + 3) {
            reg.reset();
            if let Some(msg) = reg.react(&Ok(TurnOutcome::Complete("x".into())), ctx(), &emit(), &mut log) {
                assert_eq!(
                    turn, REMIND_EVERY,
                    "no-pins reminder should only fire on the {REMIND_EVERY}th turn"
                );
                assert!(msg.contains("set-goal"));
                fires += 1;
            }
        }
        assert_eq!(fires, 1, "no-pins reminder should fire exactly once in this window");
    }
}
