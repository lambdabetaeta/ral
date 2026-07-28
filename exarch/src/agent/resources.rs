//! The `/resources` probe fold: one [`ProbeRow`] per session-lived
//! accumulator — name, size, cap, pressure policy — rendered as one card.
//!
//! The fold has two halves, split by who may legally read what: the agent
//! assembles its own rows on its attend thread ([`Agent::resource_rows`]),
//! and the TUI's `Kind::Resources` arm appends the rows for the accumulators
//! *it* owns ([`frontend_rows`]). Neither half reaches across a thread for
//! the other's figures. Probing mutates nothing and renews no lease, so
//! `/resources` cannot immortalise the zombies it exists to reveal.

use crate::agent::Agent;
use crate::agent::digest::COMPACT_THRESHOLD;
use crate::bus::card::{Card, Field, FieldVal, Mark, Role, Span};
use crate::bus::{Emitter, Kind};
use crate::fleet::registry::{AGENT_DEMOTE_IDLE, AGENT_LEASE_IDLE};
use crate::shell_eval;
use ral_core::serial::FOValue;
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

/// One probed accumulator, one row per figure: what the fold renders and
/// `transcript.jsonl` records. `policy` comes from a closed vocabulary —
/// `"coalesce"`, `"reject"`, `"evict"`, `"reap"`, `"warn"`, `"none
/// (unbounded)"` — and is stated even where the enforcement lands later,
/// the row then carrying `cap: None` and a note saying so.
#[derive(Clone, Debug, Serialize)]
pub struct ProbeRow {
    pub name: String,
    /// Its size now, in the unit the name implies (a count, bytes, seconds).
    pub current: u64,
    /// The enforced bound, when one is armed; `None` for a decided-but-
    /// unenforced cap and for a genuinely unbounded figure alike.
    pub cap: Option<u64>,
    pub policy: &'static str,
    /// A free clause: the nearest time-to-reap, the probed path.
    pub note: Option<String>,
}

