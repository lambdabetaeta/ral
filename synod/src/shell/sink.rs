//! The window's own [`exarch::bus::Sink`]: projects the bus's [`Kind`] into
//! a JSON-ready [`SynodEvent`] and emits each as a `synod-event` to the
//! frontend, in place of the developer-flavoured stderr lines headless
//! writes.
//!
//! [`project`] and [`project_helper`] are the pure heart of this module —
//! total functions from a bus [`Kind`] to `Option<SynodEvent>` — so their
//! mapping is unit-testable without a running window, as is the id-and-
//! counter routing between them ([`Router`]); [`TauriSink`] is the thin,
//! untested shim that hands each routed event to `app.emit`.

use exarch::agent::event::ProviderErrorRecord;
use exarch::bus::card::{Card, Field, Hunk, Mark, Measure, Span};
use exarch::bus::{AgentId, AgentOutcome, Event, Kind, Sink};
use serde::Serialize;

/// A shadow of [`Mark`] whose byte-carrying variants (`Listing`, `Raw`) hold
/// lossy text instead of raw bytes.
///
/// A JSON array of small-integer bytes is pure waste over the wire — the
/// window only ever renders the text, never re-decodes the bytes — so the
/// conversion happens once here, at the seam, rather than in the frontend.
#[derive(Clone, Serialize)]
#[serde(tag = "mark", rename_all = "snake_case")]
pub enum MarkDto {
    Text { spans: Vec<Span> },
    Measure(Measure),
    Fields { rows: Vec<Field> },
    Diff { path: String, hunks: Vec<Hunk> },
    Listing { text: String, more: bool },
    Raw { text: String },
}

/// Convert a whole [`Card`] into its wire shape, mark by mark.
fn marks_dto(card: Card) -> Vec<MarkDto> {
    card.0
        .into_iter()
        .map(|mark| match mark {
            Mark::Text { spans } => MarkDto::Text { spans },
            Mark::Measure(m) => MarkDto::Measure(m),
            Mark::Fields { rows } => MarkDto::Fields { rows },
            Mark::Diff { path, hunks } => MarkDto::Diff { path, hunks },
            Mark::Listing { bytes, more } => MarkDto::Listing {
                text: String::from_utf8_lossy(&bytes).into_owned(),
                more,
            },
            Mark::Raw { bytes } => MarkDto::Raw {
                text: String::from_utf8_lossy(&bytes).into_owned(),
            },
        })
        .collect()
}

/// The wire shape of one exchange's worth of narration, as the window's own
/// `synod-event` listener sees it — [`project`]'s codomain, plus
/// [`Self::Failure`], which never comes from the bus.
#[derive(Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SynodEvent {
    Token {
        text: String,
    },
    Step {
        n: u32,
    },
    /// The agent's whole state, not a passing label: the status bar names this
    /// one until the next arrives, and `pending` says whether anything is
    /// outstanding — the spinner's warrant.
    State {
        label: &'static str,
        pending: bool,
    },
    ToolCall {
        tool: &'static str,
        cmd: String,
        summary: Option<String>,
    },
    HarnessCall {
        verb: &'static str,
        subject: Option<String>,
        payload: String,
        failed: bool,
    },
    Card {
        marks: Vec<MarkDto>,
    },
    /// The rendered face of a structural fact — an exec's io, a worker's
    /// settling, housekeeping, a probe.  Process, not output: the window
    /// folds it into the dial's deepest rung, never the transcript.
    ProcessCard {
        marks: Vec<MarkDto>,
    },
    /// The live helper count, folded from every `Born`/`Died` on the bus
    /// into one running number — the dial's fixed station for "how many
    /// are working", emitted only when the count actually changes.
    Helpers {
        live: u32,
    },
    /// A helper's settling, folded from `Kind::SubagentDone` — one process
    /// line in the dial's deepest rung, in the helper's own words rather
    /// than the conversation's.
    HelperDone {
        name: String,
        ok: bool,
        elapsed_secs: f64,
    },
    Usage {
        input: u64,
        output: u64,
        dollars: f64,
        unmetered: bool,
    },
    StopReason {
        reason: String,
    },
    Error {
        message: String,
    },
    ProviderError {
        record: ProviderErrorRecord,
    },
    /// A stream that broke mid-turn.  Carries the same record as
    /// [`Self::ProviderError`] and is deliberately a separate type: the window
    /// grades it below a failure that ended the exchange, because the partial
    /// reply already on screen stands and the turn resumes from it.
    Stalled {
        record: ProviderErrorRecord,
    },
    /// A conversation failure, emitted directly by `commands.rs` — never by
    /// [`project`], since no [`Kind`] carries this fact.  A
    /// `Conversation::begin` refusal is terminal, and `synod-ended` follows;
    /// a `Conversation::exchange` `Err` leaves the conversation open for the
    /// next message.
    Failure {
        message: String,
    },
}

