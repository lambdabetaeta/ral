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
use crate::agent::digest::{COMPACT_THRESHOLD, compaction_trigger};
use crate::bus::card::{Card, Field, FieldVal, Mark, Role, Span};
use crate::fleet::registry::{AGENT_DEMOTE_IDLE, AGENT_LEASE_IDLE};
use crate::shell_eval;
use ral_core::serial::FOValue;
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

/// One probed accumulator, one row per figure: what the fold renders. A probe
/// fold is an interactive diagnostic, read when it is run; no session keeps a
/// pressure history.
///
/// `policy` comes from a closed vocabulary — `"coalesce"`, `"reject"`,
/// `"evict"`, `"reap"`, `"warn"`, `"none (unbounded)"` — and is stated even
/// where the enforcement lands later, the row then carrying `cap: None` and a
/// note saying so.
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

/// The presentation bus's two probe figures.
///
/// Neither carries a cap: the transport's one enforced number is a
/// *per-entry* text cap (`bus::MERGE_TEXT_CAP`), a different axis from either
/// aggregate, so it is named in `bus.bytes`'s note rather than faked into
/// `cap`.
#[derive(Clone, Copy)]
pub struct BusFigures {
    /// Queue entries — a merged run and a reserved kind each count as one.
    pub depth: u64,
    /// Resident merged `Token`/`Thinking` text bytes.  `State` coalesces by
    /// replacement and carries no text, so it weighs nothing here.
    pub bytes: u64,
}

