//! The window's own [`exarch::bus::Sink`]: folds the bus's [`Record`] and
//! [`Transient`] into a JSON-ready [`SynodEvent`] and emits each as a
//! `synod-event` to the frontend, in place of the developer-flavoured
//! stderr lines headless writes.
//!
//! [`project`], [`project_helper`], [`transient`] and [`transient_helper`]
//! are the pure heart of this module — total functions from the seam's own
//! vocabulary to `Option<SynodEvent>` — so their mapping is unit-testable
//! without a running window, as is the id-and-counter routing between them
//! ([`Router`]); [`TauriSink`] is the thin, untested shim that hands each
//! routed event to `app.emit`.
//!
//! `#![deny(clippy::wildcard_enum_match_arm)]` at the top of this module is
//! the guarantee's crate-boundary half: this fold matches `record::Record`
//! and `record::Transient` exhaustively on their own three-and-one classes,
//! so a new variant on either is a compile error here, not a silent drop.
#![deny(clippy::wildcard_enum_match_arm)]

use exarch::agent::event::ProviderErrorRecord;
use exarch::bus::card::{
    Card, Field, Hunk, Mark, Measure, Span, context_rows_card, done_card, notice_card,
    observation_display_card, observation_group_card, to_card_done, to_card_notice,
};
use exarch::bus::{AgentId, Sink};
use exarch::record::{Display, Forensic, Protocol, Record, Transient};
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

/// `Some(card)` as a wire [`SynodEvent::ProcessCard`], `None` drawing nothing
/// — the shared tail of every arm below that draws process, not output.
#[allow(
    clippy::single_option_map,
    reason = "shared by every process-card arm in both folds; inlining it back to each caller is the duplication this exists to avoid"
)]
fn process_card(card: Option<Card>) -> Option<SynodEvent> {
    card.map(|card| SynodEvent::ProcessCard {
        marks: marks_dto(card),
    })
}

/// Decode a recorded `Display::Card`'s opaque wire form back into a [`Card`]
/// — the mark tree a deliberate `surface` act recorded whole, since it is
/// its own fact rather than a rendering of one.
fn decode_card(marks: &serde_json::Value) -> Option<Card> {
    serde_json::from_value(marks.clone()).ok()
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
        tool: String,
        cmd: String,
        summary: Option<String>,
    },
    HarnessCall {
        verb: String,
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
    /// The live helper count, folded from every `Transient::Born`/`Died` on
    /// the bus into one running number — the dial's fixed station for "how
    /// many are working", emitted only when the count actually changes.
    Helpers {
        live: u32,
    },
    /// A helper's settling, folded from `Display::SubagentDone` — one
    /// process line in the dial's deepest rung, in the helper's own words
    /// rather than the conversation's.
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
    /// [`project`], since no [`Record`] carries this fact.  A
    /// `Conversation::begin` refusal is terminal, and `synod-ended` follows;
    /// a `Conversation::exchange` `Err` leaves the conversation open for the
    /// next message.
    Failure {
        message: String,
    },
}

/// `Protocol` is the model fold's own verbatim payload — a `ChatMessage`, a
/// tool-call id, a session bookend — round-tripped to the provider, never
/// drawn.  Every arm is named so a ninth class arriving here is a compile
/// error, not a silent drop.
fn project_protocol(protocol: &Protocol) -> Option<SynodEvent> {
    match protocol {
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::UserPrompt { .. }
        | Protocol::ContextMessage { .. }
        | Protocol::StepStarted { .. }
        | Protocol::AssistantMessage { .. }
        | Protocol::ToolResults { .. }
        | Protocol::ContextEdited { .. } => None,
    }
}

