//! The `/resources` probe fold: one row per session-lived accumulator,
//! rendered as one card.
//!
//! The probe convention ([[invariants/probe-convention]], per
//! `decisions/260705_leases-and-budgets`): every session-lived accumulator
//! registers a [`ProbeRow`] — its name, current size, cap, and pressure
//! policy — and `/resources` is a *fold over the registered probes*, never
//! a bespoke report. The fold is deliberately the inspector built before
//! any enforcer: an accumulator whose cap lands later states its *decided*
//! policy with `cap: None` and a note saying so, because a budget that
//! cannot be inspected will be debugged by restarting the process.
//!
//! Probing never mutates and never renews a lease — enumeration is not
//! observation, the same ledger law the `workers` listing obeys
//! (`decisions/260705_session-ledger`). Residents (workers, agents,
//! schedules — things a capability reaches) and mere accumulators (a
//! viewport, the bus, an inbox) are both probed; only residents are
//! listed, cancelled, and leased.
//!
//! The fold has two halves, split by who may legally read what: the agent
//! assembles its own rows on its drive thread (the shell's registry and
//! bindings, its inbox, log, and disk — `Agent::resource_rows`), and the
//! frontend appends the rows for the accumulators *it* owns (viewports,
//! views, the bus — [`frontend_rows`]) when the card reaches it. Neither
//! half reaches across a thread for the other's figures.

use crate::card::{Card, Field, FieldVal, Mark, Role, Span};
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

/// One probed accumulator: the row the `/resources` fold renders and
/// `transcript.jsonl` records.
///
/// `policy` is the accumulator's *pressure policy* from the ADR's closed
/// vocabulary — `"coalesce"` (idempotent entries merge), `"reject"`
/// (admission refused at the cap), `"evict"` (old entries dropped),
/// `"reap"` (a lease expires it), `"warn"` (reported, never acted on), or
/// `"none (unbounded)"` — stated even where the enforcement lands later:
/// the row then carries `cap: None` and a note saying the cap is pending,
/// so the inspector precedes the enforcer honestly.
#[derive(Clone, Debug, Serialize)]
pub struct ProbeRow {
    /// The accumulator's name, one row per figure of a multi-figure
    /// accumulator (`viewport.blocks`, `inbox[user]`, …).
    pub name: String,
    /// Its size now, in the unit the name implies (a count, bytes,
    /// seconds).
    pub current: u64,
    /// The enforced bound, when one is armed; `None` for a decided-but-
    /// unenforced cap (see the note) or a genuinely unbounded figure.
    pub cap: Option<u64>,
    /// The pressure policy, from the closed vocabulary above.
    pub policy: &'static str,
    /// A free clause: the nearest time-to-reap, the pending-enforcement
    /// disclaimer, the probed path.
    pub note: Option<String>,
}

