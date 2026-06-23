//! Single home for the inter-attempt nudge facility.
//!
//! Owns the rule set, the budget counter, and the post-attempt decision:
//! walk the rules and either accept the turn (surfacing any attached provider
//! error on the way out) or hand back a synthetic next prompt for the drive
//! loop to post to itself as an [`InboxMsg::Nudge`](crate::bus::InboxMsg).
//! Records to events.json; emits to the bus only for the budget-exhausted and
//! provider-error breadcrumbs.  Nudges themselves are quiet.
//!
//! The registry is **per-session** ([`Session::nudges`]): the drive loop runs
//! one [`Session::apply`] per inbox message, not one whole turn, so the
//! one-shot latches must outlive a single `apply`.  They reset on a genuine
//! turn-boundary message via [`Registry::reset`], never on a self-nudge.
//!
//! [`Session::apply`]: crate::session::Session::apply
//! [`Session::nudges`]: crate::session::Session

use crate::bus::{Emitter, Kind};
use crate::event::SessionLog;
use crate::provider::ProviderError;
use crate::session::TurnOutcome;

/// Outer-attempt budget per user turn.  Independent of, and stacked
/// on top of, the provider's inner retry budget.
const BUDGET: u32 = 3;

/// How many genuine turns elapse between pinned-state reminders.  A gentle
/// cadence: often enough that a stale gauge is caught, rare enough not to nag.
const REMIND_EVERY: u32 = 12;

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
/// the current turn.  Threaded in by [`Session::drive`].
///
/// [`Session::drive`]: crate::session::Session::drive
pub(crate) struct NudgeCtx {
    pub expect_action: bool,
    pub acted: bool,
    /// Whether this session returns through `reply` — true for a sub-agent,
    /// false for the root.  A sub-agent that finishes a tool-call-free step
    /// without having replied earns one reminder to call `reply`.
    pub must_reply: bool,
    /// The model's current pinned state as a one-line description, or `None`
    /// when nothing is pinned.  Drives the periodic pinned-state reminder so a
    /// long-lived rail gauge does not drift out of the model's attention.
    pub pinned: Option<String>,
}

/// Per-session nudge state.  Lives on the [`Session`](crate::session::Session)
/// and is reset at each genuine turn boundary by [`Self::reset`];
/// [`Self::react`] is the only post-attempt entry point.
pub(crate) struct Registry {
    used: u32,
    /// One-shot latch for the idle-completion nudge: it fires at most
    /// once per turn and never draws on the [`BUDGET`] counter.
    idle_nudged: bool,
    /// One-shot latch for the verify-before-finish nudge: same contract —
    /// at most once per turn, never drawing on the [`BUDGET`] counter.
    verify_nudged: bool,
    /// One-shot latch for the no-reply reminder a sub-agent earns when it
    /// finishes a tool-call-free step without calling `reply`: same contract —
    /// at most once per turn, budget-free.
    reply_nudged: bool,
    /// Genuine turns since the last pinned-state reminder, bumped once per
    /// turn-boundary message by [`Self::reset`].  Unlike the latches it
    /// accumulates *across* turns; the reminder fires once it reaches
    /// [`REMIND_EVERY`], then it returns to zero.
    turns_since_pin_reminder: u32,
    /// One-shot latch for the pinned-state reminder: at most once per turn,
    /// budget-free, like the completion gates.
    pin_reminded: bool,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            used: 0,
            idle_nudged: false,
            verify_nudged: false,
            reply_nudged: false,
            turns_since_pin_reminder: 0,
            pin_reminded: false,
        }
    }

    /// Reset the per-turn state — the retry budget and the one-shot latches —
    /// and count this turn toward the periodic pinned-state reminder.  Called
    /// by [`Session::drive`] on a genuine turn-boundary message, never on a
    /// self-nudge (which is the same turn continuing), so the turn counter
    /// advances once per real turn.
    ///
    /// [`Session::drive`]: crate::session::Session::drive
    pub fn reset(&mut self) {
        self.used = 0;
        self.idle_nudged = false;
        self.verify_nudged = false;
        self.reply_nudged = false;
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
        log: &mut SessionLog,
    ) -> Option<String> {
        let Some((cause, message)) = RULES.iter().find_map(|&r| r(attempt)) else {
            // A sub-agent reaches a tool-call-free `Complete` without having
            // called `reply`: its parent would otherwise receive nothing (a
            // `Complete` is no longer scraped).  Remind it once to return
            // through `reply`.  A `Replied` outcome never lands here, so a
            // child that already replied is not re-nudged.
            if ctx.must_reply
                && !self.reply_nudged
                && matches!(attempt, Ok(TurnOutcome::Complete(_)))
            {
                self.reply_nudged = true;
                let _ = log.record_nudge(self.used, BUDGET, "no-reply finish (sub-agent)".into());
                return Some(REPLY_MESSAGE.into());
            }
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
                    return Some(IDLE_MESSAGE.into());
                }
                if ctx.acted && !self.verify_nudged {
                    self.verify_nudged = true;
                    let _ = log.record_nudge(
                        self.used,
                        BUDGET,
                        "verify-before-finish (expect-action)".into(),
                    );
                    return Some(VERIFY_MESSAGE.into());
                }
            }
            // A periodic, budget-free reminder of the model's pinned state —
            // at most once per turn, only on a clean completion, only when
            // something is pinned, and only every `REMIND_EVERY` turns — so a
            // long-lived rail gauge stays in the model's attention without the
            // kit having to nag for it.
            if let Some(desc) = &ctx.pinned
                && matches!(attempt, Ok(TurnOutcome::Complete(_)))
                && !self.pin_reminded
                && self.turns_since_pin_reminder >= REMIND_EVERY
            {
                self.pin_reminded = true;
                self.turns_since_pin_reminder = 0;
                let _ = log.record_nudge(self.used, BUDGET, "pinned-state reminder".into());
                return Some(format!("{PIN_REMINDER_PREFIX}{desc}{PIN_REMINDER_SUFFIX}"));
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
        // Recorded in events.json for forensics; not surfaced on the
        // bus — nudges are an internal driver concern.
        let _ = log.record_nudge(self.used, BUDGET, cause);
        Some(message.into())
    }
}