/// The trunk's own fold over [`Display`] — the view fold's commits.
///
/// The card-carrying arms split by intent: [`Display::Card`] is a deliberate
/// user-facing act and
/// projects to [`SynodEvent::Card`], which the window stands in the
/// transcript; the grouped and single observations, a done, a notice, and a
/// context survey are raw-fact pairings whose card is a presentation of
/// process, so they collapse to [`SynodEvent::ProcessCard`] and stay inside
/// the dial. The card itself is built here, at fold time — `Display` keeps
/// only the fact, never a mark tree it would otherwise throw away.
#[allow(clippy::match_same_arms)]
fn project_display(display: &Display) -> Option<SynodEvent> {
    match display {
        Display::ToolCall { tool, cmd, summary } => Some(SynodEvent::ToolCall {
            tool: tool.clone(),
            cmd: cmd.clone(),
            summary: summary.clone(),
        }),
        Display::HarnessCall {
            verb,
            subject,
            payload,
            failed,
        } => Some(SynodEvent::HarnessCall {
            verb: verb.clone(),
            subject: subject.clone(),
            payload: payload.clone(),
            failed: *failed,
        }),
        Display::ObservationGroup { values } => process_card(observation_group_card(values)),
        Display::Observation { value } => process_card(observation_display_card(value)),
        Display::Card { marks } => decode_card(marks).map(|card| SynodEvent::Card {
            marks: marks_dto(card),
        }),
        Display::Done { outcome } => process_card(Some(done_card(&to_card_done(outcome)))),
        Display::Notice { notice } => process_card(Some(notice_card(&to_card_notice(notice)))),
        Display::Context { rows } => process_card(Some(context_rows_card(rows))),
        Display::Step { n } => Some(SynodEvent::Step { n: *n }),
        // The trunk's committed reasoning, its prose chopped for the
        // durable scrollback, a tool result already said on its call row,
        // and a display twin of a protocol fact the screen never actually
        // drew live: none of these are narrated to the window, which draws
        // only from the live deltas and the acts above.
        Display::Thinking { .. }
        | Display::Prompt { .. }
        | Display::Answer { .. }
        | Display::Result { .. }
        | Display::ContextEdited { .. } => None,
        // Intercepted by `Router::route_fact` before this fold ever runs.
        Display::SubagentDone { .. } => None,
    }
}

/// A helper's own fold over [`Display`]: its prose, calls, and turn state are
/// its own business, never the conversation's — only its process facts fold
/// into the dial exactly as the trunk's do, and even its own deliberate
/// [`Display::Card`] collapses to [`SynodEvent::ProcessCard`] rather than
/// standing in the transcript.
#[allow(clippy::match_same_arms)]
fn project_display_helper(display: &Display) -> Option<SynodEvent> {
    match display {
        Display::ObservationGroup { values } => process_card(observation_group_card(values)),
        Display::Observation { value } => process_card(observation_display_card(value)),
        Display::Card { marks } => process_card(decode_card(marks)),
        Display::Done { outcome } => process_card(Some(done_card(&to_card_done(outcome)))),
        Display::Notice { notice } => process_card(Some(notice_card(&to_card_notice(notice)))),
        Display::Context { rows } => process_card(Some(context_rows_card(rows))),
        Display::Thinking { .. }
        | Display::Prompt { .. }
        | Display::Answer { .. }
        | Display::ToolCall { .. }
        | Display::HarnessCall { .. }
        | Display::Result { .. }
        | Display::Step { .. }
        | Display::ContextEdited { .. } => None,
        Display::SubagentDone { .. } => None,
    }
}

/// The trunk's own fold over [`Forensic`] — breadcrumbs the model fold never
/// sees.
fn project_forensic(forensic: &Forensic) -> Option<SynodEvent> {
    match forensic {
        Forensic::UsageDelta { usage } => Some(SynodEvent::Usage {
            input: usage.input,
            output: usage.output,
            dollars: usage.dollars,
            unmetered: usage.unmetered,
        }),
        Forensic::Error { text } => Some(SynodEvent::Error {
            message: text.clone(),
        }),
        Forensic::ProviderError { error } => Some(SynodEvent::ProviderError {
            record: error.clone(),
        }),
        Forensic::Stalled { error } => Some(SynodEvent::Stalled {
            record: error.clone(),
        }),
        // The harness minding itself, a forensic pairing whose act row
        // already said everything, a cancellation with nothing to pair
        // with, and a register slot the window does not have: kept for
        // post-mortem alone.
        Forensic::Cancelled
        | Forensic::Nudge { .. }
        | Forensic::SystemNote { .. }
        | Forensic::HarnessResult { .. }
        | Forensic::Pin { .. }
        | Forensic::Unpin { .. }
        | Forensic::ModelChanged { .. } => None,
    }
}

