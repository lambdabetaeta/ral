//! The lines the harness speaks to the model in its own voice.
//!
//! Two kinds of nudge, one message: a **repair** (`Empty`, `Stopped`,
//! `Truncated`, a returning agent's un-replied `Complete`) is event-shaped,
//! exceptional, and spends the consecutive-repair [`BUDGET`]; a **standing
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
//! `Agent::deliberate` per attempt within an exchange, so this state
//! must outlive a single deliberation.  [`Nudges::reset`] clears the repair
//! count at an exchange opening and after each completed tool round, never on a
//! self-nudge; the edge fields survive it.  Every path that discards a
//! decided-but-uncommitted nudge (`/clear`, `/rewind`) is reborn whole
//! ([`Nudges::reborn`]), since a decided edge cannot be un-consumed piecemeal.

use crate::agent::gauge::{Load, Reminder, Unit};
use crate::agent::outcome::Outcome;
use crate::provider::{CutShort, ProviderError};
use crate::record::{AgentLog, Forensic, Spent};

/// Consecutive model-visible repairs, distinct from the provider's transport
/// retries.
const BUDGET: u32 = 3;

/// Brackets every synthetic nudge, so the model can tell a system reminder
/// from genuine user input.
const EXARCH_REMINDER_OPEN: &str = "[EXARCH_REMINDER // Do not mention to user.] ";
const EXARCH_REMINDER_CLOSE: &str = " [/EXARCH_REMINDER]";

fn wrap_reminder(body: &str) -> String {
    format!("{EXARCH_REMINDER_OPEN}{body}{EXARCH_REMINDER_CLOSE}")
}

impl Reminder {
    fn cause(&self) -> String {
        match self {
            Self::Pressure(_) => "context pressure".into(),
            Self::Ration { pct, .. } => format!("usage {pct}%"),
        }
    }

    /// The forced eviction is announced before it comes, so the model can
    /// leave its future self a line.  With nothing old enough to shed there is
    /// no span to name, and the trigger alone is the whole warning.
    fn body(&self) -> String {
        match self {
            Self::Pressure(
                load @ Load {
                    planned: Some(span),
                    ..
                },
            ) => {
                let (they, them) = if span.is_single() {
                    ("It", "it")
                } else {
                    ("They", "them")
                };
                format!(
                    "Context pressure: {}. Once it passes {}, a forced eviction will take the \
                     oldest part of your context; as things stand, that is {span}. {they} would \
                     stay readable with `exarch-transcript`, but a forced eviction leaves no \
                     note. To leave your future self a line, evict {them} yourself now: \
                     `` exarch-context `evict [from: {}, to: {}, note: '…'] ``.",
                    load.detail(),
                    load.trigger(),
                    span.from(),
                    span.to()
                )
            }
            Self::Pressure(load) => format!(
                "Context pressure: {}. A forced eviction is due once it passes {}.",
                load.detail(),
                load.trigger()
            ),
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
                    (false, _) => {
                        "When it runs out, requests fail until its limit is raised or its allowance replenished."
                    }
                };
                format!("Your {service} usage allowance is {pct}% spent ({label}). {outcome}")
            }
        }
    }
}

impl Load {
    /// What the context weighs: `173000 of 200000 tokens`, or `400 KB` where
    /// the window is unknown.
    fn detail(&self) -> String {
        let Self {
            used, total, unit, ..
        } = *self;
        match total {
            Some(total) => format!(
                "{} of {} {}",
                unit.count(used),
                unit.count(total),
                unit.name()
            ),
            None => format!("{} {}", unit.count(used), unit.name()),
        }
    }

    /// Where a forced eviction falls: `170000 tokens`, or `500 KB`.
    fn trigger(&self) -> String {
        format!("{} {}", self.unit.count(self.trigger), self.unit.name())
    }
}

impl Unit {
    /// A figure as the model reads it: tokens as counted, bytes in whole KB.
    fn count(self, figure: u64) -> u64 {
        match self {
            Self::Tokens => figure,
            Self::Bytes => figure / 1024,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::Bytes => "KB",
        }
    }
}

/// The agent-side facts one nudge decision reads, assembled by `follow_up` in
/// `agent/attend.rs`.
pub(crate) struct Facts {
    /// True for an agent that returns through `reply` — a fork or the
    /// headless root, never a conversing one.
    pub must_reply: bool,
    /// One-line digest of the whole pin register, `None` when nothing is
    /// pinned.
    pub pinned: Option<String>,
    /// No running worker and no busy children.
    pub quiet: bool,
}

