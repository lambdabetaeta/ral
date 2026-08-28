//! Inter-attempt nudges.
//!
//! After each deliberation, either accept the outcome or hand back a synthetic
//! prompt for the attend loop to post to itself as a `Post::Nudge`.  Reporting
//! the outcome is the attend loop's own step, taken before this one.
//!
//! Two kinds of nudge, one message: a **repair** (`Empty`, `Stopped`,
//! `Truncated`, a returning agent's un-replied `Complete`) is event-shaped,
//! exceptional, and spends the per-exchange [`BUDGET`]; a **standing
//! condition** (the pin register, context pressure) is a fact about the
//! agent's own live state, told once when it changes and never re-told while
//! it holds — budget-free.  An edge is consumed in the same act as the part's
//! emission, so "told but not sent" and "sent but not told" are
//! inexpressible.
//!
//! [`Nudges`] is per-session because the attend loop runs one
//! `Avatar::deliberate` per inbox item, not one per exchange, so this state
//! must outlive a single deliberation.  [`Nudges::reset`] clears the budget at
//! a genuine exchange boundary, never on a self-nudge; the edge fields survive
//! it — a new exchange is not a new condition.  Every path that discards a
//! decided-but-uncommitted nudge (`/clear`, `/rewind`) rebuilds `Nudges`
//! whole, since a decided edge cannot be un-consumed piecemeal.

use crate::agent::deliberate;
use crate::agent::event::AgentLog;
use crate::provider::ProviderError;

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

/// One stateless reading of the context-pressure gauge, taken by the caller.
/// `Unknown` (a stale token measure under a known window) must neither warn
/// nor re-arm: a stale measure relieves nothing.
pub(crate) enum Pressure {
    /// The rendered detail, e.g. "173000 of 200000 tokens".
    Over(String),
    Under,
    Unknown,
}

/// The agent-side facts one nudge decision reads, assembled by `take_up` in
/// `agent/attend.rs`.
pub(crate) struct Facts {
    /// True for an agent that returns through `reply` — a peer or the
    /// headless root, never the interactive root.
    pub must_reply: bool,
    /// One-line digest of the whole pin register, `None` when nothing is
    /// pinned.
    pub pinned: Option<String>,
    pub pressure: Pressure,
    /// Nothing else is already carrying this agent forward: no reply standing
    /// for a parent to fetch, no detached shell work, no busy children.  The
    /// one gate every nudge kind shares — `false` and `react` returns `None`
    /// unspent, budget untouched.
    pub quiet: bool,
}

/// Per-session nudge state; [`Self::react`] is the only post-attempt entry
/// point.
pub(crate) struct Nudges {
    /// Repairs spent this exchange.
    used: u32,
    /// The pin digest last told to the model; a differing register re-arms.
    pinned_told: Option<String>,
    /// This pressure excursion has been told; an `Under` reading re-arms.
    pressure_told: bool,
}

impl Nudges {
    pub fn new() -> Self {
        Self {
            used: 0,
            pinned_told: None,
            pressure_told: false,
        }
    }

    /// Clear the repair budget — the attend loop calls this on every
    /// exchange-opening item.  Edge state survives: a new exchange is not a
    /// new condition.
    pub fn reset(&mut self) {
        self.used = 0;
    }

    /// Decide the synthetic prompt the attend loop self-posts, or `None` to
    /// accept the attempt as it stands.
    pub fn react(
        &mut self,
        attempt: &Result<deliberate::Outcome, ProviderError>,
        facts: &Facts,
        log: &mut AgentLog,
    ) -> Option<String> {
        // The gauge re-arms on every attempt, ahead of the quiet gate: an
        // excursion can end while a reply or busy children hold the nudge
        // back.
        if matches!(facts.pressure, Pressure::Under) {
            self.pressure_told = false;
        }
        if !facts.quiet {
            return None;
        }
        match attempt {
            Ok(deliberate::Outcome::Complete(_)) => self.on_complete(facts, log),
            Ok(deliberate::Outcome::Empty) => self.repair("empty turn".into(), EMPTY_MESSAGE, log),
            Ok(deliberate::Outcome::Stopped { reason }) => {
                self.repair(format!("stop={reason}"), EARLY_STOP_MESSAGE, log)
            }
            Err(ProviderError::Truncated { .. }) => {
                self.repair("truncated".into(), TRUNCATED_MESSAGE, log)
            }
            // A reply is final; a cancel was asked for; a step cap would only
            // buy another MAX_STEPS; every other provider error is the
            // transport's own.
            _ => None,
        }
    }