/// A helper's own fold over [`Forensic`]: its usage still counts toward the
/// one bill; every other breadcrumb is its own business.
fn project_forensic_helper(forensic: &Forensic) -> Option<SynodEvent> {
    match forensic {
        Forensic::UsageDelta { usage } => Some(SynodEvent::Usage {
            input: usage.input,
            output: usage.output,
            dollars: usage.dollars,
            unmetered: usage.unmetered,
        }),
        Forensic::Cancelled
        | Forensic::Error { .. }
        | Forensic::Nudge { .. }
        | Forensic::ProviderError { .. }
        | Forensic::Stalled { .. }
        | Forensic::SystemNote { .. }
        | Forensic::HarnessResult { .. }
        | Forensic::Pin { .. }
        | Forensic::Unpin { .. }
        | Forensic::ModelChanged { .. } => None,
    }
}

/// Fold one durable [`Record`] the trunk itself produced into the event the
/// window renders, or `None` for a record the window has no use for.
///
/// `Step`'s protocol twin's `tuning` is dropped, exactly as the `Display`
/// class that carries it already drops it: the window narrates exchanges,
/// not the provider's effort dial.
pub fn project(record: &Record) -> Option<SynodEvent> {
    match record {
        Record::Protocol(protocol) => project_protocol(protocol),
        Record::Display(display) => project_display(display),
        Record::Forensic(forensic) => project_forensic(forensic),
    }
}

/// [`project`]'s narrower twin for a record a spawned helper produced.
pub fn project_helper(record: &Record) -> Option<SynodEvent> {
    match record {
        Record::Protocol(_) => None,
        Record::Display(display) => project_display_helper(display),
        Record::Forensic(forensic) => project_forensic_helper(forensic),
    }
}

/// The trunk's own fold over a live [`Transient`] delta.
#[allow(clippy::match_same_arms)]
fn project_transient(t: &Transient) -> Option<SynodEvent> {
    match t {
        Transient::Token(text) => Some(SynodEvent::Token { text: text.clone() }),
        Transient::State(state) => Some(SynodEvent::State {
            label: state.label(),
            pending: state.pending(),
        }),
        Transient::StopReason(reason) => Some(SynodEvent::StopReason {
            reason: reason.clone(),
        }),
        Transient::Resources { card, .. } => process_card(Some(card.clone())),
        // A seam append failure or a channel elision marker: the plumbing's
        // own diagnostic, with nowhere durable to go, so the window is the
        // one place left to lose it.
        Transient::Fault { text } => Some(SynodEvent::Error {
            message: text.clone(),
        }),
        // A live reasoning chunk has no block of its own here, the streaming
        // flush boundary and a cleared segment draw nothing on their own,
        // and the live pin register is a slot the window does not have.
        Transient::Thinking(_)
        | Transient::Boundary
        | Transient::Cleared
        | Transient::Pin { .. }
        | Transient::Unpin { .. } => None,
        // Intercepted by `Router::route_transient` before this fold ever
        // runs — a helper's own lifecycle, folded into `Helpers` instead.
        Transient::Born { .. } | Transient::Died => None,
    }
}

/// A helper's own fold over a live [`Transient`] delta: its prose and turn
/// state are its own business, never the conversation's — only its process
/// facts and its own plumbing faults fold into the dial exactly as the
/// trunk's do.
#[allow(clippy::match_same_arms)]
fn project_transient_helper(t: &Transient) -> Option<SynodEvent> {
    match t {
        Transient::Resources { card, .. } => process_card(Some(card.clone())),
        Transient::Fault { text } => Some(SynodEvent::Error {
            message: text.clone(),
        }),
        Transient::Token(_)
        | Transient::Thinking(_)
        | Transient::State(_)
        | Transient::Boundary
        | Transient::StopReason(_)
        | Transient::Cleared
        | Transient::Pin { .. }
        | Transient::Unpin { .. } => None,
        Transient::Born { .. } | Transient::Died => None,
    }
}

