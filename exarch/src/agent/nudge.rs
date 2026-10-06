//! The lines the harness speaks to the model in its own voice.
//!
//! Two kinds of nudge, one message: a **repair** (`Empty`, `Stopped`,
//! `Truncated`, a returning agent's un-replied `Complete`) is event-shaped,
//! exceptional, and spends the per-exchange [`BUDGET`]; a **standing
//! condition** — the pin register here, the ladders of
//! [`gauge`](crate::agent::gauge) — is a fact about the agent's own live
//! state, told once when it changes and never re-told while it holds,
//! budget-free.  An edge is consumed in the same act as the part's emission,
//! so "told but not sent" and "sent but not told" are inexpressible.
//!
//! The pin register joins [`Nudges::react`]'s post-deliberation message.  The
//! ladder conditions are told at the tool boundary through
//! [`Nudges::remind`], since an agentic run takes one prompt and then two
//! hundred tool turns, and a warning that waited for the next exchange would
//! arrive after the cut.  Every model-facing sentence lives in this file; a
//! gauge hands over a [`Reminder`] and nothing it has worded.
//!
//! [`Nudges`] is per-session because the attend loop runs one
//! `Avatar::deliberate` per inbox item, not one per exchange, so this state
//! must outlive a single deliberation.  [`Nudges::reset`] clears the budget at
//! a genuine exchange boundary, never on a self-nudge; the edge fields survive
//! it — a new exchange is not a new condition.  Every path that discards a
//! decided-but-uncommitted nudge (`/clear`, `/rewind`) is reborn whole
//! ([`Nudges::reborn`]), since a decided edge cannot be un-consumed piecemeal.

use crate::agent::deliberate::Outcome;
use crate::agent::log::AgentLog;
use crate::provider::ProviderError;
use serde::{Deserialize, Serialize};

/// Per-exchange repair budget, distinct from the provider's transport
/// retries: this one is for model-visible recovery.
const BUDGET: u32 = 3;

/// Brackets every synthetic nudge, so the model can tell a system reminder
/// from genuine user input.
const EXARCH_REMINDER_OPEN: &str = "[EXARCH_REMINDER // Do not mention to user.] ";
const EXARCH_REMINDER_CLOSE: &str = " [/EXARCH_REMINDER]";

fn wrap_reminder(body: &str) -> String {
    format!("{EXARCH_REMINDER_OPEN}{body}{EXARCH_REMINDER_CLOSE}")
}

/// What a repair spent of the per-exchange budget, as the record keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spent {
    pub used: u32,
    pub max: u32,
}

/// A standing condition a gauge found newly crossed — the figures, not the
/// sentence, which is this module's to word.
pub(crate) enum Reminder {
    /// The context has crossed the soft line; `planned` is the turns the next
    /// boundary's cut would take, `None` when nothing is old enough to shed.
    Pressure {
        /// The rendered figure, e.g. "173000 of 200000 tokens".
        detail: String,
        planned: Option<Vec<u64>>,
    },
    /// An allowance window on `service` has climbed past a rung.
    Ration {
        service: String,
        label: String,
        pct: u32,
        /// Whether the allowance resets on its own — a window — or must be
        /// topped up.
        windowed: bool,
        /// Whether this agent resumes at the reset, or merely waits it out.
        resumes: bool,
    },
}

impl Reminder {
    fn cause(&self) -> String {
        match self {
            Self::Pressure { .. } => "context pressure".into(),
            Self::Ration { pct, .. } => format!("usage {pct}%"),
        }
    }

    /// The cut is announced before it happens, so the model can leave its
    /// future self a line.  With nothing old enough to shed there is no cut to
    /// announce, and the reading alone is the whole message.
    fn body(&self) -> String {
        match self {
            Self::Pressure {
                detail,
                planned: Some(turns),
            } if !turns.is_empty() => {
                let runs = crate::record::model::runs(turns);
                let first = turns[0];
                let last_plus_one = turns[turns.len() - 1] + 1;
                format!(
                    "Context pressure: {detail}. At the next turn boundary, turns {runs} will \
                     leave your context; they stay readable with `exarch-transcript`. To leave \
                     your future self a line, run `exarch-context `evict [turns: !{{range {first} \
                     {last_plus_one}}}, note: `some '…']` now: a prompt whose exchange is still \
                     in hand stays on its own; otherwise nothing is required of you."
                )
            }
            Self::Pressure { detail, .. } => format!("Context pressure: {detail}."),
            Self::Ration {
                service,
                label,
                pct,
                windowed,
                resumes,
            } => {
                let outcome = match (windowed, resumes) {
                    (true, true) => {
                        "If it runs out, this task pauses until it resets and then resumes."
                    }
                    (true, false) => "If it runs out, requests are refused until it resets.",
                    (false, _) => "When it runs out, requests fail until it is topped up.",
                };
                format!("Your {service} usage allowance is {pct}% spent ({label}). {outcome}")
            }
        }
    }
}