impl ProbeRow {
    /// Build a row; the one constructor, so every call site reads the same
    /// field order the struct declares.
    pub fn new(
        name: impl Into<String>,
        current: u64,
        cap: Option<u64>,
        policy: &'static str,
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

/// Render `rows` as one aligned [`Mark::Fields`] matrix: per row, the
/// current figure (with `/cap` when one is armed), then the policy and
/// note as muted ink — data first, discipline second.
pub fn rows_mark(rows: &[ProbeRow]) -> Mark {
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
                    text: format!(" — {note}"),
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

/// A muted one-line section heading, for the frontend to title the rows it
/// appends beneath the agent's.
pub fn section_mark(title: &str) -> Mark {
    Mark::Text {
        spans: vec![Span {
            role: Some(Role::Strong),
            text: title.to_string(),
        }],
    }
}

/// Compose the agent's probe rows into the `/resources` card: a `resources`
/// heading over one [`rows_mark`] matrix. The frontend appends its own
/// section to this card at render time; the raw rows ride beside it on the
/// bus for `transcript.jsonl`.
pub fn resources_card(rows: &[ProbeRow]) -> Card {
    Card(vec![section_mark("resources"), rows_mark(rows)])
}

/// The rows for the accumulators the frontend owns — viewport figures for
/// the probed agent's view, the fleet's view counts, and the bus. Pure in
/// its figures so the row shapes are unit-testable without a terminal:
/// the caller (the TUI's `Kind::Resources` arm) reads the figures off the
/// tabs/viewport structures it holds.
///
/// The bus row carries no figure: the session-lifetime channel is
/// unbounded and exposes no depth, so the row states the decided policy
/// and says the figure arrives with the bounded transport — the inspector
/// does not grow counting machinery the enforcement replaces.
pub fn frontend_rows(
    viewport_blocks: u64,
    viewport_rows: u64,
    viewport_bytes: u64,
    live_views: u64,
    dead_views: u64,
    live_agents: u64,
) -> Vec<ProbeRow> {
    let windowed = || Some("window lands with enforcement".to_string());
    vec![
        ProbeRow::new(
            "viewport.blocks",
            viewport_blocks,
            None,
            "evict",
            windowed(),
        ),
        ProbeRow::new("viewport.rows", viewport_rows, None, "evict", windowed()),
        ProbeRow::new("viewport.bytes", viewport_bytes, None, "evict", windowed()),
        ProbeRow::new(
            "views.live",
            live_views,
            None,
            "none (unbounded)",
            Some("one per live agent".to_string()),
        ),
        ProbeRow::new(
            "views.dead",
            dead_views,
            None,
            "evict",
            Some("tombstone eviction lands with enforcement".to_string()),
        ),
        ProbeRow::new(
            "bus.depth",
            0,
            None,
            "coalesce",
            Some(
                "unbounded channel exposes no depth; the figure arrives with the bounded transport"
                    .to_string(),
            ),
        ),
        ProbeRow::new(
            "fleet.agents",
            live_agents,
            None,
            "reap",
            Some("the frontend's tab view; the registry is the authority".to_string()),
        ),
    ]
}

/// A duration as terse probe ink — `2h05m`, `41m09s`, `12s` — for the
/// nearest-reap notes.
pub fn terse_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Total size in bytes of every regular file under `root`, recursively —
/// the disk probe's figure, walked at invocation and never periodically,
/// so the probe's cost is paid exactly when the operator asks. Symlinks
/// are not followed (their target may leave the probed tree); unreadable
/// entries count zero rather than fail the fold.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:resources-disk-probe] the /resources disk figure: a read-only metadata walk of the session's own log/scratch dirs, priced at invocation; operator diagnostics, not turn-time model I/O"
)]
pub fn dir_size(root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let Ok(meta) = entry.metadata() else {
                return 0;
            };
            if meta.is_dir() {
                dir_size(&entry.path())
            } else if meta.is_file() {
                meta.len()
            } else {
                0
            }
        })
        .sum()
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    /// The frontend half of the fold: every row wears its decided policy,
    /// and the not-yet-enforced accumulators say so instead of faking a
    /// cap — the inspector precedes the enforcer honestly.
    #[test]
    fn frontend_rows_state_decided_policies_without_fake_caps() {
        let rows = frontend_rows(3, 120, 4096, 2, 1, 2);
        let by_name = |n: &str| {
            rows.iter()
                .find(|r| r.name == n)
                .unwrap_or_else(|| panic!("row {n} must be emitted"))
        };
        assert_eq!(by_name("viewport.blocks").current, 3);
        assert_eq!(by_name("viewport.rows").current, 120);
        assert_eq!(by_name("viewport.bytes").current, 4096);
        assert_eq!(by_name("views.live").current, 2);
        assert_eq!(by_name("views.dead").current, 1);
        assert_eq!(by_name("fleet.agents").current, 2);
        for row in &rows {
            assert!(
                row.cap.is_none(),
                "no frontend cap is enforced yet, so no row may claim one ({})",
                row.name
            );
        }
        assert_eq!(by_name("viewport.blocks").policy, "evict");
        assert_eq!(by_name("bus.depth").policy, "coalesce");
        assert!(
            by_name("bus.depth")
                .note
                .as_deref()
                .is_some_and(|n| n.contains("bounded transport")),
            "the bus row must say where its figure arrives"
        );
    }

    /// The card is a heading plus one aligned matrix, one field per row,
    /// with the cap rendered into the figure only when armed.
    #[test]
    fn resources_card_renders_one_field_per_row() {
        let rows = vec![
            ProbeRow::new("workers.running", 3, Some(64), "reject", None),
            ProbeRow::new("bindings.count", 7, None, "reap", Some("x".into())),
        ];
        let card = resources_card(&rows);
        assert_eq!(card.marks().len(), 2, "a heading and one matrix");
        let Mark::Fields { rows: fields } = &card.marks()[1] else {
            panic!("the second mark must be the fields matrix");
        };
        assert_eq!(fields.len(), 2);
        let FieldVal::Inline(spans) = &fields[0].value else {
            panic!("a probe field renders inline spans");
        };
        assert_eq!(spans[0].text, "3/64", "an armed cap rides the figure");
        let FieldVal::Inline(spans) = &fields[1].value else {
            panic!("a probe field renders inline spans");
        };
        assert_eq!(spans[0].text, "7", "an unarmed cap adds nothing");
    }

    #[test]
    fn terse_duration_picks_the_coarsest_fitting_unit() {
        assert_eq!(terse_duration(Duration::from_secs(12)), "12s");
        assert_eq!(terse_duration(Duration::from_secs(69)), "1m09s");
        assert_eq!(terse_duration(Duration::from_secs(7500)), "2h05m");
    }

    /// The disk probe sums regular files recursively and returns zero for
    /// a missing directory rather than failing the fold.
    #[test]
    fn dir_size_sums_files_recursively() {
        let root = std::env::temp_dir().join(format!("exarch-dirsize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a"), b"12345").unwrap();
        std::fs::write(root.join("sub/b"), b"123").unwrap();
        assert_eq!(dir_size(&root), 8);
        assert_eq!(dir_size(&root.join("missing")), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