/// The id-and-counter half of [`TauriSink`]'s routing, split out so it is
/// testable without a live [`super::commands::Emitter`] — which needs a
/// running Tauri app to build at all.
#[derive(Default)]
struct Router {
    /// The trunk's own id, learned from the very first fact or transient
    /// this router ever sees.  A fresh conversation's trunk always turns at
    /// least once — a state transition, a step — before the first `agent`
    /// call could even reach the model, so the first signal a router sees is
    /// always the trunk's; nothing needs to be told which id that is.
    root: Option<AgentId>,
    /// The live helper count, re-emitted as [`SynodEvent::Helpers`] only
    /// when it changes.
    helpers_live: u32,
}

impl Router {
    /// Route one durable fact: [`Display::SubagentDone`] cuts across the
    /// trunk/helper split — a result that always drains in the trunk's own
    /// loop — and is read here first, whoever produced it; everything else
    /// follows the id.
    fn route_fact(&mut self, id: AgentId, fact: &Record) -> Option<SynodEvent> {
        match fact {
            Record::Display(Display::SubagentDone {
                name,
                error,
                elapsed_ms,
                ..
            }) => {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "elapsed-ms display precision; far below f64's mantissa"
                )]
                let elapsed_secs = *elapsed_ms as f64 / 1000.0;
                return Some(SynodEvent::HelperDone {
                    name: name.clone(),
                    ok: error.is_none(),
                    elapsed_secs,
                });
            }
            Record::Protocol(_) | Record::Display(_) | Record::Forensic(_) => {}
        }
        let root = *self.root.get_or_insert(id);
        if id == root {
            project(fact)
        } else {
            project_helper(fact)
        }
    }

    /// Route one live delta: a helper's own [`Transient::Born`]/[`Died`] cut
    /// across the split the same way — a helper's own lifecycle, folded into
    /// the one running [`SynodEvent::Helpers`] count rather than either
    /// per-agent fold.
    fn route_transient(&mut self, id: AgentId, t: &Transient) -> Option<SynodEvent> {
        match t {
            Transient::Born { .. } => return Some(self.bump_helpers(1)),
            Transient::Died => return Some(self.bump_helpers(-1)),
            Transient::Token(_)
            | Transient::Thinking(_)
            | Transient::State(_)
            | Transient::Boundary
            | Transient::StopReason(_)
            | Transient::Cleared
            | Transient::Resources { .. }
            | Transient::Pin { .. }
            | Transient::Unpin { .. }
            | Transient::Fault { .. } => {}
        }
        let root = *self.root.get_or_insert(id);
        if id == root {
            project_transient(t)
        } else {
            project_transient_helper(t)
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

/// The window's [`Sink`]: routes every seam fact and transient by whether it
/// came from the conversation's own trunk or from a helper it spawned, and
/// emits what survives as `synod-event` through the worker's own gated
/// [`Emitter`](super::commands::Emitter).
///
/// The window draws only from [`Sink::fact`]/[`Sink::transient`] below,
/// which this impl overrides.
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

    fn send(&self, dto: Option<SynodEvent>) {
        if let Some(dto) = dto {
            self.emitter.emit("synod-event", dto);
        }
    }
}

impl Sink for TauriSink {
    fn fact(&mut self, id: AgentId, fact: &Record) {
        let dto = self.router.route_fact(id, fact);
        self.send(dto);
    }

    fn transient(&mut self, id: AgentId, t: &Transient) {
        let dto = self.router.route_transient(id, t);
        self.send(dto);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exarch::bus::AgentState;
    use exarch::bus::card::{Row, Seg};
    use exarch::record::{DoneOutcome, NoticeFact};
    use ral_core::types::{CallSite, Observation, Observed};
    use std::path::PathBuf;

    /// The wire form a `Display::ObservationGroup`/`Observation` carries —
    /// the round trip `observation_wire` and `observation_from_wire` take in
    /// the seam itself, rebuilt here without pulling the private conversion
    /// into this crate.
    fn observation_wire(what: Observed) -> ral_core::serial::FOValue {
        let observation = Observation::instant(CallSite::default(), String::new(), what);
        ral_core::serial::FOValue::try_from(&observation.to_wire())
            .expect("a test observation always scrubs to a valid FOValue")
    }

    #[test]
    fn a_transient_token_projects_verbatim() {
        let Some(SynodEvent::Token { text }) =
            project_transient(&Transient::Token("hello".to_string()))
        else {
            panic!("expected a Token event");
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn a_deliberate_card_projects_to_the_transcript() {
        let card = Card(vec![Mark::Diff {
            path: "f.rs".to_string(),
            hunks: vec![Hunk {
                start: 1,
                rows: vec![Row::Del(vec![Seg::plain("x")])],
            }],
        }]);
        let marks = serde_json::to_value(&card).expect("Card's derived Serialize cannot fail");
        let record = Record::Display(Display::Card { marks });
        let event = project(&record).expect("card projects");
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
    }

    #[test]
    fn a_helpers_own_card_collapses_to_process_card() {
        let card = Card(vec![]);
        let marks = serde_json::to_value(&card).expect("Card's derived Serialize cannot fail");
        let record = Record::Display(Display::Card { marks });
        let Some(SynodEvent::ProcessCard { marks }) = project_helper(&record) else {
            panic!("a helper's own deliberate card still stays inside the dial");
        };
        assert!(marks.is_empty());
    }

    #[test]
    fn listing_with_invalid_utf8_projects_to_lossy_text() {
        let bytes = vec![0xff, 0xfe];
        let expected = String::from_utf8_lossy(&bytes).into_owned();
        let card = Card(vec![Mark::Listing { bytes, more: false }]);
        let marks = serde_json::to_value(&card).expect("Card's derived Serialize cannot fail");
        let Some(SynodEvent::Card { marks }) = project(&Record::Display(Display::Card { marks }))
        else {
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
        let marks = serde_json::to_value(&card).expect("Card's derived Serialize cannot fail");
        let Some(SynodEvent::Card { marks }) = project(&Record::Display(Display::Card { marks }))
        else {
            panic!("expected a Card event");
        };
        let [MarkDto::Raw { text }] = marks.as_slice() else {
            panic!("expected exactly one Raw mark");
        };
        assert_eq!(*text, expected);
    }

    #[test]
    fn done_notice_context_and_a_lone_observation_all_collapse_to_process_card() {
        let done = Record::Display(Display::Done {
            outcome: DoneOutcome::Ok,
        });
        let notice = Record::Display(Display::Notice {
            notice: NoticeFact::Prune {
                names: vec!["x".to_string()],
                idle_calls: vec![0],
            },
        });
        let context = Record::Display(Display::Context { rows: Vec::new() });
        let observation = Record::Display(Display::Observation {
            value: observation_wire(Observed::Worker {
                id: ral_core::types::WorkerId(1),
                cmd: "watch build".to_string(),
                class: ral_core::types::LeaseClass::Worker,
            }),
        });
        for record in [done, notice, context, observation] {
            let Some(SynodEvent::ProcessCard { marks }) = project(&record) else {
                panic!("expected a ProcessCard event for {record:?}");
            };
            assert!(!marks.is_empty(), "every one of these renders some ink");
        }
    }

    #[test]
    fn a_grouped_run_of_reads_collapses_to_one_process_card() {
        let values = vec![
            observation_wire(Observed::Read {
                path: "a.rs".to_string(),
            }),
            observation_wire(Observed::Read {
                path: "b.rs".to_string(),
            }),
        ];
        let record = Record::Display(Display::ObservationGroup { values });
        let Some(SynodEvent::ProcessCard { marks }) = project(&record) else {
            panic!("expected a ProcessCard event");
        };
        assert!(!marks.is_empty());
    }

    #[test]
    fn wire_format_pins_camel_case_tool_call() {
        let call = SynodEvent::ToolCall {
            tool: "ral".to_string(),
            cmd: "ls".to_string(),
            summary: Some("list files".to_string()),
        };
        let call_value = serde_json::to_value(&call).expect("tool call serialises");
        assert_eq!(call_value["type"], "tool_call");
        assert!(call_value.get("summary").is_some());
        assert_eq!(call_value["summary"], "list files");
    }

    #[test]
    fn the_first_signal_seen_names_the_root_whatever_its_id() {
        let mut router = Router::default();
        let Some(SynodEvent::Token { text }) =
            router.route_transient(7, &Transient::Token("hi".into()))
        else {
            panic!("the first signal, from whatever id, is the root's own");
        };
        assert_eq!(text, "hi");
    }

    #[test]
    fn a_helpers_token_and_state_are_dropped_but_its_process_card_folds_in() {
        let mut router = Router::default();
        router.route_transient(0, &Transient::Token("root warms up".into()));

        assert!(
            router
                .route_transient(1, &Transient::Token("a helper's prose".into()))
                .is_none()
        );

        let done = Record::Display(Display::Done {
            outcome: DoneOutcome::Ok,
        });
        let Some(SynodEvent::ProcessCard { marks }) = router.route_fact(1, &done) else {
            panic!("a helper's structural facts still fold into the dial");
        };
        assert!(!marks.is_empty(), "a done outcome always renders some ink");
    }

    #[test]
    fn a_helpers_usage_still_counts_toward_the_one_bill() {
        let mut router = Router::default();
        router.route_transient(0, &Transient::State(AgentState::Ready));

        let usage = exarch::agent::event::UsageDelta {
            input: 7,
            output: 3,
            cache_creation: None,
            cache_read: None,
            dollars: 0.125,
            unmetered: false,
        };
        let fact = Record::Forensic(Forensic::UsageDelta { usage });
        let Some(SynodEvent::Usage { input, .. }) = router.route_fact(1, &fact) else {
            panic!("a helper's usage must still reach the window");
        };
        assert_eq!(input, 7);
    }

    #[test]
    fn born_and_died_accumulate_into_one_live_helper_count() {
        let mut router = Router::default();
        router.route_transient(0, &Transient::State(AgentState::Ready));

        let born = |id: AgentId| Transient::Born {
            log_dir: PathBuf::new(),
            name: format!("helper-{id}"),
            parent: 0,
            branch: false,
        };
        let Some(SynodEvent::Helpers { live }) = router.route_transient(1, &born(1)) else {
            panic!("a Born must announce the live count");
        };
        assert_eq!(live, 1);
        let Some(SynodEvent::Helpers { live }) = router.route_transient(2, &born(2)) else {
            panic!("a second Born must announce the live count");
        };
        assert_eq!(live, 2);
        let Some(SynodEvent::Helpers { live }) = router.route_transient(1, &Transient::Died) else {
            panic!("a Died must announce the live count");
        };
        assert_eq!(live, 1);
    }

    #[test]
    fn subagent_done_becomes_a_named_helper_done_whatever_its_outcome() {
        let mut router = Router::default();
        router.route_transient(0, &Transient::State(AgentState::Ready));

        let done = |error: Option<String>| {
            Record::Display(Display::SubagentDone {
                name: "letters".to_string(),
                text: String::new(),
                error,
                elapsed_ms: 2_000,
            })
        };
        let Some(SynodEvent::HelperDone {
            name,
            ok,
            elapsed_secs,
        }) = router.route_fact(0, &done(None))
        else {
            panic!("expected a HelperDone event");
        };
        assert_eq!(name, "letters");
        assert!(ok);
        assert!((elapsed_secs - 2.0).abs() < f64::EPSILON);

        let Some(SynodEvent::HelperDone { ok, .. }) =
            router.route_fact(0, &done(Some("boom".to_string())))
        else {
            panic!("expected a HelperDone event");
        };
        assert!(!ok);
    }

    #[test]
    fn a_seam_fault_reaches_the_window_as_an_error() {
        let mut router = Router::default();
        router.route_transient(0, &Transient::State(AgentState::Ready));

        let Some(SynodEvent::Error { message }) = router.route_transient(
            0,
            &Transient::Fault {
                text: "record.jsonl: permission denied".to_string(),
            },
        ) else {
            panic!("a plumbing fault must still reach the window");
        };
        assert_eq!(message, "record.jsonl: permission denied");
    }
}
