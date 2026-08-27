//! Inter-attempt nudges.
//!
//! After each deliberation, either accept the outcome or hand back a synthetic
//! prompt for the attend loop to post to itself as a `Post::Nudge`.  Reporting
//! the outcome is the attend loop's own step, taken before this one.
//!
//! The registry is per-session because the attend loop runs one
//! `Agent::deliberate` per inbox item, not one per exchange, so this state must
//! outlive a single deliberation.  `Registry::reset` clears it at a genuine
//! exchange boundary, never on a self-nudge.

use crate::agent::deliberate;
use crate::agent::event::AgentLog;
use crate::provider::ProviderError;

/// Outer-attempt budget per user exchange, distinct from the provider's own
/// transport retries: this one is for model-visible recovery.
const BUDGET: u32 = 3;
/// Exchanges between "nothing is pinned" reminders.  The reminder for state that
/// *is* pinned is budget-free and fires on every clean completion instead.
const REMIND_EVERY: u32 = 12;
/// Brackets every synthetic nudge, so the model can tell a system reminder from
/// genuine user input.
const EXARCH_REMINDER_OPEN: &str = "[EXARCH_REMINDER // Do not mention to user.] ";
const EXARCH_REMINDER_CLOSE: &str = " [/EXARCH_REMINDER]";
/// A degenerate-outcome classifier: `None` if the attempt doesn't match, else the
/// `cause` recorded to log and trace and the `message` sent to the model.
type Rule = fn(&Result<deliberate::Outcome, ProviderError>) -> Option<(String, &'static str)>;

const RULES: &[Rule] = &[on_empty, on_early_stop, on_truncated];

fn wrap_reminder(body: &str) -> String {
    format!("{EXARCH_REMINDER_OPEN}{body}{EXARCH_REMINDER_CLOSE}")
}

/// The agent-side facts a nudge decision needs, as against the attempt outcome.
/// Assembled by `take_up` in `agent/attend.rs`.
pub(crate) struct NudgeCtx {
    /// True for an agent that returns through `reply` — a peer or a headless
    /// root, never the interactive root.  Only shapes what the reply reminder
    /// says; `quiet` alone decides whether it fires at all.
    pub must_reply: bool,
    /// One-line digest of the whole pin register, `None` when nothing is pinned.
    pub pinned: Option<String>,
    /// True for the headless root (`parent.is_none() && !interactive`), whose
    /// `reply` reaches an external caller with no parent left to push back on it.
    pub is_headless_root: bool,
    /// The rendered pressure gauge once the soft line ahead of auto-compaction
    /// is crossed — `Some` only when that line is crossed *and* unlatched, so
    /// one excursion draws one reminder.  The attend loop owns the latch;
    /// this module only ever reports what it was handed.
    pub pressure: Option<String>,
    /// Nothing else is already carrying this agent forward: no reply standing
    /// for a parent to fetch, no detached shell work, no busy children.  The
    /// one gate every nudge kind shares — `false` and `react` returns `None`
    /// unspent, budget untouched.
    pub quiet: bool,
}

/// Per-session nudge state; [`Self::react`] is the only post-attempt entry point.
pub(crate) struct Registry {
    used: u32,
    /// Unlike the budget, accumulates *across* exchanges: bumped by
    /// [`Self::reset`], zeroed once it reaches [`REMIND_EVERY`] and fires.
    exchanges_since_no_pins_reminder: u32,
    /// One-shot for the whole run, not per exchange — [`Self::reset`] never
    /// clears it, since the obligation is "verify once before the result stands".
    reply_verified: bool,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            used: 0,
            exchanges_since_no_pins_reminder: 0,
            reply_verified: false,
        }
    }

    /// Clear the retry budget and count one exchange toward the no-pins reminder.
    /// The attend loop calls this only on a genuine exchange-boundary item, never
    /// on a self-nudge, which is the same exchange continuing.
    pub fn reset(&mut self) {
        self.used = 0;
        self.exchanges_since_no_pins_reminder += 1;
    }

    pub(crate) fn reset_budget(&mut self) {
        self.used = 0;
    }

    /// The one decider: walk [`RULES`] against `attempt` and return the synthetic
    /// prompt the attend loop self-posts as a `Post::Nudge`, or `None` to stop.
    pub fn react(
        &mut self,
        attempt: &Result<deliberate::Outcome, ProviderError>,
        ctx: &NudgeCtx,
        log: &mut AgentLog,
    ) -> Option<String> {
        // A headless root's first `reply` is turned back before the rules see it,
        // budget-free, for self-verification; the latch then honours the very next
        // one whatever it carries, so the attend loop can terminate.
        if matches!(attempt, Ok(deliberate::Outcome::Replied(_))) {
            if ctx.is_headless_root && !self.reply_verified {
                self.reply_verified = true;
                record_nudge(log, self.used, "reply verification".into());
                return Some(wrap_reminder(VERIFY_REPLY_MESSAGE));
            }
            return None;
        }
        if !ctx.quiet {
            return None;
        }
        let Some((cause, message)) = RULES.iter().find_map(|&r| r(attempt)) else {
            // Only a clean completion reaches the nudges below; anything else that
            // no rule claimed — an unclassified provider error, a `Cancelled` or
            // `Capped` end — is accepted as-is.
            if !matches!(attempt, Ok(deliberate::Outcome::Complete(_))) {
                return None;
            }
            // Two independent obligations, neither suppressing the other: a
            // returning agent owes a `reply` whatever its pins, and any agent owes
            // a clean pin register whether or not it returns.  Compose both.
            let mut parts = Vec::new();
            if ctx.must_reply {
                if self.used < BUDGET {
                    self.used += 1;
                    record_nudge(log, self.used, "no-reply finish (returning agent)".into());
                    parts.push(REPLY_MESSAGE.to_string());
                } else {
                    // An agent that cannot call `reply` even after the full budget
                    // fails outright, whatever it still has pinned.
                    let msg = "agent finished without calling `reply` after the nudge budget; \
                               returning a failure"
                        .to_string();
                    if let Err(error) = log.record_error(msg) {
                        eprintln!("exarch: a nudge-budget failure was not recorded: {error}");
                    }
                    return None;
                }
            }
            // Anything pinned keeps the agent restless on every clean completion,
            // budget-free; an empty register gets a throttled suggestion instead.
            if let Some(pinned) = ctx.pinned.as_deref() {
                record_nudge(log, self.used, "pinned-state reminder".into());
                parts.push(format!("There is pinned state: {pinned}"));
            } else if self.exchanges_since_no_pins_reminder >= REMIND_EVERY {
                self.exchanges_since_no_pins_reminder = 0;
                record_nudge(log, self.used, "no-pins reminder".into());
                parts.push(
                    "Nothing is pinned — consider calling `set-goal` to remember what you are \
                     working on, or tracking some tasks with `add-task`."
                        .to_string(),
                );
            }
            // Context pressure is about this agent's own history alone.
            if let Some(detail) = &ctx.pressure {
                record_nudge(log, self.used, "context pressure".into());
                parts.push(format!("Context pressure: {detail}. {PRESSURE_MESSAGE}"));
            }
            if parts.is_empty() {
                return None;
            }
            return Some(wrap_reminder(&parts.join(" ")));
        };
        if self.used >= BUDGET {
            let msg = format!("nudge budget exhausted ({BUDGET} attempts; last cause: {cause})");
            if let Err(error) = log.record_error(msg) {
                eprintln!("exarch: a nudge-budget failure was not recorded: {error}");
            }
            return None;
        }
        self.used += 1;
        record_nudge(log, self.used, cause);
        Some(wrap_reminder(message))
    }
}