/// Per-session nudge state, entered from [`Self::react`] at the end of a
/// deliberation and [`Self::remind`] at a tool boundary within one.
pub(crate) struct Nudges {
    /// Whether the model holds a tool to be steered toward.  Off — the
    /// toolless `--chat` trunk — every nudge would steer it toward a tool it
    /// does not have, so nothing is ever said.
    steers: bool,
    /// Consecutive repairs spent.
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

    /// Clear the repair budget: at every exchange-opening item and after every
    /// completed tool round.  Edge state survives: a new exchange is not a new
    /// condition.
    pub fn reset(&mut self) {
        self.used = 0;
    }

    /// A standing condition's reminder for the tool boundary, its breadcrumb
    /// recorded.  Budget-free: the condition, not the exchange, decides when
    /// it is owed.
    pub fn remind(&self, reminder: &Reminder, log: &AgentLog) -> Option<String> {
        if !self.steers {
            return None;
        }
        log.record_emitter().emit_or_end(Forensic::Nudge {
            cause: reminder.cause(),
            spent: None,
        });
        Some(wrap_reminder(&reminder.body()))
    }

    /// Decide the exchange-local continuation prompt, or `None` to
    /// accept the attempt as it stands.
    pub fn react(
        &mut self,
        attempt: Result<&Outcome, &ProviderError>,
        facts: &Facts,
        log: &AgentLog,
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
            Err(ProviderError::Truncated { cause }) => match cause.as_ref() {
                CutShort::OutputCap { .. } => {
                    self.repair("truncated".into(), TRUNCATED_MESSAGE, log)
                }
                CutShort::Stalled(_) => self.repair("stalled".into(), STALLED_MESSAGE, log),
            },
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

    fn repair(&mut self, cause: String, message: &str, log: &AgentLog) -> Option<String> {
        if self.used >= BUDGET {
            log.record_emitter().emit_or_end(Forensic::Error {
                text: format!("nudge budget exhausted ({BUDGET} attempts; last cause: {cause})"),
            });
            return None;
        }
        self.used += 1;
        log.record_emitter().emit_or_end(Forensic::Nudge {
            cause,
            spent: Some(self.spent()),
        });
        Some(wrap_reminder(message))
    }

    fn on_complete(&mut self, facts: &Facts, log: &AgentLog) -> Option<String> {
        let mut parts = Vec::new();
        if facts.must_reply {
            if self.used >= BUDGET {
                // The un-replied finish is accepted; `agent_outcome` maps it
                // to Failed.  Returning here leaves the edges below armed.
                log.record_emitter().emit_or_end(Forensic::Error {
                    text: "agent finished without calling `reply` after the nudge budget; \
                           returning a failure"
                        .into(),
                });
                return None;
            }
            self.used += 1;
            log.record_emitter().emit_or_end(Forensic::Nudge {
                cause: "no-reply finish (returning agent)".into(),
                spent: Some(self.spent()),
            });
            parts.push(REPLY_MESSAGE.to_string());
        }
        // Edge-triggered: the edge is consumed in the same act as the part's
        // emission, so "told but not sent" and "sent but not told" are
        // inexpressible.
        match &facts.pinned {
            Some(pinned) if self.pinned_told.as_ref() != Some(pinned) => {
                self.pinned_told = Some(pinned.clone());
                log.record_emitter().emit_or_end(Forensic::Nudge {
                    cause: "pinned-state reminder".into(),
                    spent: None,
                });
                parts.push(format!("There is pinned state: {pinned}"));
            }
            Some(_) => {}
            // An emptied register re-arms even an identical future digest.
            None => self.pinned_told = None,
        }
        (!parts.is_empty()).then(|| wrap_reminder(&parts.join(" ")))
    }
}

// ── Messages ─────────────────────────────────────────────────────────

/// The no-reply reminder: the agent finished with prose but never called `reply`,
/// its sole return path.  Re-issued until [`BUDGET`] is spent, then the run fails.
const REPLY_MESSAGE: &str = "You ended your turn without calling `reply`, so nothing has been \
    returned. Return your result now with `` exarch-agents `reply <value> ``: a string for a \
    markdown report, or a record/list for structured findings; if the value carries `$`, `!`, \
    or a quote, write it as a raw string `#'…'#`. This is the only way to hand your work back; \
    a final message on its own is not delivered.";

const EMPTY_MESSAGE: &str = "Your previous turn produced no text and no tool calls. \
    If you are finished, say so explicitly; otherwise continue.";

const EARLY_STOP_MESSAGE: &str = "Your previous turn ended early on a provider-side stop. \
    Reformulate the previous reply or take a different approach.";

const TRUNCATED_MESSAGE: &str = "Your previous reply was cut off before it completed. \
    Continue concisely from where it stopped.";

const STALLED_MESSAGE: &str = "The connection to the model failed partway through your reply. \
    What you had written so far is kept above; continue from where it ends.";

#[cfg(test)]
mod tests;