impl ProbeRow {
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

/// Render `rows` as one aligned [`Mark::Fields`] matrix: the figure (with
/// `/cap` when one is armed), then policy and note as muted ink.
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

/// A strong one-line section heading, titling a run of rows.
pub fn section_mark(title: &str) -> Mark {
    Mark::Text {
        spans: vec![Span {
            role: Some(Role::Strong),
            text: title.to_string(),
        }],
    }
}

/// Compose the agent's rows into the `/resources` card: a heading over one
/// [`rows_mark`] matrix. The frontend appends its own section at render
/// time; the raw rows ride beside the card on the bus.
pub fn resources_card(rows: &[ProbeRow]) -> Card {
    Card(vec![section_mark("resources"), rows_mark(rows)])
}

/// The probed agent's viewport window: figures beside the caps that bound
/// them, in one struct so a figure cannot drift from its cap.
#[derive(Clone, Copy)]
pub struct ViewportFigures {
    pub blocks: u64,
    /// Rendered rows in the memoised flatten, as of the last paint.
    pub rows: u64,
    /// Those rows' summed text bytes; no cap of its own, bounded indirectly
    /// by the blocks/rows caps.
    pub bytes: u64,
    /// `tui::viewport::VIEWPORT_MAX_BLOCKS`.
    pub blocks_cap: u64,
    /// `tui::viewport::VIEWPORT_MAX_ROWS`.
    pub rows_cap: u64,
}

/// The fleet's view counts: per-agent views the frontend holds, split
/// live/dead, plus the live-agent tab count.
#[derive(Clone, Copy)]
pub struct ViewFigures {
    pub live: u64,
    /// Views whose agent has died — lingering, or already tombstoned down to
    /// (id, status, log path) once past `tui::LINGER`.
    pub dead: u64,
    pub agents: u64,
}

/// The presentation bus's two probe figures. Neither carries a cap: the
/// transport's one enforced number is a *per-entry* text cap
/// (`bus::MERGE_TEXT_CAP`), a different axis from either aggregate, so it is
/// named in `bus.bytes`'s note rather than faked into `cap`.
#[derive(Clone, Copy)]
pub struct BusFigures {
    /// Queue entries — a merged run and a reserved kind each count as one.
    pub depth: u64,
    /// Resident merged `Token`/`Thinking`/`Phase` text bytes.
    pub bytes: u64,
}

/// The rows for the accumulators the frontend owns. Pure in its figures so
/// the row shapes are unit-testable without a terminal: the TUI's
/// `Kind::Resources` arm reads them off the tabs/viewport/bus it holds.
pub fn frontend_rows(
    viewport: ViewportFigures,
    views: ViewFigures,
    bus: BusFigures,
) -> Vec<ProbeRow> {
    vec![
        ProbeRow::new(
            "viewport.blocks",
            viewport.blocks,
            Some(viewport.blocks_cap),
            "evict",
            Some("oldest evicted first; already durable in user.log".to_string()),
        ),
        ProbeRow::new(
            "viewport.rows",
            viewport.rows,
            Some(viewport.rows_cap),
            "evict",
            Some("oldest evicted first; already durable in user.log".to_string()),
        ),
        ProbeRow::new(
            "viewport.bytes",
            viewport.bytes,
            None,
            "evict",
            Some("no byte cap of its own; bounded indirectly by the blocks/rows caps".to_string()),
        ),
        ProbeRow::new(
            "views.live",
            views.live,
            None,
            "none (unbounded)",
            Some("one per live agent".to_string()),
        ),
        ProbeRow::new(
            "views.dead",
            views.dead,
            None,
            "evict",
            Some("tombstoned (id, status, log path) once past LINGER".to_string()),
        ),
        ProbeRow::new(
            "bus.depth",
            bus.depth,
            None,
            "coalesce",
            Some("entries; a same-class run off one agent merges into its tail".to_string()),
        ),
        ProbeRow::new(
            "bus.bytes",
            bus.bytes,
            None,
            "evict",
            Some(format!(
                "resident merged token/thinking/phase text; each run elides past {} KiB",
                crate::bus::MERGE_TEXT_CAP / 1024
            )),
        ),
        ProbeRow::new(
            "fleet.agents",
            views.agents,
            None,
            "reap",
            Some("the frontend's tab view; the registry is the authority".to_string()),
        ),
    ]
}

/// The shared 3-tier `h/m/s` formatter — `2h05m` / `41m09s` / `12s` — with
/// `sep` between the two units of the multi-unit forms: `terse_duration`
/// passes `""`, the TUI's rate-limit readout `" "`.
pub fn hms(secs: u64, sep: &str) -> String {
    if secs >= 3600 {
        format!("{}h{sep}{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{sep}{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// A duration as terse probe ink, for the nearest-reap notes.
pub fn terse_duration(d: Duration) -> String {
    hms(d.as_secs(), "")
}

/// Total bytes of every regular file under `root`, recursively; symlinks are
/// not followed (their target may leave the probed tree) and an unreadable
/// entry counts zero rather than failing the fold. Sizes come per-path from
/// `symlink_metadata`, not the `DirEntry`: on Windows the enumeration figure
/// is the directory's *cached* size, which NTFS refreshes only when the last
/// writer closes, so a live, still-open log file would probe as 0.
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

impl Agent {
    /// Assemble this agent's half of the fold — one [`ProbeRow`] per
    /// accumulator this attend thread may legally read: the shell's worker
    /// registry and bindings, the inbox, the event log, the log-dir and
    /// scratch footprint (walked here at invocation, not on a timer), and
    /// the sub-agent idle lease. Nothing mutated, no lease renewed.
    fn resource_rows(&self) -> Vec<ProbeRow> {
        let mut rows = Vec::new();

        let entries = self.probe_workers();
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
                // An unstamped entry has its whole retention ahead — the
                // sweep stamps it next call.
                let left = match entry.settled_epoch {
                    Some(s) => shell_eval::SETTLED_WORKER_RETENTION
                        .saturating_sub(self.ral_epoch.saturating_sub(s)),
                    None => shell_eval::SETTLED_WORKER_RETENTION,
                };
                nearest_expiry = Some(nearest_expiry.map_or(left, |m| m.min(left)));
            }
        }
        rows.push(ProbeRow::new(
            "workers.running",
            running_worker + running_durable,
            Some(shell_eval::LIVE_WORKER_CAP as u64),
            "reject",
            None,
        ));
        rows.push(ProbeRow::new(
            "workers.running[worker]",
            running_worker,
            None,
            "reap",
            nearest_reap.map(|d| format!("nearest reap in {}", terse_duration(d))),
        ));
        rows.push(ProbeRow::new(
            "workers.running[durable]",
            running_durable,
            None,
            "none (unbounded)",
            Some("durable — dies by cancel, /clear, or process exit".to_string()),
        ));
        rows.push(ProbeRow::new(
            "workers.settled",
            settled,
            None,
            "reap",
            nearest_expiry.map(|n| format!("nearest expiry in {n} ral calls")),
        ));

        for (source, depth) in self.inbox.source_depths() {
            // Idempotent sources merge or dedupe and never reject, so no cap
            // is enforced against their depth; the rest are admitted or
            // rejected at `INBOX_SOURCE_CAP`, never silently dropped.
            let (policy, cap, note) = match source {
                "user" | "schedule" | "nudge" => (
                    "coalesce",
                    None,
                    "merges/dedupes; never rejects".to_string(),
                ),
                _ => (
                    "reject",
                    Some(crate::bus::INBOX_SOURCE_CAP as u64),
                    format!(
                        "rejected at quota; {} total across every source",
                        crate::bus::INBOX_TOTAL_CAP
                    ),
                ),
            };
            rows.push(ProbeRow::new(
                format!("inbox[{source}]"),
                depth,
                cap,
                policy,
                Some(note),
            ));
        }

        rows.push(ProbeRow::new(
            "log.events",
            self.log.lock().event_count() as u64,
            None,
            "evict",
            Some("prefix drops with compaction".to_string()),
        ));
        rows.push(ProbeRow::new(
            "log.bytes",
            self.log.lock().history_bytes() as u64,
            Some(COMPACT_THRESHOLD as u64),
            "evict",
            Some("auto-compaction threshold".to_string()),
        ));

        let probe_count = |label: &str| match self.seat.transport().probe(FOValue::Variant {
            label: label.into(),
            payload: None,
        }) {
            Ok(FOValue::Int { value }) => {
                #[allow(
                    clippy::cast_sign_loss,
                    reason = "probe binding-count is a non-negative cardinality"
                )]
                let count = value as u64;
                count
            }
            other => unreachable!("`{label} probe must answer an Int, got {other:?}"),
        };
        rows.push(ProbeRow::new(
            "bindings.count",
            probe_count("binding-count"),
            None,
            "reap",
            Some("baseline (prelude, agent library, host seeds) never expires".to_string()),
        ));
        rows.push(ProbeRow::new(
            "bindings.leased",
            probe_count("leased-binding-count"),
            None,
            "reap",
            Some(format!(
                "idle {} calls prunes",
                shell_eval::BINDING_IDLE_CALLS
            )),
        ));
        rows.push(ProbeRow::new(
            "bindings.largest_bytes",
            probe_count("largest-binding-bytes"),
            Some(shell_eval::LARGE_BINDING_BYTES),
            "warn",
            Some("shallow estimate; a closure's captures are never chased".to_string()),
        ));

        let log_dir = self.log.lock().dir().to_path_buf();
        rows.push(ProbeRow::new(
            "disk.log_dir",
            dir_size(&log_dir),
            None,
            "warn",
            Some(log_dir.display().to_string()),
        ));
        if let Some(scratch) = self.probe_env_var("EXARCH_SCRATCH") {
            rows.push(ProbeRow::new(
                "disk.scratch",
                dir_size(&std::path::PathBuf::from(&scratch)),
                None,
                "warn",
                Some(scratch),
            ));
        }

        rows.push(ProbeRow::new(
            "agents.lease",
            self.agents
                .nearest_reap()
                .unwrap_or(AGENT_LEASE_IDLE)
                .as_secs(),
            Some(AGENT_LEASE_IDLE.as_secs()),
            "reap",
            Some("renewed by a human exchange".to_string()),
        ));
        rows.push(ProbeRow::new(
            "agents.demote",
            AGENT_DEMOTE_IDLE.as_secs(),
            None,
            "warn",
            Some("idle threshold after which a child leaves the tab cycle".to_string()),
        ));

        rows
    }

