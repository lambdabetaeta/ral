//! The `/resources` probe fold: one [`ProbeRow`] per session-lived
//! accumulator — name, size, cap, pressure policy — rendered as one card.
//!
//! The fold has two halves, split by who may legally read what: the agent
//! assembles its own rows on its attend thread ([`Avatar::resource_rows`]),
//! and the TUI appends the rows for the accumulators *it* owns
//! (`tui::resources`).  Neither half reaches across a thread for the other's
//! figures.  Probing mutates nothing and renews no lease, so `/resources`
//! cannot immortalise the zombies it exists to reveal.

use crate::agent::Avatar;
use crate::agent::fleet::AGENT_LEASE_IDLE;
use crate::agent::gauge::{EVICT_THRESHOLD, eviction_trigger};
use crate::agent::seat::EngineLost;
use crate::card::{Card, Field, FieldVal, Mark, Role, Span};
use crate::shell_eval;
use ral_core::carrier::Severed;
use std::fmt;
use std::path::Path;

/// How an accumulator is kept bounded — or that it is not.  Stated even where
/// the enforcement lands later, the row then carrying `cap: None` and a note
/// saying so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Policy {
    Coalesce,
    Queue,
    Reject,
    Evict,
    Reap,
    Warn,
    Unbounded,
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Coalesce => "coalesce",
            Self::Queue => "queue",
            Self::Reject => "reject",
            Self::Evict => "evict",
            Self::Reap => "reap",
            Self::Warn => "warn",
            Self::Unbounded => "none (unbounded)",
        })
    }
}

/// One probed accumulator, one row per figure: what the fold renders.  A probe
/// fold is an interactive diagnostic, read when it is run; no session keeps a
/// pressure history.
#[derive(Clone, Debug)]
pub(crate) struct ProbeRow {
    pub name: String,
    /// Its size now, in the unit the name implies (a count, bytes, seconds).
    pub current: u64,
    /// The enforced bound, when one is armed; `None` for a decided-but-
    /// unenforced cap and for a genuinely unbounded figure alike.
    pub cap: Option<u64>,
    pub policy: Policy,
    /// A free clause: the nearest time-to-reap, the probed path.
    pub note: Option<String>,
}

impl ProbeRow {
    pub(crate) fn new(
        name: impl Into<String>,
        current: u64,
        cap: Option<u64>,
        policy: Policy,
        note: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            current,
            cap,
            policy,
            note,
        }
    }
}

/// Render `rows` as one aligned [`Mark::Fields`] matrix: the figure (with
/// `/cap` when one is armed), then policy and note as muted ink.
pub(crate) fn rows_mark(rows: &[ProbeRow]) -> Mark {
    let fields = rows
        .iter()
        .map(|row| {
            let mut spans = Vec::new();
            let figure = match row.cap {
                Some(cap) => format!("{}/{}", row.current, cap),
                None => row.current.to_string(),
            };
            spans.push(Span {
                role: None,
                text: figure,
            });
            spans.push(Span {
                role: Some(Role::Muted),
                text: format!("  {}", row.policy),
            });
            if let Some(note) = &row.note {
                spans.push(Span {
                    role: Some(Role::Muted),
                    text: format!(" ({note})"),
                });
            }
            Field {
                label: row.name.clone(),
                value: FieldVal::Inline(spans),
            }
        })
        .collect();
    Mark::Fields { rows: fields }
}

/// Compose the agent's rows into the `/resources` card: a heading over one
/// [`rows_mark`] matrix. The frontend appends its own section at render time.
pub(crate) fn resources_card(rows: &[ProbeRow]) -> Card {
    Card(vec![Mark::heading("resources"), rows_mark(rows)])
}

/// Total bytes of every regular file under `root`, recursively; symlinks are
/// not followed (their target may leave the probed tree) and an unreadable
/// entry counts zero rather than failing the fold.
///
/// Sizes come per-path from `symlink_metadata`, not the `DirEntry`: on
/// Windows the enumeration figure is the directory's *cached* size, which
/// NTFS refreshes only when the last writer closes, so a live, still-open log
/// file would probe as 0.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:resources-disk-probe] the /resources disk figure: a read-only metadata walk of the session's own log/scratch dirs, priced at invocation; operator diagnostics, not turn-time model I/O"
)]
pub(crate) fn dir_size(root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                return 0;
            };
            if meta.is_dir() {
                dir_size(&path)
            } else if meta.is_file() {
                meta.len()
            } else {
                0
            }
        })
        .sum()
}

