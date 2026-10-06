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
use crate::agent::gauge::{EVICT_THRESHOLD, eviction_trigger};
use crate::agent::seat::EngineLost;
use crate::bus::card::{Card, Field, FieldVal, Mark, Role, Span};
use crate::fleet::AGENT_LEASE_IDLE;
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

        let log = self.log.lock();
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

        let log_dir = self.log.lock().dir().to_path_buf();
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

    /// Publish the fold as one [`Transient::Resources`] card, drawn live and
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
        let survey = self.log.lock().context().context_survey();
        // The card is a rendering the scrollback fold rebuilds at draw time,
        // never what the log carries.
        let recorder = self.recorder();
        if let Err(error) = recorder.emit(crate::record::Display::Context { turns: survey.rows }) {
            recorder.report_fault(&error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::testkit::*;
    use crate::bus::{Emitter, Post};

    /// The card is a heading plus one matrix, one field per row, the cap
    /// rendered into the figure only when armed.
    #[test]
    fn resources_card_renders_one_field_per_row() {
        let rows = vec![
            ProbeRow::new("workers.running", 3, Some(64), Policy::Reject, None),
            ProbeRow::new("bindings.count", 7, None, Policy::Reap, Some("x".into())),
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

    /// `/context`'s survey records a `Display::Context` commit — what lets a
    /// resumed scrollback rebuild the survey card the user saw.
    #[test]
    fn context_survey_records_a_display_commit() {
        use crate::record::{Display, Record};

        let session = Avatar::for_test("system").unwrap();
        {
            let mut log = session.log.lock();
            log.append_user("one".into(), None).unwrap();
            log.append_assistant(genai::chat::ChatMessage::assistant("answer"), vec![], None)
                .unwrap();
        }
        let (tx, rx) = crate::bus::channel();
        session.recorder().attach(crate::record::FleetSink {
            id: session.agent.id,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        session.emit_context_survey();

        let fact = crate::bus::drain_records(&rx)
            .into_iter()
            .find_map(|rec| match rec {
                Record::Display(Display::Context { turns, .. }) => Some(turns),
                _ => None,
            })
            .expect("the survey records a Display::Context commit");
        assert_eq!(
            (fact[0].id, fact[0].role),
            (1, crate::agent::log::Role::User)
        );
        assert_eq!(fact[0].kind, crate::agent::log::TurnKind::Own);
    }

    /// The agent half surveys what this thread owns: the worker registry's
    /// running/settled split with its time-to-reap notes, the binding count,
    /// the inbox depths (counted, never drained), and the idle lease's
    /// fallback when nothing has forked.
    #[test]
    fn resource_rows_survey_the_agents_accumulators() {
        let session = dressed_trunk(|shell| shell.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

        session.ral("spawn { test-clear-block-forever }", 30, &emit);
        session.ral("spawn { return 7 }", 30, &emit);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !session
            .seat
            .read(|t| t.workers())
            .expect("an identity seat never severs")
            .iter()
            .any(|w| !w.running)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the instant worker must settle within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let before = row(
            &session
                .resource_rows()
                .expect("an identity seat never severs"),
            "bindings.count",
        )
        .current;
        session.ral("let probe_marker = 1", 30, &emit);
        let rows = session
            .resource_rows()
            .expect("an identity seat never severs");
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
            "a settled entry carries the retention the engine says it has left"
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
            "no live children: the lease row reports the full idle window"
        );

        // The settled spawn's deferred `Surface` batch may also sit queued —
        // a legitimate arrival, not the probe's doing — so stability
        // compares snapshots rather than pinning the whole vector.
        session.inbox.push(Post::UserSteering("hold".into()));
        session.inbox.push(Post::Nudge {
            prompt: 1,
            text: "go on".into(),
        });
        let depths_before = session.inbox.source_depths();
        let rows = session
            .resource_rows()
            .expect("an identity seat never severs");
        assert_eq!(row(&rows, "inbox[user]").current, 1);
        assert_eq!(row(&rows, "inbox[nudge]").current, 1);
        assert_eq!(
            row(&rows, "inbox[agent]").current,
            0,
            "an idle source still emits its zero row: the row set is stable"
        );
        assert_eq!(
            session.inbox.source_depths(),
            depths_before,
            "probing drained nothing"
        );

        // End the blocked worker so the test does not leak a live thread.
        for entry in workers(&session) {
            entry
                .handle
                .cancel
                .cancel(ral_core::process::CancelCause::Cancelled);
        }
    }

    /// Probing renews nothing: assembling the rows reads a running worker's
    /// `last_observed` cell without touching it.
    #[test]
    fn resource_rows_renew_no_lease() {
        let session = dressed_trunk(|shell| shell.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        session.ral("spawn { test-clear-block-forever }", 30, &emit);

        let entry = workers(&session)
            .pop()
            .expect("the spawn registered its worker");
        let before = entry.handle.last_observed();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = session
            .resource_rows()
            .expect("an identity seat never severs");
        let after = entry.handle.last_observed();
        assert_eq!(after, before, "the probe must not renew the lease");

        entry
            .handle
            .cancel
            .cancel(ral_core::process::CancelCause::Cancelled);
    }

    /// The scripted test provider's model has no pricing-catalog entry (the
    /// catalog is fetched over the network, never populated in a test
    /// process), so its window reads `None` — the fallback arm nearly every
    /// other test in this module exercises without knowing it. `log.bytes`
    /// alone carries the cap, and no `context.tokens` row appears.
    #[test]
    fn resource_rows_unknown_window_falls_back_to_capped_log_bytes() {
        let session = Avatar::for_test("system").unwrap();
        let rows = session
            .resource_rows()
            .expect("an identity seat never severs");
        let bytes = row(&rows, "log.bytes");
        assert_eq!(
            bytes.cap,
            Some(EVICT_THRESHOLD as u64),
            "an unknown window falls back to the byte threshold `evict` itself uses"
        );
        assert_eq!(bytes.policy, Policy::Evict);
        assert!(
            !rows.iter().any(|r| r.name == "context.tokens"),
            "no window means no token-pressure row to show"
        );
    }

    /// A known window is the pressure `Avatar::evict` actually fires on, so
    /// that is what the fold must show: `context.tokens` capped at
    /// `eviction_trigger(w)`, and `log.bytes` demoted to an uncapped
    /// fallback gauge rather than faking a second, unenforced ceiling.
    #[test]
    fn known_window_reports_context_tokens_not_bytes() {
        let rows = pressure_rows(Some(12_345), 4_096, Some(200_000));
        let tokens = row(&rows, "context.tokens");
        assert_eq!(tokens.current, 12_345, "the live input-token numerator");
        assert_eq!(
            tokens.cap,
            Some(eviction_trigger(200_000)),
            "the same trigger `Avatar::evict` fires auto-eviction on"
        );
        assert_eq!(tokens.policy, Policy::Evict);
        assert!(
            tokens.note.as_deref().is_some_and(|n| n.contains("200000")),
            "the note names the window the trigger was computed from"
        );

        let bytes = row(&rows, "log.bytes");
        assert_eq!(bytes.current, 4_096, "the byte gauge still reports");
        assert_eq!(
            bytes.cap, None,
            "log.bytes is no longer where the eviction pressure lives"
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
            Some(EVICT_THRESHOLD as u64),
            "falls back to the same byte threshold as an unknown window"
        );
    }
}