    fn repair(&mut self, cause: String, message: &str, log: &mut AgentLog) -> Option<String> {
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

    fn on_complete(&mut self, facts: &Facts, log: &mut AgentLog) -> Option<String> {
        let mut parts = Vec::new();
        if facts.must_reply {
            if self.used >= BUDGET {
                // The un-replied finish is accepted; `agent_outcome` maps it
                // to Failed.  Returning here leaves the edges below armed.
                let msg = "agent finished without calling `reply` after the nudge budget; \
                           returning a failure"
                    .to_string();
                if let Err(error) = log.record_error(msg) {
                    eprintln!("exarch: a nudge-budget failure was not recorded: {error}");
                }
                return None;
            }
            self.used += 1;
            record_nudge(log, self.used, "no-reply finish (returning agent)".into());
            parts.push(REPLY_MESSAGE.to_string());
        }
        // Edge-triggered: the edge is consumed in the same act as the part's
        // emission, so "told but not sent" and "sent but not told" are
        // inexpressible.
        match &facts.pinned {
            Some(pinned) if self.pinned_told.as_ref() != Some(pinned) => {
                self.pinned_told = Some(pinned.clone());
                record_nudge(log, self.used, "pinned-state reminder".into());
                parts.push(format!("There is pinned state: {pinned}"));
            }
            Some(_) => {}
            // An emptied register re-arms even an identical future digest.
            None => self.pinned_told = None,
        }
        if let Pressure::Over(detail) = &facts.pressure
            && !self.pressure_told
        {
            self.pressure_told = true;
            record_nudge(log, self.used, "context pressure".into());
            parts.push(format!("Context pressure: {detail}. {PRESSURE_MESSAGE}"));
        }
        (!parts.is_empty()).then(|| wrap_reminder(&parts.join(" ")))
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

// ── Messages ─────────────────────────────────────────────────────────

/// The no-reply reminder: the agent finished with prose but never called `reply`,
/// its sole return path.  Re-issued until [`BUDGET`] is spent, then the run fails.
const REPLY_MESSAGE: &str = "You ended your turn without calling `reply`, so your parent will \
    receive nothing. Return your result now with `ral { reply <value> }` — a string for a \
    markdown report, or a record/list for structured findings; if the value carries `$`, `!`, \
    or a quote, write it as a raw string `#'…'#`. This is the only way to hand your work back; \
    a final message on its own is not delivered.";

/// Shown once [`Facts::pressure`] crosses its soft line, budget-free like the
/// pinned-state reminder: durable state belongs in files, not bindings, so it
/// survives the summary that compaction is about to fold history into.
const PRESSURE_MESSAGE: &str = "Older history will be auto-compacted into a summary soon. \
    Preserve what must survive verbatim: write durable state to files and keep the paths — do \
    not park large values in bindings — and record your intent with `set-goal`/`add-task`.";

const EMPTY_MESSAGE: &str = "Your previous turn produced no text and no tool calls. \
    If you are finished, say so explicitly; otherwise continue.";

const EARLY_STOP_MESSAGE: &str = "Your previous turn ended early on a provider-side stop. \
    Reformulate the previous reply or take a different approach.";

const TRUNCATED_MESSAGE: &str = "Your previous reply was cut off before it completed. \
    Continue concisely from where it stopped.";

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

    fn facts() -> Facts {
        Facts {
            must_reply: false,
            pinned: None,
            quiet: true,
            pressure: Pressure::Under,
        }
    }

    /// The transient exhausted the provider loop before the model produced output;
    /// nudging would resend that same invisible request as a fake user turn.
    #[test]
    fn transient_surfaces_without_nudge() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let attempt = Err(ProviderError::Transient {
            cause: "stream idle: no response within timeout".into(),
            attempts: 3,
            body: None,
        });
        assert!(
            nudges.react(&attempt, &facts(), &mut log).is_none(),
            "transient provider failures are surfaced directly"
        );
        assert_eq!(nudges.used, 0);
    }

    /// The provider loop owns retry attempts; the budget here is only for
    /// model-visible recovery.
    #[test]
    fn provider_failures_do_not_consume_budget() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let attempt = || {
            Err(ProviderError::RateLimited {
                retry_after: None,
                cause: "429".into(),
                body: None,
            })
        };
        for _ in 0..=BUDGET {
            assert!(
                nudges.react(&attempt(), &facts(), &mut log).is_none(),
                "provider failure should stop, not nudge"
            );
        }
        assert_eq!(nudges.used, 0);
    }