/// The eviction-pressure rows, mirroring `Avatar::evict`'s own trigger so
/// the pressure shown is the pressure that fires: a known window evicts on
/// tokens against that window, and only the unknown-window fallback still
/// runs on serialised bytes. `measured` is `None` when the token count is
/// stale ([`Avatar::measured_input`]) — a stale read must report unknown,
/// never a number that could sit at or over the trigger, so the
/// `context.tokens` row is omitted rather than shown with a fabricated figure.
fn pressure_rows(measured: Option<u64>, history_bytes: u64, window: Option<u64>) -> Vec<ProbeRow> {
    match (window, measured) {
        (Some(w), Some(tokens)) if w > 0 => vec![
            ProbeRow::new(
                "context.tokens",
                tokens,
                Some(eviction_trigger(w)),
                Policy::Evict,
                Some(format!("auto-eviction trigger; window {w} tokens")),
            ),
            ProbeRow::new(
                "log.bytes",
                history_bytes,
                None,
                Policy::Evict,
                Some(
                    "context bytes (fallback eviction gauge when the context window is unknown)"
                        .to_string(),
                ),
            ),
        ],
        (Some(w), None) if w > 0 => vec![ProbeRow::new(
            "log.bytes",
            history_bytes,
            Some(EVICT_THRESHOLD as u64),
            Policy::Evict,
            Some(format!(
                "auto-eviction threshold; token measure stale (window {w} tokens)"
            )),
        )],
        _ => vec![ProbeRow::new(
            "log.bytes",
            history_bytes,
            Some(EVICT_THRESHOLD as u64),
            Policy::Evict,
            Some("auto-eviction threshold; window unknown".to_string()),
        )],
    }
}

impl Avatar {
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