/// Project one bus [`Kind`] into the event the window renders, or `None`
/// for a `Kind` the window has no use for.
///
/// `Step`'s `tuning` is dropped: the window narrates exchanges, not the
/// provider's effort dial.  The card-carrying kinds split by intent:
/// [`Kind::Card`] is a deliberate user-facing act and projects to `Card`,
/// which the window stands in the transcript; `Io`/`Done`/`Notice`/
/// `Resources` are raw-fact pairings whose card is a presentation of
/// process, so they collapse to `ProcessCard` and stay inside the dial.
/// Either way the raw structural fact still lands in `transcript.jsonl`;
/// only the rendering reaches the window.
pub fn project(kind: Kind) -> Option<SynodEvent> {
    Some(match kind {
        Kind::Token(text) => SynodEvent::Token { text },
        Kind::Step { n, .. } => SynodEvent::Step { n },
        Kind::State(state) => SynodEvent::State {
            label: state.label(),
            pending: state.pending(),
        },
        Kind::ToolCall { tool, cmd, summary } => SynodEvent::ToolCall { tool, cmd, summary },
        Kind::HarnessCall {
            verb,
            subject,
            payload,
            failed,
        } => SynodEvent::HarnessCall {
            verb,
            subject,
            payload,
            failed,
        },
        Kind::Card(card) => SynodEvent::Card {
            marks: marks_dto(card),
        },
        Kind::Io { card, .. }
        | Kind::Done { card, .. }
        | Kind::Notice { card, .. }
        | Kind::Resources { card, .. } => SynodEvent::ProcessCard {
            marks: marks_dto(card),
        },
        Kind::Usage(u) => SynodEvent::Usage {
            input: u.input,
            output: u.output,
            dollars: u.dollars,
            unmetered: u.unmetered,
        },
        Kind::StopReason(reason) => SynodEvent::StopReason { reason },
        Kind::Error(message) => SynodEvent::Error { message },
        Kind::ProviderError(record) => SynodEvent::ProviderError { record },
        Kind::Stalled(record) => SynodEvent::Stalled { record },
        // `Born`/`Died` are a helper's own lifecycle and `SubagentDone` its
        // settling; `Router::route` reads all three before this projection
        // ever runs, turning them into `Helpers`/`HelperDone`, so they never
        // actually reach this arm — it is listed only so this match stays
        // exhaustive over `Kind`.  `Boundary`, `Reasoning`, and
        // `UserPromptEcho` are TUI presentation blocks — the content they
        // carry already round-trips to the model on the assistant message,
        // so there is nothing here for the window to add.  A live `Thinking`
        // chunk has no block of its own here and the status bar already
        // names the wait, so only the committed reasoning matters, and that
        // reaches the transcript.  `ToolResult`/`HarnessResult` are forensic
        // pairings for their call, kept in the transcript for post-mortem,
        // never shown live.  `Pin`/`Unpin` write a TUI register slot synod
        // does not have.  `Nudge` and `SystemNote` are the harness minding
        // itself — an empty-turn correction, a truncation recovery, a
        // compaction's byte counts — named in the operator's terms, so the
        // window is not told; the state a compaction announces is its whole
        // user-facing surface.
        Kind::Born { .. }
        | Kind::Died
        | Kind::Boundary
        | Kind::Thinking(_)
        | Kind::Nudge { .. }
        | Kind::SystemNote(_)
        | Kind::Reasoning { .. }
        | Kind::UserPromptEcho(_)
        | Kind::ToolResult(_)
        | Kind::HarnessResult(_)
        | Kind::SubagentDone { .. }
        | Kind::Pin { .. }
        | Kind::Unpin { .. } => return None,
    })
}

