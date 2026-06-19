//! Single home for the inter-attempt nudge facility.
//!
//! Owns the rule set, the budget counter, and the post-attempt
//! decision: walk the rules, and either stop the turn (surfacing any
//! attached provider error on the way out) or loop back into
//! [`Session::apply`] with a possibly-synthetic next prompt.  Records
//! to events.json; emits to the bus only for the budget-exhausted and
//! provider-error breadcrumbs.  Nudges themselves are quiet.
//!
//! [`Session::apply`]: crate::session::Session::apply

use crate::bus::{Event, Kind, SessionId, Sink};
use crate::event::SessionLog;
use crate::provider::ProviderError;
use crate::session::TurnOutcome;

/// Outer-attempt budget per user turn.  Independent of, and stacked
/// on top of, the provider's inner retry budget.
const BUDGET: u32 = 3;

/// A firing rule: inspect the turn outcome and, when it matches, return
/// the cause (for the forensic log) and the synthetic user message to
/// continue with.  Walked in order; the first `Some` wins.
type Rule = fn(&Result<TurnOutcome, ProviderError>) -> Option<(String, &'static str)>;

/// The rule set the binary ships with.  Every rule reacts to a
/// model-behaviour outcome shape from [`Session::apply`] by continuing
/// with a synthetic nudge.  Transport failures (transient, rate-limit)
/// are deliberately absent: they are retried with backoff inside the
/// provider loop, and once that budget is spent they surface and end
/// the turn rather than re-issuing the request without backoff.
///
/// [`Session::apply`]: crate::session::Session::apply
const RULES: &[Rule] = &[on_empty_turn, on_early_stop, on_truncated];

/// Per-attempt expectations that depend on the *driving* session, not
/// the attempt outcome: whether this run wants the model to act (set by
/// `--expect-action`, root + headless only) and whether it has acted in
/// the current turn.  Threaded in by [`Session::run_turn`].
///
/// [`Session::run_turn`]: crate::session::Session::run_turn
pub(crate) struct NudgeCtx {
    pub expect_action: bool,
    pub acted: bool,
}

/// Per-turn nudge state.  Constructed fresh by [`Session::run_turn`]
/// at the top of each user turn; [`Self::react`] is the only public
/// entry point.
///
/// [`Session::run_turn`]: crate::session::Session::run_turn
pub(crate) struct Registry {
    used: u32,
    /// One-shot latch for the idle-completion nudge: it fires at most
    /// once per turn and never draws on the [`BUDGET`] counter.
    idle_nudged: bool,
    /// One-shot latch for the verify-before-finish nudge: same contract —
    /// at most once per turn, never drawing on the [`BUDGET`] counter.
    verify_nudged: bool,
}

/// What the driver should do after one attempt.
pub(crate) enum Step {
    /// Turn is over — clean completion, fatal error, or budget
    /// exhausted.  Any user-facing breadcrumbs have already been
    /// emitted by [`Registry::react`].
    Stop,
    /// Loop back into `apply` appending this synthetic user message
    /// first.
    Continue(String),
}

impl Registry {
    pub fn new() -> Self {
        Self {
            used: 0,
            idle_nudged: false,
            verify_nudged: false,
        }
    }

    /// The one decider.  Walks [`RULES`] against `attempt`, optionally
    /// records the nudge to `log`, emits any user-facing error
    /// breadcrumbs through `sink`, and tells the caller whether to
    /// stop or to loop with a (possibly synthetic) next prompt.
    pub fn react<S: Sink>(
        &mut self,
        attempt: &Result<TurnOutcome, ProviderError>,
        ctx: NudgeCtx,
        sink: &mut S,
        log: &mut SessionLog,
        id: SessionId,
    ) -> Step {
        let Some((cause, message)) = RULES.iter().find_map(|&r| r(attempt)) else {
            // Under `--expect-action`, a clean completion is gated by two
            // one-shot, budget-free nudges before it is accepted: a turn
            // that never used a tool earns the idle nudge to engage, and a
            // turn that did earns the verify nudge to check its output
            // against the task before claiming done.
            if ctx.expect_action && matches!(attempt, Ok(TurnOutcome::Complete(_))) {
                if !ctx.acted && !self.idle_nudged {
                    self.idle_nudged = true;
                    let _ = log.record_nudge(
                        self.used,
                        BUDGET,
                        "idle-completion (expect-action)".into(),
                    );
                    return Step::Continue(IDLE_MESSAGE.into());
                }
                if ctx.acted && !self.verify_nudged {
                    self.verify_nudged = true;
                    let _ = log.record_nudge(
                        self.used,
                        BUDGET,
                        "verify-before-finish (expect-action)".into(),
                    );
                    return Step::Continue(VERIFY_MESSAGE.into());
                }
            }
            surface_provider_error(attempt, sink, log, id);
            return Step::Stop;
        };
        if self.used >= BUDGET {
            let msg = format!("nudge budget exhausted ({BUDGET} attempts; last cause: {cause})");
            let _ = log.record_error(msg.clone());
            sink.handle(Event {
                id,
                kind: Kind::Error(msg),
            });
            surface_provider_error(attempt, sink, log, id);
            return Step::Stop;
        }
        self.used += 1;
        // Recorded in events.json for forensics; not surfaced on the
        // bus — nudges are an internal driver concern.
        let _ = log.record_nudge(self.used, BUDGET, cause);
        Step::Continue(message.into())
    }
}

fn surface_provider_error<S: Sink>(
    attempt: &Result<TurnOutcome, ProviderError>,
    sink: &mut S,
    log: &mut SessionLog,
    id: SessionId,
) {
    if let Err(e) = attempt {
        let _ = log.record_provider_error(e);
        sink.handle(Event {
            id,
            kind: Kind::ProviderError(e.into()),
        });
    }
}

// ── Rules ────────────────────────────────────────────────────────────

/// The one-shot idle-completion nudge (gated by `--expect-action`).  Not
/// a [`Rule`]: it fires on a clean `Complete` only when the run expects
/// action and none was taken, so it lives outside the always-on rule set.
const IDLE_MESSAGE: &str = "You produced a final answer but used no tools this turn — \
    you have not inspected the workspace or made any changes. If the task requires \
    modifying files, start now (explore and edit with the ral tool). If you are \
    certain no change is needed, restate your conclusion and it will be accepted.";

/// The one-shot verify-before-finish nudge (gated by `--expect-action`).
/// The complement of [`IDLE_MESSAGE`]: it fires on a clean `Complete` when
/// the run expects action and the turn *did* act, turning a plausible-but-
/// unchecked result into one verification pass before completion. A clean
/// exit is not a correct answer, so the model is asked to re-read its
/// output against the task's stated requirements.
const VERIFY_MESSAGE: &str = "Before finishing: re-read the output you produced and confirm it \
    satisfies every requirement the task stated — the named tool or library, the output path and \
    format, and the success criteria. A clean run is not a correct answer: an exit code of 0 means \
    the command ran, not that the result is right, so check the result against what was asked. If \
    you have already verified it against the task, restate your conclusion and it will be accepted.";

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
            "Your previous reply was truncated by the output cap. \
             Continue concisely from where it stopped.",
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Nudges record to the log and emit only error breadcrumbs; the
    /// tests assert on the returned [`Step`] and the budget counter, so
    /// a sink that drops every event is all they need.
    struct NullSink;
    impl Sink for NullSink {
        fn handle(&mut self, _e: Event) {}
    }

    fn fresh_log(tag: &str) -> SessionLog {
        let root = std::env::temp_dir().join(format!("exarch-nudge-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        SessionLog::root(&root, 0, "test", "test", 0).expect("session log")
    }

    /// A surfaced rate-limit is a transport condition the provider loop
    /// already retried with backoff.  It must end the turn, not draw on
    /// the semantic nudge budget and re-issue the request.
    #[test]
    fn rate_limit_ends_turn_without_consuming_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log("ratelimit");
        let attempt = Err(ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(5)),
            cause: "429".into(),
            body: None,
        });
        assert!(matches!(
            reg.react(
                &attempt,
                NudgeCtx {
                    expect_action: false,
                    acted: false
                },
                &mut NullSink,
                &mut log,
                0
            ),
            Step::Stop
        ));
        assert_eq!(reg.used, 0, "transport failure must not spend nudge budget");
    }

    /// Same contract for a generic transient failure.
    #[test]
    fn transient_ends_turn_without_consuming_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log("transient");
        let attempt = Err(ProviderError::Transient {
            cause: "web stream error".into(),
            attempts: 3,
            body: None,
        });
        assert!(matches!(
            reg.react(
                &attempt,
                NudgeCtx {
                    expect_action: false,
                    acted: false
                },
                &mut NullSink,
                &mut log,
                0
            ),
            Step::Stop
        ));
        assert_eq!(reg.used, 0);
    }

    /// A model-behaviour outcome still nudges and spends one unit of
    /// budget.
    #[test]
    fn empty_turn_nudges_and_consumes_budget() {
        let mut reg = Registry::new();
        let mut log = fresh_log("empty");
        match reg.react(
            &Ok(TurnOutcome::Empty),
            NudgeCtx {
                expect_action: false,
                acted: false,
            },
            &mut NullSink,
            &mut log,
            0,
        ) {
            Step::Continue(msg) => assert!(msg.contains("no text and no tool calls")),
            Step::Stop => panic!("empty turn must nudge, not stop"),
        }
        assert_eq!(reg.used, 1);
    }

    /// Past the budget, even a nudgeable outcome stops the turn.
    #[test]
    fn exhausted_budget_stops() {
        let mut reg = Registry::new();
        let mut log = fresh_log("budget");
        for _ in 0..BUDGET {
            assert!(matches!(
                reg.react(
                    &Ok(TurnOutcome::Empty),
                    NudgeCtx {
                        expect_action: false,
                        acted: false
                    },
                    &mut NullSink,
                    &mut log,
                    0
                ),
                Step::Continue(_)
            ));
        }
        assert!(matches!(
            reg.react(
                &Ok(TurnOutcome::Empty),
                NudgeCtx {
                    expect_action: false,
                    acted: false
                },
                &mut NullSink,
                &mut log,
                0
            ),
            Step::Stop
        ));
    }

    /// Under `--expect-action`, a tool-less `Complete` earns one
    /// self-confirming nudge; a second identical attempt is accepted
    /// (the one-shot latch is spent) and ends the turn.
    #[test]
    fn idle_completion_nudges_once_then_accepts() {
        let mut reg = Registry::new();
        let mut log = fresh_log("idle");
        let ctx = || NudgeCtx {
            expect_action: true,
            acted: false,
        };
        match reg.react(
            &Ok(TurnOutcome::Complete("essay".into())),
            ctx(),
            &mut NullSink,
            &mut log,
            0,
        ) {
            Step::Continue(msg) => assert!(msg.contains("no tools")),
            Step::Stop => panic!("idle completion under expect-action must nudge"),
        }
        assert!(matches!(
            reg.react(
                &Ok(TurnOutcome::Complete("essay".into())),
                ctx(),
                &mut NullSink,
                &mut log,
                0
            ),
            Step::Stop
        ));
        assert_eq!(reg.used, 0, "idle nudge must not draw on the budget");
    }

    /// With `--expect-action` off, a clean `Complete` ends the turn as
    /// before — the idle path is inert.
    #[test]
    fn completion_without_expect_action_stops() {
        let mut reg = Registry::new();
        let mut log = fresh_log("noexpect");
        assert!(matches!(
            reg.react(
                &Ok(TurnOutcome::Complete("done".into())),
                NudgeCtx {
                    expect_action: false,
                    acted: false
                },
                &mut NullSink,
                &mut log,
                0
            ),
            Step::Stop
        ));
    }

    /// Under `--expect-action`, a turn that used a tool earns one
    /// verify-before-finish nudge on completion; a second identical attempt
    /// is accepted (the one-shot latch is spent) and ends the turn.
    #[test]
    fn verify_nudges_once_then_accepts() {
        let mut reg = Registry::new();
        let mut log = fresh_log("verify");
        let ctx = || NudgeCtx {
            expect_action: true,
            acted: true,
        };
        match reg.react(
            &Ok(TurnOutcome::Complete("done".into())),
            ctx(),
            &mut NullSink,
            &mut log,
            0,
        ) {
            Step::Continue(msg) => assert!(msg.contains("Before finishing")),
            Step::Stop => panic!("an acted completion under expect-action must nudge to verify"),
        }
        assert!(matches!(
            reg.react(
                &Ok(TurnOutcome::Complete("done".into())),
                ctx(),
                &mut NullSink,
                &mut log,
                0
            ),
            Step::Stop
        ));
        assert_eq!(reg.used, 0, "verify nudge must not draw on the budget");
    }
}
