//! Standing conditions: a reading climbs a ladder of rungs, each told once
//! per excursion; a fall below a rung re-arms it.  The thresholds every
//! reading is weighed against live here too, and every measurement an agent
//! takes of itself; the words a crossing is told in are
//! [`nudge`](crate::agent::nudge)'s.

pub(crate) mod ration;

use crate::agent::Avatar;
use crate::latch::Latch;
use crate::provider::{Provider, Usage};
use crate::record::Forensic;
use ral_core::carrier::Severed;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// A climbed rung, told to the one audience its ladder names.
pub(crate) enum Warning {
    User(String),
    Model(Reminder),
}

/// A standing condition newly crossed, for the model: the figures, not the
/// sentence, which is [`nudge`](crate::agent::nudge)'s to word.
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
    /// When the disk ceiling was last walked; `None` before the first walk.
    disk_walked: Option<Instant>,
    ration: ration::Ration,
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

    /// Whether the disk ceiling is due a walk, stamping it walked if so.
    fn disk_due(&mut self) -> bool {
        if self
            .disk_walked
            .is_some_and(|at| at.elapsed() < DISK_WALK_INTERVAL)
        {
            return false;
        }
        self.disk_walked = Some(Instant::now());
        true
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

/// Between disk-ceiling walks: a full scan of the session log dir and
/// scratch, too costly to pay at every tool boundary.
const DISK_WALK_INTERVAL: Duration = Duration::from_secs(60);

/// Fallback eviction trigger, in serialised model-view bytes, for
/// [`Avatar::must_evict`] — used only when the model's context window is unknown
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
    /// Record a completion's usage; its input count, where reported, becomes
    /// the measure the gauges weigh.
    pub(super) fn measure(&mut self, usage: Usage) -> io::Result<()> {
        let at = {
            let log = self.log.borrow();
            log.record_emitter()
                .emit(Forensic::UsageDelta { usage })
                .map(drop)?;
            log.context().log_len()
        };
        if let Some(tokens) = usage.measured_input() {
            self.readings.measure = Some(Measure { tokens, at });
        }
        Ok(())
    }

    /// The provider's last input-token count, `None` until one has been
    /// reported for the context as it now stands: a count measured before an
    /// edit says nothing about what is left.
    pub(crate) fn measured_input(&self) -> Option<u64> {
        let Measure { tokens, at } = self.readings.measure?;
        (!self.log.borrow().context().token_measure_is_stale(at)).then_some(tokens)
    }

    /// Auto-eviction tracks real context pressure: the tokens the model last
    /// saw against its window, firing once they grow into the reserve.  An
    /// unknown window falls back to the byte heuristic.
    pub(super) fn must_evict(&self, provider: &Provider) -> bool {
        match provider.context_window() {
            Some(window) => self
                .measured_input()
                .is_some_and(|tokens| eviction_due(tokens, window)),
            None => self.log.borrow().context().history_bytes() >= EVICT_THRESHOLD,
        }
    }

    /// The turns an eviction would take were it to run now, `None` when
    /// nothing is old enough to shed.
    /// [`crate::record::model::Context::plan_eviction`] never names the
    /// newest turn, so this never answers with the work in hand either.
    pub(super) fn planned_eviction(&self) -> Option<Vec<u64>> {
        let log = self.log.borrow();
        let context = log.context();
        context
            .can_evict()
            .then(|| context.plan_eviction(suffix_keep_budget(context.history_bytes())))
            .flatten()
    }

    /// The engine's scratch and its size, both read where the scratch is.
    ///
    /// # Errors
    /// The engine's severance.
    pub(super) fn scratch_bytes(&self) -> Result<Option<(String, u64)>, Severed> {
        let Some(scratch) = self.seat.read(|t| t.env_var("EXARCH_SCRATCH"))? else {
            return Ok(None);
        };
        let bytes = self.seat.read(|t| t.path_bytes(Path::new(&scratch)))?;
        Ok(Some((scratch, bytes)))
    }

    /// Every standing condition newly climbed, for the tool boundary to tell.
    ///
    /// # Errors
    /// The engine's severance, from the disk probe.
    pub(super) fn climbed(&mut self, provider: &Provider) -> Result<Vec<Warning>, Severed> {
        let pressure = self.pressure(provider);
        let mut climbed: Vec<Warning> = self
            .readings
            .gauges
            .pressure(pressure)
            .into_iter()
            .collect();
        climbed.extend(self.disk_warning()?);
        let account = provider.account();
        climbed.extend(ration::told(
            account,
            &provider.climb_allowances(ration::USER_HEARS),
        ));
        let resumes = self.resumes_at_reset();
        climbed.extend(
            self.readings
                .gauges
                .ration
                .climb(account, &provider.allowances(), resumes),
        );
        Ok(climbed)
    }

    /// One reading of the pressure gauge, no state.  The soft line is
    /// [`pressure_due`], one reserve ahead of auto-eviction; an unknown window
    /// falls back to the byte heuristic against
    /// [`PRESSURE_THRESHOLD_FALLBACK`].  Either way the reading carries the
    /// cut [`Avatar::planned_eviction`] would make, so the reminder can name it.
    fn pressure(&self, provider: &Provider) -> Pressure {
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

    /// The disk-warn ceiling's verdict, once per excursion — nothing is ever
    /// rotated or deleted.  Unconfigured it walks nothing at all; otherwise
    /// it walks at most once per [`DISK_WALK_INTERVAL`].
    ///
    /// # Errors
    /// The engine's severance, from the `EXARCH_SCRATCH` probe.
    fn disk_warning(&mut self) -> Result<Option<Warning>, Severed> {
        let Some(ceiling) = self.fleet.launch.disk_warn_bytes else {
            return Ok(None);
        };
        if !self.readings.gauges.disk_due() {
            return Ok(None);
        }
        let mut total = ral_core::path::tree::bytes(self.log.borrow().dir());
        if let Some((_, bytes)) = self.scratch_bytes()? {
            total += bytes;
        }
        Ok(self.readings.gauges.disk(total, ceiling))
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests;