/// The id-and-counter half of [`TauriSink`]'s routing, split out so it is
/// testable without a live [`super::commands::Emitter`] — which needs a
/// running Tauri app to build at all.
#[derive(Default)]
struct Router {
    /// The trunk's own id, learned from the very first event this router
    /// ever sees.  A fresh conversation's trunk always turns at least
    /// once — a state transition, a step — before the first `agent` call
    /// could even reach the model, so the first event a router sees is
    /// always the trunk's; nothing needs to be told which id that is.
    root: Option<AgentId>,
    /// The live helper count, re-emitted as [`SynodEvent::Helpers`] only
    /// when it changes.
    helpers_live: u32,
}

impl Router {
    /// Route one event: the trunk's own project unchanged, a helper's
    /// through [`project_helper`] — except `Born`/`Died`/`SubagentDone`,
    /// which cut across that split (a helper's own lifecycle, a result that
    /// always drains in the trunk's own loop) and are read here first.
    fn route(&mut self, e: Event) -> Option<SynodEvent> {
        let root = *self.root.get_or_insert(e.id);
        match e.kind {
            Kind::Born { .. } => Some(self.bump_helpers(1)),
            Kind::Died => Some(self.bump_helpers(-1)),
            Kind::SubagentDone {
                name,
                outcome,
                elapsed,
                ..
            } => Some(SynodEvent::HelperDone {
                name,
                ok: matches!(outcome, AgentOutcome::Complete | AgentOutcome::Empty),
                elapsed_secs: elapsed.as_secs_f64(),
            }),
            kind if e.id == root => project(kind),
            kind => project_helper(kind),
        }
    }

    fn bump_helpers(&mut self, delta: i32) -> SynodEvent {
        self.helpers_live = if delta > 0 {
            self.helpers_live + 1
        } else {
            self.helpers_live.saturating_sub(1)
        };
        SynodEvent::Helpers {
            live: self.helpers_live,
        }
    }
}

/// A helper's own projection: its prose, tool calls, and state are its own
/// business, never the conversation's — only its process facts fold into
/// the dial exactly as the trunk's do, and its usage still counts toward
/// the one bill.
fn project_helper(kind: Kind) -> Option<SynodEvent> {
    match kind {
        Kind::Usage(u) => Some(SynodEvent::Usage {
            input: u.input,
            output: u.output,
            dollars: u.dollars,
            unmetered: u.unmetered,
        }),
        Kind::Io { card, .. }
        | Kind::Done { card, .. }
        | Kind::Notice { card, .. }
        | Kind::Resources { card, .. }
        | Kind::Card(card) => Some(SynodEvent::ProcessCard {
            marks: marks_dto(card),
        }),
        _ => None,
    }
}

/// The window's [`Sink`]: routes every bus event by whether it came from the
/// conversation's own trunk or from a helper it spawned, and emits what
/// survives as `synod-event` through the worker's own gated
/// [`Emitter`](super::commands::Emitter).
pub struct TauriSink {
    emitter: super::commands::Emitter,
    router: Router,
}