    #[test]
    fn empty_turn_nudges_and_consumes_budget() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        match nudges.react(&Ok(deliberate::Outcome::Empty), &facts(), &mut log) {
            Some(msg) => assert!(msg.contains("no text and no tool calls")),
            None => panic!("empty turn must nudge, not stop"),
        }
        assert_eq!(nudges.used, 1);
    }

    #[test]
    fn exhausted_budget_stops() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        for _ in 0..BUDGET {
            assert!(
                nudges
                    .react(&Ok(deliberate::Outcome::Empty), &facts(), &mut log)
                    .is_some()
            );
        }
        assert!(
            nudges
                .react(&Ok(deliberate::Outcome::Empty), &facts(), &mut log)
                .is_none()
        );
    }

    /// Once the budget is spent the un-replied finish is accepted (`None`) so the
    /// loop ends; `agent_outcome` in `agent/attend.rs` maps that `Complete` to
    /// `Failed`.
    #[test]
    fn no_reply_finish_re_nudges_up_to_budget_then_fails() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let f = || Facts {
            must_reply: true,
            ..facts()
        };
        for _ in 0..BUDGET {
            match nudges.react(
                &Ok(deliberate::Outcome::Complete("prose, no reply".into())),
                &f(),
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
            nudges.used, BUDGET,
            "the no-reply nudge now draws on the budget"
        );
        assert!(
            nudges
                .react(
                    &Ok(deliberate::Outcome::Complete("still no reply".into())),
                    &f(),
                    &mut log,
                )
                .is_none(),
            "past the budget the un-replied finish is accepted (mapped to Failed downstream)"
        );
    }

    /// The interactive root converses and never returns, so it owes no `reply`.
    #[test]
    fn root_completion_is_never_reply_nudged() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        assert!(
            nudges
                .react(
                    &Ok(deliberate::Outcome::Complete("answer to the user".into())),
                    &facts(),
                    &mut log,
                )
                .is_none()
        );
    }

    /// A live child *is* the next action, so every nudge — the pin reminder
    /// included — waits for it to settle rather than pile on; and once it
    /// settles the pin reminder fires on the first quiet completion.
    #[test]
    fn pinned_state_waits_while_children_live() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let busy = Facts {
            pinned: Some("tasks 3/8".into()),
            quiet: false,
            ..facts()
        };
        assert!(
            nudges
                .react(
                    &Ok(deliberate::Outcome::Complete(
                        "waiting on a descendant".into()
                    )),
                    &busy,
                    &mut log,
                )
                .is_none(),
            "a live descendant is the next action, so the pin reminder should wait"
        );
        assert_eq!(nudges.used, 0, "waiting must not spend nudge budget");

        let quiet = Facts {
            pinned: Some("tasks 3/8".into()),
            quiet: true,
            ..facts()
        };
        let msg = nudges
            .react(
                &Ok(deliberate::Outcome::Complete("settled".into())),
                &quiet,
                &mut log,
            )
            .expect("the first quiet completion after settling should nudge");
        assert!(msg.contains("There is pinned state: tasks 3/8"));
    }

    /// Both obligations compose into one reminder, each on its own accounting:
    /// the reply half spends budget, the pinned-state half does not.
    #[test]
    fn returning_agent_gets_both_reply_and_pinned_nudges() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let f = Facts {
            must_reply: true,
            pinned: Some("tasks 3/8".into()),
            ..facts()
        };
        let msg = nudges
            .react(
                &Ok(deliberate::Outcome::Complete("prose, no reply".into())),
                &f,
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
        assert_eq!(nudges.used, 1, "only the reply half spends budget");
    }

    /// The pressure gauge composes on a clean completion, budget-free, and
    /// carries both the detail and the file-paths-over-bindings guidance.
    #[test]
    fn pressure_part_composes_budget_free_on_complete() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let f = Facts {
            pressure: Pressure::Over("400 of 500 tokens".into()),
            ..facts()
        };
        let msg = nudges
            .react(
                &Ok(deliberate::Outcome::Complete("done".into())),
                &f,
                &mut log,
            )
            .expect("pressure due should nudge");
        assert!(msg.contains("400 of 500 tokens"), "{msg}");
        assert!(
            msg.contains("write durable state to files")
                && msg.contains("do not park large values in bindings"),
            "must steer state to files, not bindings: {msg}"
        );
        assert_eq!(nudges.used, 0, "the pressure nudge is budget-free");
    }

    /// All three obligations join one message: the returning agent's `reply`,
    /// its pinned state, and its context pressure — only the first spends
    /// budget.
    #[test]
    fn pressure_composes_alongside_reply_and_pinned_parts() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let f = Facts {
            must_reply: true,
            pinned: Some("tasks 3/8".into()),
            pressure: Pressure::Over("400 of 500 tokens".into()),
            ..facts()
        };
        let msg = nudges
            .react(
                &Ok(deliberate::Outcome::Complete("prose, no reply".into())),
                &f,
                &mut log,
            )
            .expect("all three obligations should nudge");
        assert!(msg.contains("`reply`"), "{msg}");
        assert!(msg.contains("There is pinned state: tasks 3/8"), "{msg}");
        assert!(msg.contains("400 of 500 tokens"), "{msg}");
        assert_eq!(nudges.used, 1, "only the reply half spends budget");
    }

    /// `quiet` is the one gate every nudge kind shares: a live child suppresses
    /// the pressure gauge exactly as it suppresses the pin reminder.
    #[test]
    fn pressure_waits_while_children_live_too() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let f = Facts {
            pinned: Some("tasks 3/8".into()),
            quiet: false,
            pressure: Pressure::Over("400 of 500 tokens".into()),
            ..facts()
        };
        assert!(
            nudges
                .react(
                    &Ok(deliberate::Outcome::Complete(
                        "waiting on a descendant".into(),
                    )),
                    &f,
                    &mut log,
                )
                .is_none(),
            "not quiet: nothing nudges, pressure included"
        );
    }

    /// A `reply` is accepted outright, whatever else is true.
    #[test]
    fn pressure_is_not_delivered_on_replied() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let f = Facts {
            pressure: Pressure::Over("400 of 500 tokens".into()),
            ..facts()
        };
        assert!(
            nudges
                .react(
                    &Ok(deliberate::Outcome::Replied(FOValue::String {
                        value: "done".into(),
                    })),
                    &f,
                    &mut log,
                )
                .is_none(),
            "a reply is accepted outright; pressure does not apply to it"
        );
    }

    #[test]
    fn reset_clears_budget() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let _ = nudges.react(&Ok(deliberate::Outcome::Empty), &facts(), &mut log);
        assert!(nudges.used >= 1);
        nudges.reset();
        assert_eq!(nudges.used, 0, "reset clears the budget");
    }

    /// `Replied` answers `None` for peer and headless-root facts alike, budget
    /// untouched — there is no self-verification round-trip left.
    #[test]
    fn any_reply_is_accepted_outright() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        for must_reply in [false, true] {
            let f = Facts {
                must_reply,
                ..facts()
            };
            assert!(
                nudges
                    .react(
                        &Ok(deliberate::Outcome::Replied(FOValue::String {
                            value: "done".into(),
                        })),
                        &f,
                        &mut log,
                    )
                    .is_none()
            );
        }
        assert_eq!(nudges.used, 0);
    }

    /// The livelock regression: the same digest over repeated quiet
    /// completions fires exactly once; a changed digest fires again; and
    /// unpinning then re-pinning the identical digest fires again too.
    #[test]
    fn pin_reminder_fires_once_per_register_change() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let with = |digest: &str| Facts {
            pinned: Some(digest.into()),
            ..facts()
        };
        let complete = || Ok(deliberate::Outcome::Complete("done".into()));

        let first = nudges
            .react(&complete(), &with("tasks 3/8"), &mut log)
            .expect("the first sighting of a digest must nudge");
        assert!(first.contains("tasks 3/8"));
        for _ in 0..3 {
            assert!(
                nudges
                    .react(&complete(), &with("tasks 3/8"), &mut log)
                    .is_none(),
                "a stationary digest must not re-fire"
            );
        }

        let changed = nudges
            .react(&complete(), &with("tasks 4/8"), &mut log)
            .expect("a changed digest must fire again");
        assert!(changed.contains("tasks 4/8"));

        // Unpin, then re-pin the identical digest: the edge re-arms through
        // `None`.
        let none = Facts { pinned: None, ..facts() };
        assert!(nudges.react(&complete(), &none, &mut log).is_none());
        let refired = nudges
            .react(&complete(), &with("tasks 4/8"), &mut log)
            .expect("re-pinning even the identical digest after an empty register must fire");
        assert!(refired.contains("tasks 4/8"));
    }

    /// `Over` repeatedly fires once; `Under` re-arms; a second `Over` fires
    /// again.
    #[test]
    fn pressure_fires_once_per_excursion() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let over = Facts {
            pressure: Pressure::Over("400 of 500 tokens".into()),
            ..facts()
        };
        let under = Facts {
            pressure: Pressure::Under,
            ..facts()
        };
        let complete = || Ok(deliberate::Outcome::Complete("done".into()));

        assert!(nudges.react(&complete(), &over, &mut log).is_some());
        assert!(
            nudges.react(&complete(), &over, &mut log).is_none(),
            "the same excursion must not re-fire"
        );
        assert!(nudges.react(&complete(), &under, &mut log).is_none());
        assert!(
            nudges.react(&complete(), &over, &mut log).is_some(),
            "a fresh excursion after Under must fire again"
        );
    }

    /// A stale token measure (`Unknown`) neither warns nor re-arms a warning
    /// still owed.
    #[test]
    fn stale_measure_neither_warns_nor_rearms() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let over = Facts {
            pressure: Pressure::Over("400 of 500 tokens".into()),
            ..facts()
        };
        let unknown = Facts {
            pressure: Pressure::Unknown,
            ..facts()
        };
        let under = Facts {
            pressure: Pressure::Under,
            ..facts()
        };
        let complete = || Ok(deliberate::Outcome::Complete("done".into()));

        assert!(nudges.react(&complete(), &over, &mut log).is_some());
        for _ in 0..3 {
            assert!(
                nudges.react(&complete(), &unknown, &mut log).is_none(),
                "an unknown reading must not re-fire"
            );
        }
        assert!(
            nudges.react(&complete(), &unknown, &mut log).is_none(),
            "an unknown reading must not re-arm the told warning either"
        );
        assert!(nudges.react(&complete(), &under, &mut log).is_none());
        assert!(
            nudges.react(&complete(), &over, &mut log).is_some(),
            "a genuine Under reading re-arms; the next Over then fires"
        );
    }

    /// The re-arm runs ahead of the `quiet` gate: an excursion that ends and
    /// re-begins entirely while the agent is un-quiet still re-fires.
    #[test]
    fn pressure_rearms_even_while_unquiet() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let unquiet_under = Facts {
            pressure: Pressure::Under,
            quiet: false,
            ..facts()
        };
        let quiet_over = Facts {
            pressure: Pressure::Over("400 of 500 tokens".into()),
            ..facts()
        };
        let complete = || Ok(deliberate::Outcome::Complete("done".into()));

        assert!(nudges.react(&complete(), &quiet_over, &mut log).is_some());
        assert!(
            nudges
                .react(&complete(), &unquiet_under, &mut log)
                .is_none(),
            "unquiet never nudges, but the Under reading still re-arms"
        );
        assert!(
            nudges.react(&complete(), &quiet_over, &mut log).is_some(),
            "the re-arm from the unquiet Under reading carries through"
        );
    }

    /// A must-reply completion at budget with pins and pressure both due
    /// returns `None` and leaves both edges armed — the regression for the
    /// old three-owner latch bug.
    #[test]
    fn exhausted_reply_budget_leaves_edges_armed() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let must_reply = Facts {
            must_reply: true,
            ..facts()
        };
        for _ in 0..BUDGET {
            assert!(
                nudges
                    .react(
                        &Ok(deliberate::Outcome::Complete("no reply".into())),
                        &must_reply,
                        &mut log,
                    )
                    .is_some()
            );
        }
        let both_due = Facts {
            must_reply: true,
            pinned: Some("tasks 3/8".into()),
            pressure: Pressure::Over("400 of 500 tokens".into()),
            ..facts()
        };
        assert!(
            nudges
                .react(
                    &Ok(deliberate::Outcome::Complete("still no reply".into())),
                    &both_due,
                    &mut log,
                )
                .is_none(),
            "past budget the un-replied finish is accepted"
        );

        nudges.reset();
        let no_must_reply = Facts {
            pinned: Some("tasks 3/8".into()),
            pressure: Pressure::Over("400 of 500 tokens".into()),
            ..facts()
        };
        let msg = nudges
            .react(
                &Ok(deliberate::Outcome::Complete("done".into())),
                &no_must_reply,
                &mut log,
            )
            .expect("after reset both edges must still be armed");
        assert!(msg.contains("There is pinned state: tasks 3/8"), "{msg}");
        assert!(msg.contains("400 of 500 tokens"), "{msg}");
    }

    /// `reset()` clears only the budget, not the edges; `Nudges::new()` — the
    /// rebirth — clears everything.
    #[test]
    fn reset_does_not_rearm_edges() {
        let mut nudges = Nudges::new();
        let mut log = fresh_log();
        let f = Facts {
            pinned: Some("tasks 3/8".into()),
            ..facts()
        };
        let complete = || Ok(deliberate::Outcome::Complete("done".into()));
        assert!(nudges.react(&complete(), &f, &mut log).is_some());
        nudges.reset();
        assert!(
            nudges.react(&complete(), &f, &mut log).is_none(),
            "reset must not re-arm a told edge"
        );

        nudges = Nudges::new();
        assert!(
            nudges.react(&complete(), &f, &mut log).is_some(),
            "the rebirth must fire again"
        );
    }
}