    /// Emit the fold as one [`Kind::Resources`] event: the agent rows beside
    /// the card rendering them.  Called from `ReplControl` in
    /// `tui/tui_loop.rs`, at the exchange boundary where `/clear` runs;
    /// transcript and TUI only, never model-facing.
    pub(crate) fn emit_resources(&self, emit: &Emitter) {
        let rows = self.resource_rows();
        let card = resources_card(&rows);
        emit.emit(Kind::Resources { rows, card });
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::testkit::*;
    use crate::bus::Post;

    /// Every frontend row wears its policy, but only the viewport window's
    /// two enforced caps become real `cap`s; the rest stay `None` rather
    /// than fake a ceiling.
    #[test]
    fn frontend_rows_state_decided_policies_and_the_viewport_window_caps() {
        let rows = frontend_rows(
            ViewportFigures {
                blocks: 3,
                rows: 120,
                bytes: 4096,
                blocks_cap: 500,
                rows_cap: 20_000,
            },
            ViewFigures {
                live: 2,
                dead: 1,
                agents: 2,
            },
            BusFigures {
                depth: 5,
                bytes: 777,
            },
        );
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
        assert_eq!(by_name("bus.depth").current, 5);
        assert_eq!(by_name("bus.bytes").current, 777);
        assert_eq!(
            by_name("viewport.blocks").cap,
            Some(500),
            "the block-count window cap is now enforced and shown"
        );
        assert_eq!(
            by_name("viewport.rows").cap,
            Some(20_000),
            "the row window cap is now enforced and shown"
        );
        for name in [
            "viewport.bytes",
            "views.live",
            "views.dead",
            "bus.depth",
            "bus.bytes",
            "fleet.agents",
        ] {
            assert!(
                by_name(name).cap.is_none(),
                "no cap of its own is enforced for this row ({name})"
            );
        }
        assert_eq!(by_name("viewport.blocks").policy, "evict");
        assert_eq!(by_name("bus.depth").policy, "coalesce");
        assert!(
            by_name("bus.bytes")
                .note
                .as_deref()
                .is_some_and(|n| n.contains("KiB")),
            "the bus bytes row must name the per-run elision cap"
        );
    }

    /// The card is a heading plus one matrix, one field per row, the cap
    /// rendered into the figure only when armed.
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
        assert_eq!(terse_duration(Duration::from_mins(125)), "2h05m");
    }