/// The agent-side facts one nudge decision reads, assembled by `take_up` in
/// `agent/attend.rs`.
pub(crate) struct Facts {
    /// True for an agent that returns through `reply` — a fork or the
    /// headless root, never a conversing one.
    pub must_reply: bool,
    /// One-line digest of the whole pin register, `None` when nothing is
    /// pinned.
    pub pinned: Option<String>,
    /// Nothing else is already carrying this agent forward: no reply standing
    /// for a parent to fetch, no detached shell work, no busy children.  The
    /// one gate every nudge kind shares — `false` and `react` returns `None`
    /// unspent, budget untouched.
    pub quiet: bool,
}

/// Per-session nudge state, entered from [`Self::react`] at the end of a
/// deliberation and [`Self::remind`] at a tool boundary within one.
pub(crate) struct Nudges {
    /// Whether the model holds a tool to be steered toward.  Off — the
    /// toolless `--chat` trunk — every nudge would steer it toward a tool it
    /// does not have, so nothing is ever said.
    steers: bool,
    /// Repairs spent this exchange.
    used: u32,
    /// The pin digest last told to the model; a differing register re-arms.
    pinned_told: Option<String>,
}

impl Nudges {
    pub fn new(steers: bool) -> Self {
        Self {
            steers,
            used: 0,
            pinned_told: None,
        }
    }

    pub fn steers(&self) -> bool {
        self.steers
    }

    /// The state a rebuilt context starts from: told nothing, budget whole.
    pub fn reborn(&self) -> Self {
        Self::new(self.steers)
    }

    /// Clear the repair budget — the attend loop calls this on every
    /// exchange-opening item.  Edge state survives: a new exchange is not a
    /// new condition.
    pub fn reset(&mut self) {
        self.used = 0;
    }

    /// A standing condition's reminder for the tool boundary, its breadcrumb
    /// recorded.  Budget-free: the condition, not the exchange, decides when
    /// it is owed.
    pub fn remind(&self, reminder: &Reminder, log: &mut AgentLog) -> Option<String> {
        if !self.steers {
            return None;
        }
        record_nudge(log, reminder.cause(), None);
        Some(wrap_reminder(&reminder.body()))
    }

    /// Decide the synthetic prompt the attend loop self-posts, or `None` to
    /// accept the attempt as it stands.
    pub fn react(
        &mut self,
        attempt: Result<&Outcome, &ProviderError>,
        facts: &Facts,
        log: &mut AgentLog,
    ) -> Option<String> {
        if !self.steers || !facts.quiet {
            return None;
        }
        match attempt {
            Ok(Outcome::Complete) => self.on_complete(facts, log),
            Ok(Outcome::Empty) => self.repair("empty turn".into(), EMPTY_MESSAGE, log),
            Ok(Outcome::Stopped { reason }) => {
                self.repair(format!("stop={reason}"), EARLY_STOP_MESSAGE, log)
            }
            Err(ProviderError::Truncated { .. }) => {
                self.repair("truncated".into(), TRUNCATED_MESSAGE, log)
            }
            // A reply is final; a cancel was asked for; every other provider
            // error is the transport's own.
            _ => None,
        }
    }

    fn spent(&self) -> Spent {
        Spent {
            used: self.used,
            max: BUDGET,
        }
    }

    fn repair(&mut self, cause: String, message: &str, log: &mut AgentLog) -> Option<String> {
        if self.used >= BUDGET {
            record_error(
                log,
                format!("nudge budget exhausted ({BUDGET} attempts; last cause: {cause})"),
            );
            return None;
        }
        self.used += 1;
        record_nudge(log, cause, Some(self.spent()));
        Some(wrap_reminder(message))
    }