    /// Assemble this agent's half of the fold — one [`ProbeRow`] per
    /// accumulator this attend thread may legally read: the shell's worker
    /// registry and bindings, the inbox, the event log, the log-dir and
    /// scratch footprint (walked here at invocation, not on a timer), and
    /// the sub-agent idle lease. Nothing mutated, no lease renewed.
    ///
    /// # Errors
    /// The engine's severance.
    fn resource_rows(&self) -> Result<Vec<ProbeRow>, Severed> {
        let mut rows = Vec::new();

        let entries = self.seat.read(|t| t.workers())?;
        let mut running_worker = 0u64;
        let mut running_durable = 0u64;
        let mut settled = 0u64;
        let mut nearest_reap: Option<std::time::Duration> = None;
        let mut nearest_expiry: Option<u64> = None;
        for entry in &entries {
            if entry.running {
                match entry.class {
                    ral_core::types::LeaseClass::Worker => {
                        running_worker += 1;
                        // The nearer of the entry's two margins: idle off the
                        // shared last-observed cell, backstop off its
                        // (display-only, close enough here) wall-clock start.
                        let idle_left = shell_eval::DETACHED_WORKER_CEILING
                            .saturating_sub(std::time::Duration::from_secs(entry.idle_secs));
                        let age = std::time::Duration::from_secs(entry.up_secs);
                        let backstop_left =
                            shell_eval::DETACHED_WORKER_BACKSTOP.saturating_sub(age);
                        let left = idle_left.min(backstop_left);
                        nearest_reap = Some(nearest_reap.map_or(left, |m| m.min(left)));
                    }
                    ral_core::types::LeaseClass::Durable => running_durable += 1,
                }
            } else {
                settled += 1;
                // The engine's own figure, on its own clock.
                if let Some(left) = entry.retention_left {
                    nearest_expiry = Some(nearest_expiry.map_or(left, |m| m.min(left)));
                }
            }
        }
        rows.push(ProbeRow::new(
            "workers.running",
            running_worker + running_durable,
            Some(shell_eval::LIVE_WORKER_CAP as u64),
            Policy::Reject,
            None,
        ));
        rows.push(ProbeRow::new(
            "workers.running[worker]",
            running_worker,
            None,
            Policy::Reap,
            nearest_reap.map(|d| format!("nearest reap in {}", crate::clock::hms(d.as_secs()))),
        ));
        rows.push(ProbeRow::new(
            "workers.running[durable]",
            running_durable,
            None,
            Policy::Unbounded,
            Some("durable: dies by cancel, /clear, or process exit".to_string()),
        ));
        rows.push(ProbeRow::new(
            "workers.settled",
            settled,
            None,
            Policy::Reap,
            nearest_expiry.map(|n| format!("nearest expiry in {n} ral calls")),
        ));

        for (source, depth) in self.inbox.source_depths() {
            let (policy, note) = if source.coalesces() {
                (Policy::Coalesce, "merges/dedupes")
            } else {
                (Policy::Queue, "one per child, worker, or keystroke")
            };
            rows.push(ProbeRow::new(
                format!("inbox[{source}]"),
                depth,
                None,
                policy,
                Some(note.to_string()),
            ));
        }

        let log = self.log.borrow();
        let (events, history_bytes) = (
            log.context().event_count() as u64,
            log.context().history_bytes() as u64,
        );
        drop(log);
        rows.push(ProbeRow::new(
            "log.events",
            events,
            None,
            Policy::Evict,
            Some("counts the events the context still owns".to_string()),
        ));
        rows.extend(pressure_rows(
            self.measured_input(),
            history_bytes,
            self.agent.current_provider().context_window(),
        ));

        rows.push(ProbeRow::new(
            "bindings.count",
            self.seat.read(|t| t.binding_count())?,
            None,
            Policy::Reap,
            Some("baseline (prelude, agent library, host seeds) never expires".to_string()),
        ));
        rows.push(ProbeRow::new(
            "bindings.leased",
            self.seat.read(|t| t.leased_binding_count())?,
            None,
            Policy::Reap,
            Some(format!(
                "idle {} calls prunes",
                shell_eval::BINDING_IDLE_CALLS
            )),
        ));
        rows.push(ProbeRow::new(
            "bindings.largest_bytes",
            self.seat.read(|t| t.largest_binding_bytes())?,
            Some(shell_eval::LARGE_BINDING_BYTES),
            Policy::Warn,
            Some("shallow estimate; a closure's captures are never chased".to_string()),
        ));

        let log_dir = self.log.borrow().dir().to_path_buf();
        rows.push(ProbeRow::new(
            "disk.log_dir",
            dir_size(&log_dir),
            None,
            Policy::Warn,
            Some(log_dir.display().to_string()),
        ));
        if let Some((scratch, bytes)) = self.scratch_bytes()? {
            rows.push(ProbeRow::new(
                "disk.scratch",
                bytes,
                None,
                Policy::Warn,
                Some(scratch),
            ));
        }

        rows.push(ProbeRow::new(
            "agents.lease",
            self.fleet
                .nearest_reap()
                .unwrap_or(AGENT_LEASE_IDLE)
                .as_secs(),
            Some(AGENT_LEASE_IDLE.as_secs()),
            Policy::Reap,
            Some("renewed by a human exchange".to_string()),
        ));

        Ok(rows)
    }

    /// Publish the fold as one [`crate::record::Transient::Resources`] card, drawn live and
    /// never recorded — a probe fold is an interactive diagnostic, not a
    /// session fact.  Run by `Avatar::read` at whichever boundary drains the
    /// `/resources`; transcript and TUI only, never model-facing.
    pub(crate) fn emit_resources(&self, recorder: &crate::record::Emitter) {
        let rows = match self.resource_rows() {
            Ok(rows) => rows,
            Err(s) => {
                self.note(EngineLost::running(&s, self.agent.run_dir()).to_string());
                return;
            }
        };
        recorder.transient(crate::record::Transient::Resources {
            card: resources_card(&rows),
        });
    }

    /// `/context`'s one fact: the survey's turns as they stand, for the
    /// scrollback fold to draw.
    pub(crate) fn emit_context_survey(&self) {
        let survey = self.log.borrow().context().context_survey();
        // The card is a rendering the scrollback fold rebuilds at draw time,
        // never what the log carries.
        let recorder = self.recorder();
        if let Err(error) = recorder.emit(crate::record::Display::Context { turns: survey.rows }) {
            recorder.report_fault(&error);
        }
    }
}

#[cfg(test)]
mod tests;
