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

/// Outer-attempt budget per user turn.  Independent of the provider's
/// transport retry budget: this is only for model-visible recovery.
const BUDGET: u32 = 3;
/// How many genuine turns elapse between "nothing is pinned" reminders.  A
/// gentle cadence, since there is no outstanding obligation to be restless
/// about.  The pinned-state reminder itself is budget-free and fires on
/// every clean completion instead — see [`Registry::react`].
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
const RULES: &[Rule] = &[on_empty_turn, on_early_stop, on_truncated];

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
    /// Independent of, and additive with, the pinned-state nudge below: a
    /// returning agent owes both a reply and a clean pin register.
    pub must_reply: bool,
    /// The model's current pinned state as a one-line description, or `None`
    /// when nothing is pinned — the one register covering every pin kind
    /// (tasks, goals, protected `commitment:*` state alike; see
    /// [`Agent::pinned_digest`](crate::agent::Agent)).  Drives the pinned-state
    /// reminder for any actionable agent, regardless of role.
    pub pinned: Option<String>,
    /// True while this agent has live descendants that may still deliver the
    /// next actionable fact.  In that state the pin register stays live, but
    /// pin/no-pin reminders wait until the descendants settle.
    pub waiting_on_children: bool,
    /// True for the headless root (`parent.is_none() && !interactive`), false
    /// for everyone else — an interactive root never reaches `reply`, and a
    /// sub-agent peer's `reply` is read by a parent that can push back on it.
    /// The headless root's `reply` instead reaches an external caller with
    /// nobody left to scrutinize it, so [`Registry::react`] turns its first
    /// `reply` back once for self-verification before honouring the next one.
    pub is_headless_root: bool,
}