    fn on_complete(&mut self, facts: &Facts, log: &mut AgentLog) -> Option<String> {
        let mut parts = Vec::new();
        if facts.must_reply {
            if self.used >= BUDGET {
                // The un-replied finish is accepted; `agent_outcome` maps it
                // to Failed.  Returning here leaves the edges below armed.
                record_error(
                    log,
                    "agent finished without calling `reply` after the nudge budget; \
                     returning a failure"
                        .into(),
                );
                return None;
            }
            self.used += 1;
            record_nudge(
                log,
                "no-reply finish (returning agent)".into(),
                Some(self.spent()),
            );
            parts.push(REPLY_MESSAGE.to_string());
        }
        // Edge-triggered: the edge is consumed in the same act as the part's
        // emission, so "told but not sent" and "sent but not told" are
        // inexpressible.
        match &facts.pinned {
            Some(pinned) if self.pinned_told.as_ref() != Some(pinned) => {
                self.pinned_told = Some(pinned.clone());
                record_nudge(log, "pinned-state reminder".into(), None);
                parts.push(format!("There is pinned state: {pinned}"));
            }
            Some(_) => {}
            // An emptied register re-arms even an identical future digest.
            None => self.pinned_told = None,
        }
        (!parts.is_empty()).then(|| wrap_reminder(&parts.join(" ")))
    }
}

/// Durable and published in one call; a log that cannot take the breadcrumb
/// reports the fault, and the nudge still reaches the model.
fn record_nudge(log: &mut AgentLog, cause: String, spent: Option<Spent>) {
    if let Err(error) = log.record_nudge(cause, spent) {
        log.record_emitter().report_fault(&error);
    }
}

fn record_error(log: &mut AgentLog, text: String) {
    if let Err(error) = log.record_error(text) {
        log.record_emitter().report_fault(&error);
    }
}

// ── Messages ─────────────────────────────────────────────────────────

/// The no-reply reminder: the agent finished with prose but never called `reply`,
/// its sole return path.  Re-issued until [`BUDGET`] is spent, then the run fails.
const REPLY_MESSAGE: &str = "You ended your turn without calling `reply`, so your parent will \
    receive nothing. Return your result now with `ral { reply <value> }`: a string for a \
    markdown report, or a record/list for structured findings; if the value carries `$`, `!`, \
    or a quote, write it as a raw string `#'…'#`. This is the only way to hand your work back; \
    a final message on its own is not delivered.";

const EMPTY_MESSAGE: &str = "Your previous turn produced no text and no tool calls. \
    If you are finished, say so explicitly; otherwise continue.";

const EARLY_STOP_MESSAGE: &str = "Your previous turn ended early on a provider-side stop. \
    Reformulate the previous reply or take a different approach.";