impl TauriSink {
    pub fn new(emitter: super::commands::Emitter) -> Self {
        Self {
            emitter,
            router: Router::default(),
        }
    }
}

impl Sink for TauriSink {
    fn handle(&mut self, e: Event) {
        if let Some(dto) = self.router.route(e) {
            self.emitter.emit("synod-event", dto);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exarch::bus::AgentState;
    use exarch::bus::card::{DoneOutcome, Notice, Row, Seg};
    use ral_core::types::{CallSite, Observation, Observed};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn token_projects_verbatim() {
        let Some(SynodEvent::Token { text }) = project(Kind::Token("hello".to_string())) else {
            panic!("expected a Token event");
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn listing_with_invalid_utf8_projects_to_lossy_text() {
        let bytes = vec![0xff, 0xfe];
        let expected = String::from_utf8_lossy(&bytes).into_owned();
        let card = Card(vec![Mark::Listing { bytes, more: false }]);
        let Some(SynodEvent::Card { marks }) = project(Kind::Card(card)) else {
            panic!("expected a Card event");
        };
        let [MarkDto::Listing { text, more }] = marks.as_slice() else {
            panic!("expected exactly one Listing mark");
        };
        assert_eq!(*text, expected);
        assert!(!more);
    }

    #[test]
    fn raw_with_invalid_utf8_projects_to_lossy_text() {
        let bytes = vec![0xff, 0xfe];
        let expected = String::from_utf8_lossy(&bytes).into_owned();
        let card = Card(vec![Mark::Raw { bytes }]);
        let Some(SynodEvent::Card { marks }) = project(Kind::Card(card)) else {
            panic!("expected a Card event");
        };
        let [MarkDto::Raw { text }] = marks.as_slice() else {
            panic!("expected exactly one Raw mark");
        };
        assert_eq!(*text, expected);
    }

    #[test]
    fn io_done_notice_resources_all_collapse_to_process_card() {
        let io = Kind::Io {
            event: Observation::instant(
                CallSite::default(),
                String::new(),
                Observed::Read {
                    path: "f.rs".to_string(),
                },
            ),
            card: Card(vec![]),
        };
        let done = Kind::Done {
            outcome: DoneOutcome::Ok,
            card: Card(vec![]),
        };
        let notice = Kind::Notice {
            notice: Notice::Prune {
                names: vec!["x".to_string()],
                idle_calls: vec![0],
            },
            card: Card(vec![]),
        };
        let resources = Kind::Resources {
            rows: Vec::new(),
            card: Card(vec![]),
        };
        for kind in [io, done, notice, resources] {
            let Some(SynodEvent::ProcessCard { marks }) = project(kind) else {
                panic!("expected a ProcessCard event");
            };
            assert!(marks.is_empty());
        }
    }

    #[test]
    fn wire_format_pins_diff_card_and_camel_case_tool_call() {
        let card = Card(vec![Mark::Diff {
            path: "f.rs".to_string(),
            hunks: vec![Hunk {
                start: 1,
                rows: vec![Row::Del(vec![Seg::plain("x")])],
            }],
        }]);
        let event = project(Kind::Card(card)).expect("card projects");
        let value = serde_json::to_value(&event).expect("card serialises");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "card",
                "marks": [{
                    "mark": "diff",
                    "path": "f.rs",
                    "hunks": [{
                        "start": 1,
                        "rows": [{
                            "tag": "del",
                            "segs": [{ "emph": false, "text": "x" }],
                        }],
                    }],
                }],
            })
        );

        let io = project(Kind::Io {
            event: Observation::instant(
                CallSite::default(),
                String::new(),
                Observed::Read {
                    path: "f.rs".to_string(),
                },
            ),
            card: Card(vec![]),
        })
        .expect("io projects");
        let io_value = serde_json::to_value(&io).expect("io serialises");
        assert_eq!(io_value["type"], "process_card");

        let call = SynodEvent::ToolCall {
            tool: "ral",
            cmd: "ls".to_string(),
            summary: Some("list files".to_string()),
        };
        let call_value = serde_json::to_value(&call).expect("tool call serialises");
        assert_eq!(call_value["type"], "tool_call");
        assert!(call_value.get("summary").is_some());
        assert_eq!(call_value["summary"], "list files");
    }

    fn event(id: AgentId, kind: Kind) -> Event {
        Event { id, kind }
    }

    #[test]
    fn the_first_event_seen_names_the_root_whatever_its_id() {
        let mut router = Router::default();
        let Some(SynodEvent::Token { text }) = router.route(event(7, Kind::Token("hi".into())))
        else {
            panic!("the first event, from whatever id, is the root's own");
        };
        assert_eq!(text, "hi");
    }

    #[test]
    fn a_helpers_token_and_state_are_dropped_but_its_process_card_folds_in() {
        let mut router = Router::default();
        router.route(event(0, Kind::Token("root warms up".into())));

        assert!(
            router
                .route(event(1, Kind::Token("a helper's prose".into())))
                .is_none()
        );

        let done = event(
            1,
            Kind::Done {
                outcome: DoneOutcome::Ok,
                card: Card(vec![]),
            },
        );
        let Some(SynodEvent::ProcessCard { marks }) = router.route(done) else {
            panic!("a helper's structural facts still fold into the dial");
        };
        assert!(marks.is_empty());
    }

    #[test]
    fn a_helpers_usage_still_counts_toward_the_one_bill() {
        let mut router = Router::default();
        router.route(event(0, Kind::State(AgentState::Ready)));

        let usage = exarch::provider::Usage {
            input: 7,
            output: 3,
            dollars: 0.125,
            ..Default::default()
        };
        let Some(SynodEvent::Usage { input, .. }) = router.route(event(1, Kind::Usage(usage)))
        else {
            panic!("a helper's usage must still reach the window");
        };
        assert_eq!(input, 7);
    }

    #[test]
    fn born_and_died_accumulate_into_one_live_helper_count() {
        let mut router = Router::default();
        router.route(event(0, Kind::State(AgentState::Ready)));

        let born = |id: AgentId| Kind::Born {
            log_dir: PathBuf::new(),
            name: format!("helper-{id}"),
            parent: 0,
            branch: false,
        };
        let Some(SynodEvent::Helpers { live }) = router.route(event(1, born(1))) else {
            panic!("a Born must announce the live count");
        };
        assert_eq!(live, 1);
        let Some(SynodEvent::Helpers { live }) = router.route(event(2, born(2))) else {
            panic!("a second Born must announce the live count");
        };
        assert_eq!(live, 2);
        let Some(SynodEvent::Helpers { live }) = router.route(event(1, Kind::Died)) else {
            panic!("a Died must announce the live count");
        };
        assert_eq!(live, 1);
    }

    #[test]
    fn subagent_done_becomes_a_named_helper_done_whatever_its_outcome() {
        let mut router = Router::default();
        router.route(event(0, Kind::State(AgentState::Ready)));

        let done = |outcome: AgentOutcome| Kind::SubagentDone {
            name: "letters".to_string(),
            outcome,
            text: String::new(),
            elapsed: Duration::from_secs(2),
        };
        let Some(SynodEvent::HelperDone {
            name,
            ok,
            elapsed_secs,
        }) = router.route(event(0, done(AgentOutcome::Complete)))
        else {
            panic!("expected a HelperDone event");
        };
        assert_eq!(name, "letters");
        assert!(ok);
        assert!((elapsed_secs - 2.0).abs() < f64::EPSILON);

        let Some(SynodEvent::HelperDone { ok, .. }) =
            router.route(event(0, done(AgentOutcome::Failed("boom".into()))))
        else {
            panic!("expected a HelperDone event");
        };
        assert!(!ok);
    }
}