/// The one call through the seam: `record_nudge` is durable and published in
/// the same breath, so there is no separate display-side emit left to keep in
/// step with it.  Best effort at this last resort alone — a nudge whose
/// breadcrumb cannot land still has to reach the model.
fn record_nudge(log: &mut AgentLog, used: u32, cause: String) {
    if let Err(error) = log.record_nudge(used, BUDGET, cause) {
        eprintln!("exarch: a nudge breadcrumb was not recorded: {error}");
    }
}

/// The headless root's one-shot reply-verification nudge.
const VERIFY_REPLY_MESSAGE: &str = "Recall the original prompt, and ensure that you \
    have correctly responded to it, completing any tasks. Then call `reply` again, with \
    `ral { reply <value> }`.";

// ── Rules ────────────────────────────────────────────────────────────

/// The no-reply reminder: the agent finished with prose but never called `reply`,
/// its sole return path.  Re-issued until [`BUDGET`] is spent, then the run fails.
const REPLY_MESSAGE: &str = "You ended your turn without calling `reply`, so your parent will \
    receive nothing. Return your result now with `ral { reply <value> }` — a string for a \
    markdown report, or a record/list for structured findings; if the value carries `$`, `!`, \
    or a quote, write it as a raw string `#'…'#`. This is the only way to hand your work back; \
    a final message on its own is not delivered.";