const TRUNCATED_MESSAGE: &str = "Your previous reply was cut off before it completed. \
    Continue concisely from where it stopped.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Limit, Refusal};

    fn fresh_log() -> AgentLog {
        AgentLog::for_test(0, "test", &crate::agent::RecordedAccount::for_test("test"))
            .expect("session log")
    }

    fn facts() -> Facts {
        Facts {
            must_reply: false,
            pinned: None,
            quiet: true,
        }
    }

    const COMPLETE: Result<&Outcome, &ProviderError> = Ok(&Outcome::Complete);
    const EMPTY: Result<&Outcome, &ProviderError> = Ok(&Outcome::Empty);

    /// The transient exhausted the provider loop before the model produced output;
    /// nudging would resend that same invisible request as a fake user turn.
    #[test]
    fn transient_surfaces_without_nudge() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        let error = ProviderError::Transient {
            cause: "stream idle: no response within timeout".into(),
            attempts: 3,
            body: None,
            status: None,
        };
        assert!(
            nudges.react(Err(&error), &facts(), &mut log).is_none(),
            "transient provider failures are surfaced directly"
        );
        assert_eq!(nudges.used, 0);
    }

    /// The provider loop owns retry attempts; the budget here is only for
    /// model-visible recovery.
    #[test]
    fn provider_failures_do_not_consume_budget() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        let error = ProviderError::Refused(Refusal::for_test(Limit::Rate, None));
        for _ in 0..=BUDGET {
            assert!(
                nudges.react(Err(&error), &facts(), &mut log).is_none(),
                "provider failure should stop, not nudge"
            );
        }
        assert_eq!(nudges.used, 0);
    }

    #[test]
    fn empty_turn_nudges_and_consumes_budget() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        match nudges.react(EMPTY, &facts(), &mut log) {
            Some(msg) => assert!(msg.contains("no text and no tool calls")),
            None => panic!("empty turn must nudge, not stop"),
        }
        assert_eq!(nudges.used, 1);
    }

    #[test]
    fn exhausted_budget_stops() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        for _ in 0..BUDGET {
            assert!(nudges.react(EMPTY, &facts(), &mut log).is_some());
        }
        assert!(nudges.react(EMPTY, &facts(), &mut log).is_none());
    }

    /// A toolless trunk has nothing to be steered toward: no repair, no
    /// reminder, nothing synthetic joins its conversation.
    #[test]
    fn a_trunk_that_steers_nothing_says_nothing() {
        let mut nudges = Nudges::new(false);
        let mut log = fresh_log();
        assert!(nudges.react(EMPTY, &facts(), &mut log).is_none());
        let pressure = Reminder::Pressure {
            detail: "400 of 500 tokens".into(),
            planned: None,
        };
        assert!(nudges.remind(&pressure, &mut log).is_none());
        assert_eq!(nudges.used, 0);
    }

    /// Once the budget is spent the un-replied finish is accepted (`None`) so the
    /// loop ends; `agent_outcome` in `agent/attend.rs` maps that `Complete` to
    /// `Failed`.
    #[test]
    fn no_reply_finish_re_nudges_up_to_budget_then_fails() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        let f = || Facts {
            must_reply: true,
            ..facts()
        };
        for _ in 0..BUDGET {
            match nudges.react(COMPLETE, &f(), &mut log) {
                Some(msg) => assert!(msg.contains("`reply`")),
                None => {
                    panic!(
                        "a returning agent that did not reply must re-nudge while budget remains"
                    )
                }
            }
        }
        assert_eq!(
            nudges.used, BUDGET,
            "the no-reply nudge now draws on the budget"
        );
        assert!(
            nudges.react(COMPLETE, &f(), &mut log).is_none(),
            "past the budget the un-replied finish is accepted (mapped to Failed downstream)"
        );
    }

    /// A conversing agent never returns, so it owes no `reply`.
    #[test]
    fn root_completion_is_never_reply_nudged() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        assert!(nudges.react(COMPLETE, &facts(), &mut log).is_none());
    }

    /// A live child *is* the next action, so every nudge — the pin reminder
    /// included — waits for it to settle rather than pile on; and once it
    /// settles the pin reminder fires on the first quiet completion.
    #[test]
    fn pinned_state_waits_while_children_live() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        let busy = Facts {
            pinned: Some("tasks 3/8".into()),
            quiet: false,
            ..facts()
        };
        assert!(
            nudges.react(COMPLETE, &busy, &mut log).is_none(),
            "a live descendant is the next action, so the pin reminder should wait"
        );
        assert_eq!(nudges.used, 0, "waiting must not spend nudge budget");

        let quiet = Facts {
            pinned: Some("tasks 3/8".into()),
            quiet: true,
            ..facts()
        };
        let msg = nudges
            .react(COMPLETE, &quiet, &mut log)
            .expect("the first quiet completion after settling should nudge");
        assert!(msg.contains("There is pinned state: tasks 3/8"));
    }

    /// Both obligations compose into one reminder, each on its own accounting:
    /// the reply half spends budget, the pinned-state half does not.
    #[test]
    fn returning_agent_gets_both_reply_and_pinned_nudges() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        let f = Facts {
            must_reply: true,
            pinned: Some("tasks 3/8".into()),
            ..facts()
        };
        let msg = nudges
            .react(COMPLETE, &f, &mut log)
            .expect("a returning agent with pinned state should nudge");
        assert!(
            msg.contains("`reply`"),
            "must still be reminded to reply: {msg}"
        );
        assert!(
            msg.contains("There is pinned state: tasks 3/8"),
            "must also be reminded of its pinned state: {msg}"
        );
        assert_eq!(nudges.used, 1, "only the reply half spends budget");
    }

    #[test]
    fn reset_clears_budget() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        let _ = nudges.react(EMPTY, &facts(), &mut log);
        assert!(nudges.used >= 1);
        nudges.reset();
        assert_eq!(nudges.used, 0, "reset clears the budget");
    }

    /// `Replied` answers `None` for peer and headless-root facts alike, budget
    /// untouched — there is no self-verification round-trip left.
    #[test]
    fn any_reply_is_accepted_outright() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        for must_reply in [false, true] {
            let f = Facts {
                must_reply,
                ..facts()
            };
            assert!(nudges.react(Ok(&Outcome::Replied), &f, &mut log).is_none());
        }
        assert_eq!(nudges.used, 0);
    }

    /// The livelock regression: the same digest over repeated quiet
    /// completions fires exactly once; a changed digest fires again; and
    /// unpinning then re-pinning the identical digest fires again too.
    #[test]
    fn pin_reminder_fires_once_per_register_change() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        let with = |digest: &str| Facts {
            pinned: Some(digest.into()),
            ..facts()
        };

        let first = nudges
            .react(COMPLETE, &with("tasks 3/8"), &mut log)
            .expect("the first sighting of a digest must nudge");
        assert!(first.contains("tasks 3/8"));
        for _ in 0..3 {
            assert!(
                nudges
                    .react(COMPLETE, &with("tasks 3/8"), &mut log)
                    .is_none(),
                "a stationary digest must not re-fire"
            );
        }

        let changed = nudges
            .react(COMPLETE, &with("tasks 4/8"), &mut log)
            .expect("a changed digest must fire again");
        assert!(changed.contains("tasks 4/8"));

        // Unpin, then re-pin the identical digest: the edge re-arms through
        // `None`.
        let none = Facts {
            pinned: None,
            ..facts()
        };
        assert!(nudges.react(COMPLETE, &none, &mut log).is_none());
        let refired = nudges
            .react(COMPLETE, &with("tasks 4/8"), &mut log)
            .expect("re-pinning even the identical digest after an empty register must fire");
        assert!(refired.contains("tasks 4/8"));
    }

    /// A must-reply completion at budget with a pin due returns `None` and
    /// leaves that edge armed.
    #[test]
    fn exhausted_reply_budget_leaves_the_pin_edge_armed() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        let must_reply = Facts {
            must_reply: true,
            ..facts()
        };
        for _ in 0..BUDGET {
            assert!(nudges.react(COMPLETE, &must_reply, &mut log).is_some());
        }
        let pin_due = Facts {
            must_reply: true,
            pinned: Some("tasks 3/8".into()),
            ..facts()
        };
        assert!(
            nudges.react(COMPLETE, &pin_due, &mut log).is_none(),
            "past budget the un-replied finish is accepted"
        );

        nudges.reset();
        let no_must_reply = Facts {
            pinned: Some("tasks 3/8".into()),
            ..facts()
        };
        let msg = nudges
            .react(COMPLETE, &no_must_reply, &mut log)
            .expect("after reset the pin edge must still be armed");
        assert!(msg.contains("There is pinned state: tasks 3/8"), "{msg}");
    }

    /// `reset()` clears only the budget, not the edges; the rebirth clears
    /// everything.
    #[test]
    fn reset_does_not_rearm_edges() {
        let mut nudges = Nudges::new(true);
        let mut log = fresh_log();
        let f = Facts {
            pinned: Some("tasks 3/8".into()),
            ..facts()
        };
        assert!(nudges.react(COMPLETE, &f, &mut log).is_some());
        nudges.reset();
        assert!(
            nudges.react(COMPLETE, &f, &mut log).is_none(),
            "reset must not re-arm a told edge"
        );

        nudges = nudges.reborn();
        assert!(
            nudges.react(COMPLETE, &f, &mut log).is_some(),
            "the rebirth must fire again"
        );
    }

    /// The pressure reminder carries both the reading and the turns the next
    /// boundary would cut, with the line that makes the same cut by hand.
    #[test]
    fn pressure_names_the_cut_and_offers_the_note() {
        let body = Reminder::Pressure {
            detail: "400 of 500 tokens".into(),
            planned: Some(vec![1, 2, 3, 4, 5, 6, 7]),
        }
        .body();
        assert!(body.contains("400 of 500 tokens"), "{body}");
        assert!(
            body.contains("turns 1–7 will leave your context")
                && body.contains("`exarch-context `evict [turns: !{range 1 8}"),
            "must name the cut and offer the note: {body}"
        );
    }

    /// Nothing old enough to shed: the reading stands alone, with no cut to
    /// announce and no note to offer.
    #[test]
    fn pressure_without_a_planned_cut_states_the_reading_alone() {
        let body = Reminder::Pressure {
            detail: "400 of 500 tokens".into(),
            planned: None,
        }
        .body();
        assert!(body.contains("400 of 500 tokens"), "{body}");
        assert!(
            !body.contains("evict") && !body.contains("will leave your context"),
            "with no cut planned there is nothing to announce: {body}"
        );
    }

    /// A reminder records its breadcrumb spending nothing: the budget is the
    /// repairs' alone.
    #[test]
    fn a_reminder_spends_no_budget() {
        let nudges = Nudges::new(true);
        let mut log = fresh_log();
        let reminder = Reminder::Ration {
            service: "openrouter".into(),
            label: "5 hours".into(),
            pct: 91,
            windowed: true,
            resumes: true,
        };
        let text = nudges
            .remind(&reminder, &mut log)
            .expect("a steering trunk is reminded");
        assert!(
            text.contains("91% spent") && text.contains("pauses until it resets"),
            "{text}"
        );
        assert_eq!(nudges.used, 0);
    }
}
