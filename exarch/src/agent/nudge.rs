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
use crate::provider::ProviderError;
use crate::record::{AgentLog, Spent};

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
mod tests;