/// The rows for the accumulators the frontend owns.
///
/// Pure in its figures so the row shapes are unit-testable without a
/// terminal: the TUI's `Kind::Resources` arm reads them off the
/// tabs/viewport/bus it holds.
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
            Some("oldest evicted first; already durable in record.jsonl".to_string()),
        ),
        ProbeRow::new(
            "viewport.rows",
            viewport.rows,
            Some(viewport.rows_cap),
            "evict",
            Some("oldest evicted first; already durable in record.jsonl".to_string()),
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
/// entry counts zero rather than failing the fold.
///
/// Sizes come per-path from `symlink_metadata`, not the `DirEntry`: on
/// Windows the enumeration figure is the directory's *cached* size, which
/// NTFS refreshes only when the last writer closes, so a live, still-open log
/// file would probe as 0.
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

/// The compaction-pressure rows, mirroring `Agent::compact`'s own trigger so
/// the pressure shown is the pressure that fires: a known window compacts on
/// tokens against that window, and only the unknown-window fallback still
/// runs on serialised bytes. `measured` is `None` when the token count is
/// stale ([`Agent::measured_input`]) — a stale read must report unknown,
/// never a number that could sit at or over the trigger, so the
/// `context.tokens` row is omitted rather than shown with a fabricated figure.
fn pressure_rows(measured: Option<u64>, history_bytes: u64, window: Option<u64>) -> Vec<ProbeRow> {
    match (window, measured) {
        (Some(w), Some(tokens)) if w > 0 => vec![
            ProbeRow::new(
                "context.tokens",
                tokens,
                Some(compaction_trigger(w)),
                "evict",
                Some(format!("auto-compaction trigger; window {w} tokens")),
            ),
            ProbeRow::new(
                "log.bytes",
                history_bytes,
                None,
                "evict",
                Some(
                    "model-view bytes (fallback compaction gauge when the window is unknown)"
                        .to_string(),
                ),
            ),
        ],
        (Some(w), None) if w > 0 => vec![ProbeRow::new(
            "log.bytes",
            history_bytes,
            Some(COMPACT_THRESHOLD as u64),
            "evict",
            Some(format!(
                "auto-compaction threshold; token measure stale (window {w} tokens)"
            )),
        )],
        _ => vec![ProbeRow::new(
            "log.bytes",
            history_bytes,
            Some(COMPACT_THRESHOLD as u64),
            "evict",
            Some("auto-compaction threshold; window unknown".to_string()),
        )],
    }
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
            Some("counts the events still owned by the model view".to_string()),
        ));
        rows.extend(pressure_rows(
            self.measured_input(),
            self.log.lock().history_bytes() as u64,
            self.current_provider().context_window(),
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

    /// Publish the fold as one [`Transient::Resources`]: the agent rows
    /// beside the card rendering them, drawn live and never recorded — a
    /// probe fold is an interactive diagnostic, not a session fact. Called
    /// from `ReplControl` in `tui/tui_loop.rs`, at the exchange boundary
    /// where `/clear` runs; transcript and TUI only, never model-facing.
    pub(crate) fn emit_resources(&self, recorder: &crate::record::Emitter) {
        let rows = self.resource_rows();
        let card = resources_card(&rows);
        recorder.transient(crate::record::Transient::Resources { rows, card });
    }

    pub(crate) fn emit_context_survey(&self) {
        let survey = self.log.lock().context_survey();
        // The card is a rendering the view fold rebuilds at draw time, never
        // what the log carries.
        let rows = survey
            .items
            .iter()
            .map(|item| crate::record::ContextRow {
                exchange: item.exchange,
                kind: item.kind.as_str().to_string(),
                opening: item.opening.clone(),
                bytes: item.bytes,
                steps: item.steps,
                live: item.live,
            })
            .collect();
        let recorder = self.recorder();
        if let Err(error) = recorder.emit(crate::record::Display::Context { rows }) {
            recorder.report_fault(&error);
        }
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
    use crate::bus::{Emitter, Post};

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

    /// `/context`'s survey records a `Display::Context` commit through the
    /// seam — what lets a resumed scrollback rebuild the survey card the
    /// user saw.
    #[test]
    fn context_survey_records_a_display_commit() {
        use crate::record::{Display, Record};

        let session = Agent::for_test("system").unwrap();
        {
            let mut log = session.log.lock();
            log.append_user("one".into(), None).unwrap();
            log.append_assistant(genai::chat::ChatMessage::assistant("answer"), vec![], None)
                .unwrap();
        }
        let (tx, rx) = crate::bus::channel();
        session.recorder().attach(crate::record::FleetSink {
            id: session.id,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        session.emit_context_survey();

        let fact = crate::bus::drain_records(&rx)
            .into_iter()
            .find_map(|rec| match rec {
                Record::Display(Display::Context { rows }) => Some(rows),
                _ => None,
            })
            .expect("the survey records a Display::Context commit");
        assert_eq!(fact[0].exchange, 1);
        assert_eq!(fact[0].kind, "exchange");
    }

    /// The agent half surveys what this thread owns: the worker registry's
    /// running/settled split with its time-to-reap notes, the binding count,
    /// the inbox depths (counted, never drained), and the idle lease's
    /// fallback when nothing has forked.
    #[test]
    fn resource_rows_survey_the_agents_accumulators() {
        let mut session = Agent::for_test("system").unwrap();
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

        assert_eq!(
            row(&rows, "log.events").current,
            0,
            "shell-only work has not entered the model view"
        );
        assert!(
            row(&rows, "disk.log_dir").current > 0,
            "a session dir with a written record.jsonl probes nonzero"
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
        session
            .inbox
            .push(Post::Nudge {
                exchange: 1,
                text: "go on".into(),
            })
            .unwrap();
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
        let mut session = Agent::for_test("system").unwrap();
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

    /// The scripted test provider's model has no pricing-catalog entry (the
    /// catalog is fetched over the network, never populated in a test
    /// process), so its window reads `None` — the fallback arm nearly every
    /// other test in this module exercises without knowing it. `log.bytes`
    /// alone carries the cap, and no `context.tokens` row appears.
    #[test]
    fn resource_rows_unknown_window_falls_back_to_capped_log_bytes() {
        let session = Agent::for_test("system").unwrap();
        let rows = session.resource_rows();
        let bytes = row(&rows, "log.bytes");
        assert_eq!(
            bytes.cap,
            Some(COMPACT_THRESHOLD as u64),
            "an unknown window falls back to the byte threshold `compact` itself uses"
        );
        assert_eq!(bytes.policy, "evict");
        assert!(
            !rows.iter().any(|r| r.name == "context.tokens"),
            "no window means no token-pressure row to show"
        );
    }

    /// A known window is the pressure `Agent::compact` actually fires on, so
    /// that is what the fold must show: `context.tokens` capped at
    /// `compaction_trigger(w)`, and `log.bytes` demoted to an uncapped
    /// fallback gauge rather than faking a second, unenforced ceiling.
    #[test]
    fn known_window_reports_context_tokens_not_bytes() {
        let rows = pressure_rows(Some(12_345), 4_096, Some(200_000));
        let tokens = row(&rows, "context.tokens");
        assert_eq!(tokens.current, 12_345, "the live input-token numerator");
        assert_eq!(
            tokens.cap,
            Some(compaction_trigger(200_000)),
            "the same trigger `Agent::compact` fires auto-compaction on"
        );
        assert_eq!(tokens.policy, "evict");
        assert!(
            tokens.note.as_deref().is_some_and(|n| n.contains("200000")),
            "the note names the window the trigger was computed from"
        );

        let bytes = row(&rows, "log.bytes");
        assert_eq!(bytes.current, 4_096, "the byte gauge still reports");
        assert_eq!(
            bytes.cap, None,
            "log.bytes is no longer where the compaction pressure lives"
        );
        assert_eq!(
            rows.iter().filter(|r| r.name == "log.bytes").count(),
            1,
            "log.bytes still rides along as an uncapped gauge, exactly once"
        );
    }

    /// A stale measure must read as unknown, never as a number that could sit
    /// at or over the trigger: right after a context edit the token count is
    /// stale, so `context.tokens` drops out entirely rather than showing a
    /// figure the design forbids ("stale reads unknown, never high").
    #[test]
    fn stale_measure_omits_context_tokens() {
        let rows = pressure_rows(None, 4_096, Some(200_000));
        assert!(
            !rows.iter().any(|r| r.name == "context.tokens"),
            "a stale token measure must not surface as a number"
        );
        let bytes = row(&rows, "log.bytes");
        assert_eq!(bytes.current, 4_096);
        assert_eq!(
            bytes.cap,
            Some(COMPACT_THRESHOLD as u64),
            "falls back to the same byte threshold as an unknown window"
        );
    }
}