/// Per-session nudge state.  Lives on the [`Agent`](crate::agent::Agent)
/// and is reset at each genuine turn boundary by [`Self::reset`];
/// [`Self::react`] is the only post-attempt entry point.
pub(crate) struct Registry {
    used: u32,
    /// Genuine turns since the last "nothing is pinned" reminder, bumped once
    /// per turn-boundary message by [`Self::reset`].  Unlike a latch it
    /// accumulates *across* turns; the reminder fires once it reaches
    /// [`REMIND_EVERY`], then it returns to zero.  Unused while anything is
    /// pinned — that case is budget-free, see [`Self::react`].
    turns_since_no_pins_reminder: u32,
    /// One-shot latch for the "nothing is pinned" reminder: at most once per
    /// turn.
    no_pins_reminded: bool,
    /// One-shot latch for the headless root's self-verification nudge: once
    /// its first `reply` has been turned back, every later `reply` — from
    /// this same run, regardless of turn boundaries — is honoured.  Unlike
    /// [`Self::used`] and [`Self::no_pins_reminded`] this is never cleared by
    /// [`Self::reset`]: the obligation is "verify once before the run's
    /// result stands", not a per-turn accounting.
    reply_verified: bool,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            used: 0,
            turns_since_no_pins_reminder: 0,
            no_pins_reminded: false,
            reply_verified: false,
        }
    }

    /// Reset the per-turn state — the retry budget and the one-shot latch —
    /// and count this turn toward the periodic "nothing is pinned" reminder.
    /// Called by [`Agent::drive`] on a genuine turn-boundary message, never on
    /// a self-nudge (which is the same turn continuing), so the turn counter
    /// advances once per real turn.
    ///
    /// [`Agent::drive`]: crate::agent::Agent::drive
    pub fn reset(&mut self) {
        self.used = 0;
        self.turns_since_no_pins_reminder += 1;
        self.no_pins_reminded = false;
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
        // A headless root's first `reply` is intercepted here, before it ever
        // reaches the rules below: its result is the run's entire externally
        // visible output, with no parent left to read it and push back. Turn
        // it back once, budget-free, for self-verification; [`Self::reply_verified`]
        // latches so the very next `reply` — whatever it carries — is honoured
        // and [`Agent::drive`](crate::agent::Agent::drive) lets it terminate.
        if matches!(attempt, Ok(TurnOutcome::Replied(_))) {
            if ctx.is_headless_root && !self.reply_verified {
                self.reply_verified = true;
                record_nudge(emit, log, self.used, "reply verification".into());
                return Some(format!(
                    "{EXARCH_REMINDER_OPEN}{VERIFY_REPLY_MESSAGE}{EXARCH_REMINDER_CLOSE}"
                ));
            }
            return None;
        }
        let Some((cause, message)) = RULES.iter().find_map(|&r| r(attempt)) else {
            // (No RULE matched.)  Only a clean completion reaches the two
            // nudges below; every other outcome here (a provider error the
            // rules didn't classify, a `Replied`/`Cancelled`/`Capped`
            // termination) is reported and accepted as-is.
            if !matches!(attempt, Ok(TurnOutcome::Complete(_))) {
                surface_provider_error(attempt, emit, log);
                return None;
            }
            // Two independent obligations, neither suppressing the other: a
            // returning agent owes a `reply` regardless of its pins, and any
            // agent — trunk or sub-agent alike — owes a clean pin register
            // regardless of whether it also returns.  Compose whichever
            // apply into one reminder.
            let mut parts = Vec::new();
            if ctx.must_reply {
                if self.used < BUDGET {
                    self.used += 1;
                    record_nudge(
                        emit,
                        log,
                        self.used,
                        "no-reply finish (returning agent)".into(),
                    );
                    parts.push(REPLY_MESSAGE.to_string());
                } else {
                    // A hard give-up: an agent that cannot even call `reply`
                    // after the full budget is failed outright, regardless
                    // of any pinned state.
                    let msg = "agent finished without calling `reply` after the nudge budget; \
                               returning a failure"
                        .to_string();
                    let _ = log.record_error(msg.clone());
                    emit.emit(Kind::Error(msg));
                    return None;
                }
            }
            // The one pinned-state nudge: whatever is pinned (a task, a
            // goal, a protected `commitment:*` pin — [`NudgeCtx::pinned`]
            // covers every kind uniformly) keeps the agent restless,
            // budget-free, on every clean completion.  Only the empty
            // register gets a gentler, throttled suggestion instead.  If a
            // child is still running, the agent has already taken the next
            // action; wait for that result before asking again.
            if !ctx.waiting_on_children {
                if let Some(pinned) = ctx.pinned.as_deref() {
                    record_nudge(emit, log, self.used, "pinned-state reminder".into());
                    parts.push(format!("There is pinned state: {pinned}"));
                } else if !self.no_pins_reminded
                    && self.turns_since_no_pins_reminder >= REMIND_EVERY
                {
                    self.no_pins_reminded = true;
                    self.turns_since_no_pins_reminder = 0;
                    record_nudge(emit, log, self.used, "no-pins reminder".into());
                    parts.push(
                        "Nothing is pinned — consider calling `set-goal` to remember what you are \
                         working on, or tracking some tasks with `add-task`."
                            .to_string(),
                    );
                }
            }
            if parts.is_empty() {
                surface_provider_error(attempt, emit, log);
                return None;
            }
            return Some(format!(
                "{EXARCH_REMINDER_OPEN}{}{EXARCH_REMINDER_CLOSE}",
                parts.join(" ")
            ));
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
        Some(format!(
            "{EXARCH_REMINDER_OPEN}{message}{EXARCH_REMINDER_CLOSE}"
        ))
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

/// The headless root's one-shot reply-verification nudge
/// ([`NudgeCtx::is_headless_root`]) — see [`Registry::react`].
const VERIFY_REPLY_MESSAGE: &str = "Before this run ends: verify your work — rerun tests, \
    re-check the diff, or otherwise confirm the result is correct. Once you have, call \
    `reply` again with your result; this next call ends the run.";

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

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::bus::channel;
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
    /// budget.  It is not a model turn, so the nudge layer reports it
    /// directly instead of appending a synthetic user message.
    #[test]
    fn rate_limit_surfaces_without_nudge() {
        let mut reg = Registry::new();
        let mut log = fresh_log("ratelimit");
        let attempt = Err(ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(5)),
            cause: "429".into(),
            body: None,
        });
        assert!(
            reg.react(
                &attempt,
                NudgeCtx {
                    must_reply: false,
                    is_headless_root: false,
                    pinned: None,
                    waiting_on_children: false,
                },
                &emit(),
                &mut log,
            )
            .is_none(),
            "rate limits are provider failures, not nudgeable model behavior",
        );
        assert_eq!(reg.used, 0);
    }

    /// A transient failure exhausted the provider loop before the model
    /// produced output.  Re-driving through a nudge would duplicate the same
    /// invisible request as a fake user turn, so it stops immediately.
    #[test]
    fn transient_surfaces_without_nudge() {
        let mut reg = Registry::new();
        let mut log = fresh_log("transient");
        let attempt = || {
            Err(ProviderError::Transient {
                cause: "stream idle: no response within timeout".into(),
                attempts: 3,
                body: None,
            })
        };
        let ctx = || NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: None,
            waiting_on_children: false,
        };
        assert!(
            reg.react(&attempt(), ctx(), &emit(), &mut log).is_none(),
            "transient provider failures are surfaced directly"
        );
        assert_eq!(reg.used, 0);
    }

    /// Repeated provider failures still do not spend nudge budget.  The
    /// provider loop owns retry attempts; the nudge budget is only for
    /// model-visible recovery.
    #[test]
    fn provider_failures_do_not_consume_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log("ratelimit-budget");
        let attempt = || {
            Err(ProviderError::RateLimited {
                retry_after: None,
                cause: "429".into(),
                body: None,
            })
        };
        let ctx = || NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: None,
            waiting_on_children: false,
        };
        for _ in 0..=BUDGET {
            assert!(
                reg.react(&attempt(), ctx(), &emit(), &mut log).is_none(),
                "provider failure should stop, not nudge"
            );
        }
        assert_eq!(reg.used, 0);
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
                is_headless_root: false,
                pinned: None,
                waiting_on_children: false,
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
                        is_headless_root: false,
                        pinned: None,
                        waiting_on_children: false,
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
                    is_headless_root: false,
                    pinned: None,
                    waiting_on_children: false,
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
                    is_headless_root: false,
                    pinned: None,
                    waiting_on_children: false,
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
            is_headless_root: false,
            pinned: None,
            waiting_on_children: false,
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
                    is_headless_root: false,
                    pinned: None,
                    waiting_on_children: false,
                },
                &emit(),
                &mut log,
            )
            .is_none()
        );
    }

    /// A headless root's first `reply` is turned back for self-verification
    /// rather than accepted outright — its result reaches an external caller
    /// with nobody left to scrutinize it.
    #[test]
    fn headless_root_first_reply_is_turned_back_for_verification() {
        let mut reg = Registry::new();
        let mut log = fresh_log("verify-first");
        let ctx = || NudgeCtx {
            must_reply: true,
            is_headless_root: true,
            pinned: None,
            waiting_on_children: false,
        };
        let msg = reg
            .react(
                &Ok(TurnOutcome::Replied(serde_json::json!("done"))),
                ctx(),
                &emit(),
                &mut log,
            )
            .expect("a headless root's first reply must be turned back");
        assert!(msg.contains("verify"), "{msg}");
        assert_eq!(reg.used, 0, "the verify nudge is budget-free");
    }

    /// Once turned back, the headless root's *next* `reply` — regardless of
    /// what it carries — is honoured: the latch is one-shot for the run, not
    /// re-armed by the pinned/must-reply state.
    #[test]
    fn headless_root_second_reply_is_honoured() {
        let mut reg = Registry::new();
        let mut log = fresh_log("verify-second");
        let ctx = || NudgeCtx {
            must_reply: true,
            is_headless_root: true,
            pinned: None,
            waiting_on_children: false,
        };
        assert!(
            reg.react(
                &Ok(TurnOutcome::Replied(serde_json::json!("first"))),
                ctx(),
                &emit(),
                &mut log,
            )
            .is_some(),
            "the first reply is turned back"
        );
        assert!(
            reg.react(
                &Ok(TurnOutcome::Replied(serde_json::json!("second, verified"))),
                ctx(),
                &emit(),
                &mut log,
            )
            .is_none(),
            "the second reply must be accepted so the run can terminate"
        );
    }

    /// A sub-agent peer's `reply` is never intercepted, even though it is
    /// also a returning agent (`must_reply`): its parent reads the result and
    /// can push back, so only the headless root needs the self-verification
    /// nudge.
    #[test]
    fn peer_reply_is_never_verify_nudged() {
        let mut reg = Registry::new();
        let mut log = fresh_log("verify-peer");
        assert!(
            reg.react(
                &Ok(TurnOutcome::Replied(serde_json::json!("findings"))),
                NudgeCtx {
                    must_reply: true,
                    is_headless_root: false,
                    pinned: None,
                    waiting_on_children: false,
                },
                &emit(),
                &mut log,
            )
            .is_none(),
            "a sub-agent peer's reply is accepted at once"
        );
    }

    /// A protected `commitment:*` pin is not a special case: it rides the
    /// same pinned-state digest as a task or a goal, so it keeps the agent
    /// restless via the one pinned-state nudge, budget-free, until a
    /// verifier clears it (out of this module's scope — [`nudge`] only
    /// reports pinned state, it never clears it).
    ///
    /// [`nudge`]: crate::nudge
    #[test]
    fn pinned_commitment_state_keeps_completion_restless() {
        let mut reg = Registry::new();
        let mut log = fresh_log("commitment-pin");
        let ctx = || NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: Some("commitment:abc: criteria 0/1".into()),
            waiting_on_children: false,
        };
        for _ in 0..3 {
            let msg = reg
                .react(
                    &Ok(TurnOutcome::Complete("done".into())),
                    ctx(),
                    &emit(),
                    &mut log,
                )
                .expect("pinned state should nudge");
            assert!(msg.contains("There is pinned state: commitment:abc: criteria 0/1"));
        }
        assert_eq!(reg.used, 0, "pinned-state nudges are budget-free");
    }

    /// A parent that has already launched the next action is allowed to wait:
    /// the commitment remains live, but the pinned-state nudge resumes only
    /// after the child settles.
    #[test]
    fn pinned_state_waits_while_children_live() {
        let mut reg = Registry::new();
        let mut log = fresh_log("commitment-pin-waiting");
        assert!(
            reg.react(
                &Ok(TurnOutcome::Complete("waiting on verifier".into())),
                NudgeCtx {
                    must_reply: false,
                    is_headless_root: false,
                    pinned: Some("commitment:abc: criteria 0/1".into()),
                    waiting_on_children: true,
                },
                &emit(),
                &mut log,
            )
            .is_none(),
            "a live verifier is the next action, so the pin reminder should wait"
        );
        assert_eq!(reg.used, 0, "waiting must not spend nudge budget");
    }

    /// A returning agent owes both obligations at once: it hasn't called
    /// `reply`, and it still holds pinned state.  Neither nudge suppresses
    /// the other — both compose into one reminder, and each still draws on
    /// its own accounting (the reply nudge spends budget; the pinned-state
    /// nudge does not).
    #[test]
    fn returning_agent_gets_both_reply_and_pinned_nudges() {
        let mut reg = Registry::new();
        let mut log = fresh_log("returning-agent-both");
        let ctx = || NudgeCtx {
            must_reply: true,
            is_headless_root: false,
            pinned: Some("commitment:abc: criteria 0/1".into()),
            waiting_on_children: false,
        };
        let msg = reg
            .react(
                &Ok(TurnOutcome::Complete("prose, no reply".into())),
                ctx(),
                &emit(),
                &mut log,
            )
            .expect("a returning agent with pinned state should nudge");
        assert!(
            msg.contains("`reply`"),
            "must still be reminded to reply: {msg}"
        );
        assert!(
            msg.contains("There is pinned state: commitment:abc: criteria 0/1"),
            "must also be reminded of its pinned state: {msg}"
        );
        assert_eq!(reg.used, 1, "only the reply half spends budget");
    }

    /// [`Registry::reset`] clears the per-turn budget, so a fresh
    /// turn-boundary message starts with a full budget.
    #[test]
    fn reset_clears_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log("reset");
        let ctx = || NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: None,
            waiting_on_children: false,
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
            is_headless_root: false,
            pinned: None,
            waiting_on_children: false,
        };
        let mut fires = 0;
        for turn in 1..=(REMIND_EVERY + 3) {
            reg.reset();
            if let Some(msg) = reg.react(
                &Ok(TurnOutcome::Complete("x".into())),
                ctx(),
                &emit(),
                &mut log,
            ) {
                assert_eq!(
                    turn, REMIND_EVERY,
                    "no-pins reminder should only fire on the {REMIND_EVERY}th turn"
                );
                assert!(msg.contains("set-goal"));
                fires += 1;
            }
        }
        assert_eq!(
            fires, 1,
            "no-pins reminder should fire exactly once in this window"
        );
    }
}
