//! Standing conditions: a reading climbs a ladder of rungs, each told once
//! per excursion; a fall below a rung re-arms it.  The thresholds every
//! reading is weighed against live here too; the words a crossing is told in
//! are [`nudge`](crate::agent::nudge)'s.

pub(crate) mod ration;

use crate::agent::Avatar;
use crate::agent::nudge::Reminder;
use crate::latch::Latch;
use crate::provider::Provider;

/// A climbed rung, told to the one audience its ladder names.
pub(crate) enum Warning {
    User(String),
    Model(Reminder),
}

/// Input tokens as the provider last counted them, and the log position the
/// count stood at: stale once the context is edited past it.
#[derive(Clone, Copy)]
pub(crate) struct Measure {
    pub tokens: u64,
    pub at: usize,
}

/// One stateless reading of the context-pressure gauge, taken by the caller.
/// `Unknown` (a stale token measure under a known window) must neither warn
/// nor re-arm: a stale measure relieves nothing.
pub(crate) enum Pressure {
    Over {
        /// The rendered detail, e.g. "173000 of 200000 tokens".
        detail: String,
        /// The turns the next boundary's cut would take, `None` when nothing
        /// is old enough to shed.
        planned: Option<Vec<u64>>,
    },
    Under,
    Unknown,
}

/// Every ladder an agent climbs, at its latch.
#[derive(Default)]
pub(crate) struct Gauges {
    pressure: Latch,
    disk: Latch,
    pub(crate) ration: ration::Ration,
}

impl Gauges {
    pub(crate) fn pressure(&mut self, reading: Pressure) -> Option<Warning> {
        match reading {
            Pressure::Unknown => None,
            Pressure::Under => {
                self.pressure.cross(false);
                None
            }
            Pressure::Over { detail, planned } => self
                .pressure
                .cross(true)
                .then_some(Warning::Model(Reminder::Pressure { detail, planned })),
        }
    }

    pub(crate) fn disk(&mut self, total: u64, ceiling: u64) -> Option<Warning> {
        self.disk.cross(total > ceiling).then(|| {
            Warning::User(format!(
                "disk: session log + scratch is {} KiB, over the {} KiB warn ceiling \
                 — forensic records are never rotated or deleted automatically; \
                 clean up by hand",
                total / 1024,
                ceiling / 1024
            ))
        })
    }
}

// ── Thresholds ───────────────────────────────────────────────────────

/// Fallback eviction trigger, in serialised model-view bytes, for
/// `Avatar::evict` — used only when the model's context window is unknown
/// (no source reports it).  A known window goes
/// through [`eviction_due`] instead.
pub(crate) const EVICT_THRESHOLD: usize = 500 * 1024;

/// Fallback soft line when the window is unknown: three-quarters of
/// [`EVICT_THRESHOLD`].
pub(crate) const PRESSURE_THRESHOLD_FALLBACK: usize = EVICT_THRESHOLD / 4 * 3;

/// Tokens held back from the window for the next prompt: 15% of the window,
/// with a floor so a small window still keeps a usable margin.
fn reserve_tokens(window: u64) -> u64 {
    const RESERVE_FLOOR_TOKENS: u64 = 16_384;
    (window * 15 / 100).max(RESERVE_FLOOR_TOKENS)
}

/// Whether the live context (`used` input tokens) has grown into the
/// reserve — i.e. crossed `window − reserve`.
pub(crate) fn eviction_due(used: u64, window: u64) -> bool {
    used + reserve_tokens(window) > window
}

/// The token count past which [`eviction_due`] fires, spelled as a gauge
/// cap for `/resources`.
pub(crate) fn eviction_trigger(window: u64) -> u64 {
    window.saturating_sub(reserve_tokens(window))
}

/// The soft line one full reserve ahead of [`eviction_due`]: room enough for
/// the model to leave itself a note before the older half of its context
/// goes.
pub(crate) fn pressure_due(used: u64, window: u64) -> bool {
    used + 2 * reserve_tokens(window) > window
}

/// Byte budget for the verbatim suffix kept across an eviction: half the
/// model-view bytes, the older half being what leaves the window.
/// Window-agnostic — it splits whatever is in context, which the trigger
/// bounds.
pub(crate) fn suffix_keep_budget(history_bytes: usize) -> usize {
    history_bytes / 2
}

impl Avatar {
    /// The provider's last input-token count, `None` until one has been
    /// reported for the context as it now stands: a count measured before an
    /// edit says nothing about what is left.
    pub(crate) fn measured_input(&self) -> Option<u64> {
        let Measure { tokens, at } = self.readings.measure?;
        (!self.log.borrow().context().token_measure_is_stale(at)).then_some(tokens)
    }

    /// One reading of the pressure gauge, no state — taken at a tool boundary
    /// by [`Avatar::deliberate`], where the measure is this turn's own.  The
    /// soft line is [`pressure_due`], one reserve ahead of auto-eviction; an
    /// unknown window falls back to the byte heuristic against
    /// [`PRESSURE_THRESHOLD_FALLBACK`].  Either way the reading carries the
    /// cut [`Avatar::planned_eviction`] would make, so the reminder can name it.
    pub(super) fn pressure_gauge(&self, provider: &Provider) -> Pressure {
        let detail = match provider.context_window() {
            Some(window) if window > 0 => match self.measured_input() {
                None => return Pressure::Unknown,
                Some(tokens) => {
                    pressure_due(tokens, window).then(|| format!("{tokens} of {window} tokens"))
                }
            },
            _ => {
                let bytes = self.log.borrow().context().history_bytes();
                (bytes >= PRESSURE_THRESHOLD_FALLBACK).then(|| format!("{} KB", bytes / 1024))
            }
        };
        match detail {
            Some(detail) => Pressure::Over {
                detail,
                planned: self.planned_eviction(),
            },
            None => Pressure::Under,
        }
    }
}

#[cfg(test)]
mod tests;