fn surface_provider_error(
    attempt: &Result<TurnOutcome, ProviderError>,
    emit: &Emitter,
    log: &mut SessionLog,
) {
    if let Err(e) = attempt {
        let _ = log.record_provider_error(e);
        emit.emit(Kind::ProviderError(e.into()));
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

/// The one-shot no-reply reminder (gated by [`NudgeCtx::must_reply`], so it
/// fires only for a sub-agent).  A child finished with prose but never called
/// `reply`, so as things stand its parent receives nothing; this asks it to
/// return through `reply` before the run ends.
const REPLY_MESSAGE: &str = "You ended your turn without calling `reply`, so your parent will \
    receive nothing. Return your result now by calling `reply` — pass a markdown report as \
    `result`, or a JSON object/array for structured findings. This is the only way to hand \
    your work back; a final message on its own is not delivered.";

/// The periodic pinned-state reminder, wrapped around the current pinned
/// description.  Quiet and budget-free; kit-agnostic (it names no kit), so any
/// kit that pins state earns it.
const PIN_REMINDER_PREFIX: &str =
    "Reminder — you have state pinned to the rail that the user is watching: ";
const PIN_REMINDER_SUFFIX: &str =
    ". Keep it current as your work moves on; if it is already accurate, just continue.";

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

    fn fresh_log(tag: &str) -> SessionLog {
        let root = std::env::temp_dir().join(format!("exarch-nudge-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        SessionLog::root(&root, 0, "test", "test", 0).expect("session log")
    }

    /// A surfaced rate-limit is a transport condition the provider loop
    /// already retried with backoff.  It must end the turn (`None`), not draw
    /// on the semantic nudge budget and re-issue the request.
    #[test]
    fn rate_limit_ends_turn_without_consuming_budget() {
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
                    expect_action: false,
                    acted: false,
                    must_reply: false,
                    pinned: None,
                },
                &emit(),
                &mut log,
            )
            .is_none()
        );
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
        assert!(
            reg.react(
                &attempt,
                NudgeCtx {
                    expect_action: false,
                    acted: false,
                    must_reply: false,
                    pinned: None,
                },
                &emit(),
                &mut log,
            )
            .is_none()
        );
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
                expect_action: false,
                acted: false,
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
                        expect_action: false,
                        acted: false,
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
                    expect_action: false,
                    acted: false,
                    must_reply: false,
                    pinned: None,
                },
                &emit(),
                &mut log,
            )
            .is_none()
        );
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
            must_reply: false,
            pinned: None,
        };
        match reg.react(
            &Ok(TurnOutcome::Complete("essay".into())),
            ctx(),
            &emit(),
            &mut log,
        ) {
            Some(msg) => assert!(msg.contains("no tools")),
            None => panic!("idle completion under expect-action must nudge"),
        }
        assert!(
            reg.react(
                &Ok(TurnOutcome::Complete("essay".into())),
                ctx(),
                &emit(),
                &mut log,
            )
            .is_none()
        );
        assert_eq!(reg.used, 0, "idle nudge must not draw on the budget");
    }

    /// With `--expect-action` off, a clean `Complete` ends the turn as
    /// before — the idle path is inert.
    #[test]
    fn completion_without_expect_action_stops() {
        let mut reg = Registry::new();
        let mut log = fresh_log("noexpect");
        assert!(
            reg.react(
                &Ok(TurnOutcome::Complete("done".into())),
                NudgeCtx {
                    expect_action: false,
                    acted: false,
                    must_reply: false,
                    pinned: None,
                },
                &emit(),
                &mut log,
            )
            .is_none()
        );
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
            must_reply: false,
            pinned: None,
        };
        match reg.react(
            &Ok(TurnOutcome::Complete("done".into())),
            ctx(),
            &emit(),
            &mut log,
        ) {
            Some(msg) => assert!(msg.contains("Before finishing")),
            None => panic!("an acted completion under expect-action must nudge to verify"),
        }
        assert!(
            reg.react(
                &Ok(TurnOutcome::Complete("done".into())),
                ctx(),
                &emit(),
                &mut log,
            )
            .is_none()
        );
        assert_eq!(reg.used, 0, "verify nudge must not draw on the budget");
    }

    /// A sub-agent (`must_reply`) that finishes a tool-call-free `Complete`
    /// without replying earns one reminder to call `reply`; a second identical
    /// attempt is accepted (the one-shot latch is spent) so the run can end,
    /// and the reminder never draws on the budget.
    #[test]
    fn no_reply_finish_nudges_once_then_accepts() {
        let mut reg = Registry::new();
        let mut log = fresh_log("no-reply");
        let ctx = || NudgeCtx {
            expect_action: false,
            acted: true,
            must_reply: true,
            pinned: None,
        };
        match reg.react(
            &Ok(TurnOutcome::Complete("prose, no reply".into())),
            ctx(),
            &emit(),
            &mut log,
        ) {
            Some(msg) => assert!(msg.contains("`reply`")),
            None => panic!("a sub-agent that did not reply must be nudged"),
        }
        assert!(
            reg.react(
                &Ok(TurnOutcome::Complete("still no reply".into())),
                ctx(),
                &emit(),
                &mut log,
            )
            .is_none(),
            "the second un-replied finish is accepted so the run can end"
        );
        assert_eq!(reg.used, 0, "the reply reminder must not draw on the budget");
    }

    /// The root (`must_reply` false) never gets the reply reminder — a clean
    /// `Complete` ends its turn at once.
    #[test]
    fn root_completion_is_never_reply_nudged() {
        let mut reg = Registry::new();
        let mut log = fresh_log("root-complete");
        assert!(
            reg.react(
                &Ok(TurnOutcome::Complete("answer to the user".into())),
                NudgeCtx {
                    expect_action: false,
                    acted: true,
                    must_reply: false,
                    pinned: None,
                },
                &emit(),
                &mut log,
            )
            .is_none()
        );
    }

    /// [`Registry::reset`] clears the per-turn budget and both one-shot
    /// latches, so a fresh turn-boundary message starts with a full budget
    /// and re-armed completion gates.
    #[test]
    fn reset_clears_budget_and_latches() {
        let mut reg = Registry::new();
        let mut log = fresh_log("reset");
        let ctx = || NudgeCtx {
            expect_action: true,
            acted: false,
            must_reply: false,
            pinned: None,
        };
        // Spend the idle latch and a unit of budget.
        let _ = reg.react(&Ok(TurnOutcome::Empty), ctx(), &emit(), &mut log);
        let _ = reg.react(&Ok(TurnOutcome::Complete("x".into())), ctx(), &emit(), &mut log);
        assert!(reg.used >= 1 || reg.idle_nudged);
        reg.reset();
        assert_eq!(reg.used, 0, "reset clears the budget");
        assert!(!reg.idle_nudged && !reg.verify_nudged, "reset re-arms the gates");
    }

    /// With state pinned, the periodic reminder fires once on the twelfth
    /// turn's clean completion — not before — and names the pinned state,
    /// free of the retry budget.
    #[test]
    fn pinned_state_reminder_fires_every_twelfth_turn() {
        let mut reg = Registry::new();
        let mut log = fresh_log("pin-reminder");
        let ctx = || NudgeCtx {
            expect_action: false,
            acted: false,
            must_reply: false,
            pinned: Some("tasks 3/8".into()),
        };
        // The first eleven turns count toward the reminder but do not fire it.
        for _ in 1..REMIND_EVERY {
            reg.reset();
            assert!(
                reg.react(&Ok(TurnOutcome::Complete("x".into())), ctx(), &emit(), &mut log)
                    .is_none(),
                "no reminder before the twelfth turn"
            );
        }
        // The twelfth turn fires it, naming the pinned state.
        reg.reset();
        match reg.react(&Ok(TurnOutcome::Complete("x".into())), ctx(), &emit(), &mut log) {
            Some(msg) => assert!(msg.contains("tasks 3/8"), "the reminder names the pinned state"),
            None => panic!("the twelfth turn must remind"),
        }
        assert_eq!(reg.used, 0, "the pin reminder must not draw on the budget");
    }

    /// With nothing pinned, the reminder never fires however many turns pass.
    #[test]
    fn no_reminder_without_pinned_state() {
        let mut reg = Registry::new();
        let mut log = fresh_log("no-pin-reminder");
        for _ in 0..(REMIND_EVERY + 3) {
            reg.reset();
            assert!(
                reg.react(
                    &Ok(TurnOutcome::Complete("x".into())),
                    NudgeCtx {
                        expect_action: false,
                        acted: false,
                        must_reply: false,
                        pinned: None,
                    },
                    &emit(),
                    &mut log,
                )
                .is_none()
            );
        }
    }
}