/// Shown once [`NudgeCtx::pressure`] crosses its soft line, budget-free like
/// the pinned-state reminder: durable state belongs in files, not bindings,
/// so it survives the summary that compaction is about to fold history into.
const PRESSURE_MESSAGE: &str = "Older history will be auto-compacted into a summary soon. \
    Preserve what must survive verbatim: write durable state to files and keep the paths — do \
    not park large values in bindings — and record your intent with `set-goal`/`add-task`.";

fn on_empty(r: &Result<deliberate::Outcome, ProviderError>) -> Option<(String, &'static str)> {
    match r {
        Ok(deliberate::Outcome::Empty) => Some((
            "empty turn".into(),
            "Your previous turn produced no text and no tool calls. \
             If you are finished, say so explicitly; otherwise continue.",
        )),
        _ => None,
    }
}

fn on_early_stop(r: &Result<deliberate::Outcome, ProviderError>) -> Option<(String, &'static str)> {
    match r {
        Ok(deliberate::Outcome::Stopped { reason }) => Some((
            format!("stop={reason}"),
            "Your previous turn ended early on a provider-side stop. \
             Reformulate the previous reply or take a different approach.",
        )),
        _ => None,
    }
}

fn on_truncated(r: &Result<deliberate::Outcome, ProviderError>) -> Option<(String, &'static str)> {
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
    use ral_core::serial::FOValue;

    fn fresh_log() -> AgentLog {
        AgentLog::for_test(0, "test", &crate::agent::RecordedAccount::for_test("test"))
            .expect("session log")
    }

    /// The transient exhausted the provider loop before the model produced output;
    /// nudging would resend that same invisible request as a fake user turn.
    #[test]
    fn transient_surfaces_without_nudge() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
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
            quiet: true,
            pressure: None,
        };
        assert!(
            reg.react(&attempt(), &ctx(), &mut log).is_none(),
            "transient provider failures are surfaced directly"
        );
        assert_eq!(reg.used, 0);
    }

    /// The provider loop owns retry attempts; the budget here is only for
    /// model-visible recovery.
    #[test]
    fn provider_failures_do_not_consume_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
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
            quiet: true,
            pressure: None,
        };
        for _ in 0..=BUDGET {
            assert!(
                reg.react(&attempt(), &ctx(), &mut log).is_none(),
                "provider failure should stop, not nudge"
            );
        }
        assert_eq!(reg.used, 0);
    }

    #[test]
    fn empty_turn_nudges_and_consumes_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        match reg.react(
            &Ok(deliberate::Outcome::Empty),
            &NudgeCtx {
                must_reply: false,
                is_headless_root: false,
                pinned: None,
                quiet: true,
                pressure: None,
            },
            &mut log,
        ) {
            Some(msg) => assert!(msg.contains("no text and no tool calls")),
            None => panic!("empty turn must nudge, not stop"),
        }
        assert_eq!(reg.used, 1);
    }

    #[test]
    fn exhausted_budget_stops() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        for _ in 0..BUDGET {
            assert!(
                reg.react(
                    &Ok(deliberate::Outcome::Empty),
                    &NudgeCtx {
                        must_reply: false,
                        is_headless_root: false,
                        pinned: None,
                        quiet: true,
                        pressure: None,
                    },
                    &mut log,
                )
                .is_some()
            );
        }
        assert!(
            reg.react(
                &Ok(deliberate::Outcome::Empty),
                &NudgeCtx {
                    must_reply: false,
                    is_headless_root: false,
                    pinned: None,
                    quiet: true,
                    pressure: None,
                },
                &mut log,
            )
            .is_none()
        );
    }

    /// Once the budget is spent the un-replied finish is accepted (`None`) so the
    /// loop ends; `agent_outcome` in `agent/attend.rs` maps that `Complete` to
    /// `Failed`.
    #[test]
    fn no_reply_finish_re_nudges_up_to_budget_then_fails() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = || NudgeCtx {
            must_reply: true,
            is_headless_root: false,
            pinned: None,
            quiet: true,
            pressure: None,
        };
        for _ in 0..BUDGET {
            match reg.react(
                &Ok(deliberate::Outcome::Complete("prose, no reply".into())),
                &ctx(),
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
        assert!(
            reg.react(
                &Ok(deliberate::Outcome::Complete("still no reply".into())),
                &ctx(),
                &mut log,
            )
            .is_none(),
            "past the budget the un-replied finish is accepted (mapped to Failed downstream)"
        );
    }

    /// The interactive root converses and never returns, so it owes no `reply`.
    #[test]
    fn root_completion_is_never_reply_nudged() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        assert!(
            reg.react(
                &Ok(deliberate::Outcome::Complete("answer to the user".into())),
                &NudgeCtx {
                    must_reply: false,
                    is_headless_root: false,
                    pinned: None,
                    quiet: true,
                    pressure: None,
                },
                &mut log,
            )
            .is_none()
        );
    }

    /// Its result reaches an external caller with nobody left to scrutinize it.
    #[test]
    fn headless_root_first_reply_is_turned_back_for_verification() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = || NudgeCtx {
            must_reply: true,
            is_headless_root: true,
            pinned: None,
            quiet: true,
            pressure: None,
        };
        let msg = reg
            .react(
                &Ok(deliberate::Outcome::Replied(FOValue::String {
                    value: "done".into(),
                })),
                &ctx(),
                &mut log,
            )
            .expect("a headless root's first reply must be turned back");
        assert!(msg.contains("call `reply` again"), "{msg}");
        assert_eq!(reg.used, 0, "the verify nudge is budget-free");
    }

    /// The latch is one-shot for the run, not re-armed by the pinned or
    /// must-reply state, so the second `reply` stands whatever it carries.
    #[test]
    fn headless_root_second_reply_is_honoured() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = || NudgeCtx {
            must_reply: true,
            is_headless_root: true,
            pinned: None,
            quiet: true,
            pressure: None,
        };
        assert!(
            reg.react(
                &Ok(deliberate::Outcome::Replied(FOValue::String {
                    value: "first".into()
                })),
                &ctx(),
                &mut log,
            )
            .is_some(),
            "the first reply is turned back"
        );
        assert!(
            reg.react(
                &Ok(deliberate::Outcome::Replied(FOValue::String {
                    value: "second, verified".into(),
                })),
                &ctx(),
                &mut log,
            )
            .is_none(),
            "the second reply must be accepted so the run can terminate"
        );
    }

    /// A peer returns too, but its parent reads the result and can push back, so
    /// only the headless root needs the self-verification nudge.
    #[test]
    fn peer_reply_is_never_verify_nudged() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        assert!(
            reg.react(
                &Ok(deliberate::Outcome::Replied(FOValue::String {
                    value: "findings".into()
                })),
                &NudgeCtx {
                    must_reply: true,
                    is_headless_root: false,
                    pinned: None,
                    quiet: true,
                    pressure: None,
                },
                &mut log,
            )
            .is_none(),
            "a sub-agent peer's reply is accepted at once"
        );
    }

    /// Anything pinned keeps the agent restless, budget-free.  This module only
    /// reports the pin register; nothing here ever clears it.
    #[test]
    fn pinned_state_keeps_completion_restless() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = || NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: Some("tasks 3/8".into()),
            quiet: true,
            pressure: None,
        };
        for _ in 0..3 {
            let msg = reg
                .react(
                    &Ok(deliberate::Outcome::Complete("done".into())),
                    &ctx(),
                    &mut log,
                )
                .expect("pinned state should nudge");
            assert!(msg.contains("There is pinned state: tasks 3/8"));
        }
        assert_eq!(reg.used, 0, "pinned-state nudges are budget-free");
    }

    /// A live child *is* the next action, so every nudge — the pin reminder
    /// included — waits for it to settle rather than pile on.
    #[test]
    fn pinned_state_waits_while_children_live() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        assert!(
            reg.react(
                &Ok(deliberate::Outcome::Complete(
                    "waiting on a descendant".into()
                )),
                &NudgeCtx {
                    must_reply: false,
                    is_headless_root: false,
                    pinned: Some("tasks 3/8".into()),
                    quiet: false,
                    pressure: None,
                },
                &mut log,
            )
            .is_none(),
            "a live descendant is the next action, so the pin reminder should wait"
        );
        assert_eq!(reg.used, 0, "waiting must not spend nudge budget");
    }

    /// Both obligations compose into one reminder, each on its own accounting:
    /// the reply half spends budget, the pinned-state half does not.
    #[test]
    fn returning_agent_gets_both_reply_and_pinned_nudges() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = || NudgeCtx {
            must_reply: true,
            is_headless_root: false,
            pinned: Some("tasks 3/8".into()),
            quiet: true,
            pressure: None,
        };
        let msg = reg
            .react(
                &Ok(deliberate::Outcome::Complete("prose, no reply".into())),
                &ctx(),
                &mut log,
            )
            .expect("a returning agent with pinned state should nudge");
        assert!(
            msg.contains("`reply`"),
            "must still be reminded to reply: {msg}"
        );
        assert!(
            msg.contains("There is pinned state: tasks 3/8"),
            "must also be reminded of its pinned state: {msg}"
        );
        assert_eq!(reg.used, 1, "only the reply half spends budget");
    }

    /// The pressure gauge composes on a clean completion, budget-free, and
    /// carries both the detail and the file-paths-over-bindings guidance.
    #[test]
    fn pressure_part_composes_budget_free_on_complete() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: None,
            quiet: true,
            pressure: Some("400 of 500 tokens".into()),
        };
        let msg = reg
            .react(
                &Ok(deliberate::Outcome::Complete("done".into())),
                &ctx,
                &mut log,
            )
            .expect("pressure due should nudge");
        assert!(msg.contains("400 of 500 tokens"), "{msg}");
        assert!(
            msg.contains("write durable state to files")
                && msg.contains("do not park large values in bindings"),
            "must steer state to files, not bindings: {msg}"
        );
        assert_eq!(reg.used, 0, "the pressure nudge is budget-free");
    }

    /// All three obligations join one message: the returning agent's `reply`,
    /// its pinned state, and its context pressure — only the first spends
    /// budget.
    #[test]
    fn pressure_composes_alongside_reply_and_pinned_parts() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = NudgeCtx {
            must_reply: true,
            is_headless_root: false,
            pinned: Some("tasks 3/8".into()),
            quiet: true,
            pressure: Some("400 of 500 tokens".into()),
        };
        let msg = reg
            .react(
                &Ok(deliberate::Outcome::Complete("prose, no reply".into())),
                &ctx,
                &mut log,
            )
            .expect("all three obligations should nudge");
        assert!(msg.contains("`reply`"), "{msg}");
        assert!(msg.contains("There is pinned state: tasks 3/8"), "{msg}");
        assert!(msg.contains("400 of 500 tokens"), "{msg}");
        assert_eq!(reg.used, 1, "only the reply half spends budget");
    }

    /// `quiet` is the one gate every nudge kind shares: a live child suppresses
    /// the pressure gauge exactly as it suppresses the pin reminder.
    #[test]
    fn pressure_waits_while_children_live_too() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: Some("tasks 3/8".into()),
            quiet: false,
            pressure: Some("400 of 500 tokens".into()),
        };
        assert!(
            reg.react(
                &Ok(deliberate::Outcome::Complete(
                    "waiting on a descendant".into(),
                )),
                &ctx,
                &mut log,
            )
            .is_none(),
            "not quiet: nothing nudges, pressure included"
        );
    }

    /// A `reply` is accepted (or turned back for verification) on its own
    /// terms; the pressure gauge never rides along on that path.
    #[test]
    fn pressure_is_not_delivered_on_replied() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: None,
            quiet: true,
            pressure: Some("400 of 500 tokens".into()),
        };
        assert!(
            reg.react(
                &Ok(deliberate::Outcome::Replied(FOValue::String {
                    value: "done".into(),
                })),
                &ctx,
                &mut log,
            )
            .is_none(),
            "a reply is accepted outright; pressure does not apply to it"
        );
    }

    #[test]
    fn reset_clears_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = || NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: None,
            quiet: true,
            pressure: None,
        };
        let _ = reg.react(&Ok(deliberate::Outcome::Empty), &ctx(), &mut log);
        assert!(reg.used >= 1);
        reg.reset();
        assert_eq!(reg.used, 0, "reset clears the budget");
    }

    #[test]
    fn context_edit_budget_reset_does_not_advance_exchange_reminders() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: None,
            quiet: true,
            pressure: None,
        };
        let _ = reg.react(&Ok(deliberate::Outcome::Empty), &ctx, &mut log);
        reg.reset();
        let before = reg.exchanges_since_no_pins_reminder;
        let _ = reg.react(&Ok(deliberate::Outcome::Empty), &ctx, &mut log);
        reg.reset_budget();
        assert_eq!(reg.used, 0, "a rewind clears the retry budget");
        assert_eq!(
            reg.exchanges_since_no_pins_reminder, before,
            "a context edit does not count as another exchange"
        );
    }

    /// Exactly once per [`REMIND_EVERY`]-exchange window, not on every exchange
    /// in between.
    #[test]
    fn no_pins_reminder_fires_every_remind_every_exchanges() {
        let mut reg = Registry::new();
        let mut log = fresh_log();
        let ctx = || NudgeCtx {
            must_reply: false,
            is_headless_root: false,
            pinned: None,
            quiet: true,
            pressure: None,
        };
        let mut fires = 0;
        for exchange in 1..=(REMIND_EVERY + 3) {
            reg.reset();
            if let Some(msg) = reg.react(
                &Ok(deliberate::Outcome::Complete("x".into())),
                &ctx(),
                &mut log,
            ) {
                assert_eq!(
                    exchange, REMIND_EVERY,
                    "no-pins reminder should only fire on the {REMIND_EVERY}th exchange"
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