    /// A missing directory reads zero rather than failing the fold.
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

    fn row<'a>(rows: &'a [ProbeRow], name: &str) -> &'a ProbeRow {
        rows.iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("the fold must emit a `{name}` row"))
    }

    /// The agent half surveys what this thread owns: the worker registry's
    /// running/settled split with its time-to-reap notes, the binding count,
    /// the inbox depths (counted, never drained), and the idle lease's
    /// fallback when nothing has forked.
    #[test]
    fn resource_rows_survey_the_agents_accumulators() {
        let dir = tmp("resource-rows");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        session.run_shell("c1".into(), "spawn { test-clear-block-forever }", 30, &emit);
        session.run_shell("c2".into(), "spawn { return 7 }", 30, &emit);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !session.probe_workers().iter().any(|w| !w.running) {
            assert!(
                std::time::Instant::now() < deadline,
                "the instant worker must settle within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let before = row(&session.resource_rows(), "bindings.count").current;
        session.run_shell("c3".into(), "let probe_marker = 1", 30, &emit);
        let rows = session.resource_rows();
        assert_eq!(
            row(&rows, "bindings.count").current,
            before + 1,
            "a `let` adds exactly one binding to the probe figure"
        );

        let running = row(&rows, "workers.running");
        assert_eq!(running.current, 1);
        assert_eq!(
            running.cap,
            Some(shell_eval::LIVE_WORKER_CAP as u64),
            "the admission cap is armed"
        );
        let running_worker = row(&rows, "workers.running[worker]");
        assert_eq!(running_worker.current, 1);
        assert!(
            running_worker
                .note
                .as_deref()
                .is_some_and(|n| n.starts_with("nearest reap in ")),
            "a running worker carries its time-to-reap"
        );
        let settled = row(&rows, "workers.settled");
        assert_eq!(settled.current, 1);
        assert!(
            settled
                .note
                .as_deref()
                .is_some_and(|n| n.contains("ral calls")),
            "a settled entry carries its retention remaining in ral calls"
        );

        assert!(row(&rows, "log.events").current > 0);
        assert!(
            row(&rows, "disk.log_dir").current > 0,
            "a session dir with a written events.json probes nonzero"
        );

        assert_eq!(
            row(&rows, "agents.lease").current,
            AGENT_LEASE_IDLE.as_secs(),
            "no live children — the lease row reports the full idle window"
        );

        // The settled spawn's deferred `Surface` batch may also sit queued —
        // a legitimate arrival, not the probe's doing — so stability
        // compares snapshots rather than pinning the whole vector.
        session
            .inbox
            .push(Post::UserSteering("hold".into()))
            .unwrap();
        session.inbox.push(Post::Nudge("go on".into())).unwrap();
        let depths_before = session.inbox.source_depths();
        let rows = session.resource_rows();
        assert_eq!(row(&rows, "inbox[user]").current, 1);
        assert_eq!(row(&rows, "inbox[nudge]").current, 1);
        assert_eq!(
            row(&rows, "inbox[agent]").current,
            0,
            "an idle source still emits its zero row — the row set is stable"
        );
        assert_eq!(
            session.inbox.source_depths(),
            depths_before,
            "probing drained nothing"
        );

        // End the blocked worker so the test does not leak a live thread.
        let workers = session.seat.shell_mut().shell.workers();
        for entry in workers {
            entry
                .handle
                .cancel
                .cancel(ral_core::process::CancelCause::Explicit);
        }
    }

    /// Probing renews nothing: assembling the rows reads a running worker's
    /// `last_observed` cell without touching it.
    #[test]
    fn resource_rows_renew_no_lease() {
        let dir = tmp("resource-rows-no-renew");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        session.run_shell("c1".into(), "spawn { test-clear-block-forever }", 30, &emit);

        let entry = session
            .seat
            .shell_mut()
            .shell
            .workers()
            .pop()
            .expect("the spawn registered its worker");
        let before = *entry.handle.last_observed.lock().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = session.resource_rows();
        let after = *entry.handle.last_observed.lock().unwrap();
        assert_eq!(after, before, "the probe must not renew the lease");

        entry
            .handle
            .cancel
            .cancel(ral_core::process::CancelCause::Explicit);
    }
}
