//! Role-indexed folds exhaustively match the bus; helpers are tracked by identity.
#![deny(clippy::wildcard_enum_match_arm)]

use crate::wire::{ReadoutDto, Severity, SynodEvent, marks_dto};
use exarch::bus::Sink;
use exarch::card::{Card, change_card, observation_display_card};
use exarch::provider::ProviderError;
use exarch::record::fault::Readout;
use exarch::record::{AgentId, Born};
use exarch::record::{
    Display, Forensic, Protocol, Record, Recorded, Settlement, Transient, context_rows_card,
};
use std::collections::BTreeSet;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Trunk,
    Helper,
}

fn process_card(card: Card) -> SynodEvent {
    SynodEvent::ProcessCard {
        marks: marks_dto(card),
    }
}

fn provider_error_severity(record: &ProviderError) -> Severity {
    match record {
        ProviderError::Api { .. } | ProviderError::Overflow { .. } | ProviderError::Other(_) => {
            Severity::Bad
        }
        ProviderError::Cancelled(_)
        | ProviderError::Transient { .. }
        | ProviderError::Refused(_)
        | ProviderError::Truncated { .. } => Severity::Warn,
    }
}

/// Protocol payloads return to the provider and are never drawn.
fn project_protocol(protocol: &Protocol) -> Option<SynodEvent> {
    match protocol {
        Protocol::UserPrompt { .. }
        | Protocol::Steering { .. }
        | Protocol::System { .. }
        | Protocol::Import { .. }
        | Protocol::Inherited { .. }
        | Protocol::AssistantMessage { .. }
        | Protocol::ToolResults { .. }
        | Protocol::Evicted { .. }
        | Protocol::Rewound { .. }
        | Protocol::Cleared => None,
    }
}

fn display(role: Role, display: &Display) -> Option<SynodEvent> {
    match display {
        Display::ToolCall { .. } | Display::HarnessCall { .. } if role == Role::Helper => None,
        Display::Change { change } if role == Role::Helper => {
            Some(process_card(change_card(change)))
        }
        Display::Card { card } if role == Role::Helper => Some(process_card(card.clone())),
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
        Display::Observation { value } => observation_display_card(value).map(process_card),
        Display::Change { change } => Some(SynodEvent::Card {
            marks: marks_dto(change_card(change)),
        }),
        Display::Card { card } => Some(SynodEvent::Card {
            marks: marks_dto(card.clone()),
        }),
        Display::Context { rows } => Some(process_card(context_rows_card(rows))),
        Display::Done { .. }
        | Display::Thinking { .. }
        | Display::Answer { .. }
        | Display::Verdict { .. }
        | Display::SubagentDone { .. } => None,
    }
}

fn forensic(role: Role, forensic: &Forensic) -> Option<SynodEvent> {
    match forensic {
        Forensic::Error { .. } | Forensic::ProviderError { .. } if role == Role::Helper => None,
        // An unreported turn carries no figures; the stall readout says what happened.
        Forensic::UsageDelta { usage, .. } if usage.unreported == 0 => Some(SynodEvent::Usage {
            input: usage.input,
            output: usage.output,
            dollars: usage.dollars,
            unmetered: usage.unmetered,
        }),
        Forensic::Error { text } => Some(SynodEvent::Error {
            message: text.clone(),
        }),
        Forensic::ProviderError { error } => Some(match error.stall_cause() {
            Some(cause) => {
                SynodEvent::Stalled(ReadoutDto::new(Readout::stall(cause), Severity::Warn))
            }
            None => SynodEvent::ProviderError(ReadoutDto::new(
                Readout::fatal(error),
                provider_error_severity(error),
            )),
        }),
        Forensic::Cancelled
        | Forensic::Nudge { .. }
        | Forensic::SystemNote { .. }
        | Forensic::HarnessResult { .. }
        | Forensic::Pin { .. }
        | Forensic::Reap { .. }
        | Forensic::Prune { .. }
        | Forensic::Unpin { .. }
        | Forensic::ModelChanged { .. }
        | Forensic::WindowNarrowed { .. }
        | Forensic::SessionStarted { .. }
        | Forensic::SessionResumed { .. }
        | Forensic::SessionEnded
        | Forensic::SessionClosed
        | Forensic::Retried { .. }
        | Forensic::UsageDelta { .. }
        | Forensic::TurnStarted { .. } => None,
    }
}

fn project(role: Role, record: &Record) -> Option<SynodEvent> {
    match record {
        Record::Protocol(protocol) => project_protocol(protocol),
        Record::Display(record) => display(role, record),
        Record::Forensic(record) => forensic(role, record),
    }
}

fn transient(role: Role, t: &Transient) -> Option<SynodEvent> {
    match t {
        Transient::Token(_)
        | Transient::State(_)
        | Transient::StopReason(_)
        | Transient::Boundary
            if role == Role::Helper =>
        {
            None
        }
        Transient::Token(text) => Some(SynodEvent::Token { text: text.clone() }),
        Transient::State(state) => Some(SynodEvent::State {
            label: state.label().to_string(),
            pending: state.pending(),
        }),
        Transient::StopReason(reason) => Some(SynodEvent::StopReason {
            reason: reason.clone(),
        }),
        Transient::Resources { card, .. } => Some(process_card(card.clone())),
        // Plumbing faults have no durable copy.
        Transient::Fault { text } => Some(SynodEvent::Error {
            message: text.clone(),
        }),
        Transient::Boundary => Some(SynodEvent::Boundary),
        Transient::Thinking(_)
        | Transient::Pin { .. }
        | Transient::Unpin { .. }
        | Transient::Died => None,
    }
}

#[derive(Default)]
struct Router {
    // The trunk signals before any helper can be spawned.
    root: Option<AgentId>,
    busy: BTreeSet<AgentId>,
}

impl Router {
    fn role(&mut self, id: AgentId) -> Role {
        if id == *self.root.get_or_insert(id) {
            Role::Trunk
        } else {
            Role::Helper
        }
    }

    fn route_fact(&mut self, id: AgentId, fact: &Record) -> Option<SynodEvent> {
        match fact {
            Record::Display(Display::SubagentDone {
                name,
                outcome,
                elapsed_ms,
            }) => {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "elapsed-ms display precision; far below f64's mantissa"
                )]
                let elapsed_secs = *elapsed_ms as f64 / 1000.0;
                Some(SynodEvent::HelperDone {
                    name: name.clone(),
                    ok: *outcome == Settlement::Finished,
                    elapsed_secs,
                })
            }
            Record::Protocol(_) | Record::Display(_) | Record::Forensic(_) => {
                project(self.role(id), fact)
            }
        }
    }

    fn route_born(&mut self, id: AgentId) -> Option<SynodEvent> {
        if self.root == Some(id) {
            None
        } else {
            self.set_busy(id, true)
        }
    }

    fn route_transient(&mut self, id: AgentId, t: &Transient) -> Option<SynodEvent> {
        let role = self.role(id);
        if role == Role::Helper {
            match t {
                Transient::State(state) => return self.set_busy(id, state.pending()),
                Transient::Died => return self.set_busy(id, false),
                Transient::Token(_)
                | Transient::Thinking(_)
                | Transient::Boundary
                | Transient::StopReason(_)
                | Transient::Resources { .. }
                | Transient::Pin { .. }
                | Transient::Unpin { .. }
                | Transient::Fault { .. } => {}
            }
        }
        transient(role, t)
    }

    fn set_busy(&mut self, id: AgentId, pending: bool) -> Option<SynodEvent> {
        let changed = if pending {
            self.busy.insert(id)
        } else {
            self.busy.remove(&id)
        };
        changed.then(|| SynodEvent::Helpers {
            busy: u32::try_from(self.busy.len()).expect("busy helper count exceeds u32::MAX"),
        })
    }
}

pub(crate) struct Narrator<F: FnMut(SynodEvent)> {
    emit: F,
    router: Router,
}

impl<F: FnMut(SynodEvent)> Narrator<F> {
    pub(crate) fn new(emit: F) -> Self {
        Self {
            emit,
            router: Router::default(),
        }
    }

    fn send(&mut self, event: Option<SynodEvent>) {
        if let Some(event) = event {
            (self.emit)(event);
        }
    }
}

impl<F: FnMut(SynodEvent)> Sink for Narrator<F> {
    fn fact(&mut self, id: AgentId, rec: &Recorded<Record>) {
        let event = self.router.route_fact(id, rec.value());
        self.send(event);
    }

    fn born(&mut self, id: AgentId, _born: &Born) {
        let event = self.router.route_born(id);
        self.send(event);
    }

    fn transient(&mut self, id: AgentId, t: &Transient) {
        let event = self.router.route_transient(id, t);
        self.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::MarkDto;
    use exarch::card::DoneOutcome;
    use exarch::card::{Hunk, Mark, Row, Seg};
    use exarch::record::AgentState;
    use ral_core::data::datum::Datum as _;
    use ral_core::fact::Worker;
    use ral_core::shell::runtime::{Observation, Observed};

    fn observation_wire(what: Observed) -> ral_core::data::FOValue {
        Observation::instant(None, None, what).encode()
    }

    #[test]
    fn a_transient_token_projects_verbatim() {
        let Some(SynodEvent::Token { text }) =
            transient(Role::Trunk, &Transient::Token("hello".to_string()))
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
                old: 1,
                new: 1,
                rows: vec![Row::Del(vec![Seg::plain("x")])],
            }],
        }]);
        let record = Record::Display(Display::Card { card });
        let event = project(Role::Trunk, &record).expect("card projects");
        let value = serde_json::to_value(&event).expect("card serialises");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "card",
                "marks": [{
                    "mark": "diff",
                    "path": "f.rs",
                    "hunks": [{
                        "old": 1,
                        "new": 1,
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
        let record = Record::Display(Display::Card { card });
        let Some(SynodEvent::ProcessCard { marks }) = project(Role::Helper, &record) else {
            panic!("a helper's own deliberate card still stays inside the dial");
        };
        assert!(marks.is_empty());
    }

    #[test]
    fn raw_with_invalid_utf8_projects_to_lossy_text() {
        let bytes = vec![0xff, 0xfe];
        let expected = String::from_utf8_lossy(&bytes).into_owned();
        let card = Card(vec![Mark::Raw { bytes }]);
        let Some(SynodEvent::Card { marks }) =
            project(Role::Trunk, &Record::Display(Display::Card { card }))
        else {
            panic!("expected a Card event");
        };
        let [MarkDto::Raw { text }] = marks.as_slice() else {
            panic!("expected exactly one Raw mark");
        };
        assert_eq!(*text, expected);
    }

    #[test]
    fn context_and_a_lone_observation_both_collapse_to_process_card() {
        let context = Record::Display(Display::Context { rows: Vec::new() });
        let observation = Record::Display(Display::Observation {
            value: observation_wire(Observed::Worker(Worker {
                id: ral_core::shell::runtime::WorkerId(1),
                title: "watch build".to_string(),
            })),
        });
        for record in [context, observation] {
            let Some(SynodEvent::ProcessCard { marks }) = project(Role::Trunk, &record) else {
                panic!("expected a ProcessCard event for {record:?}");
            };
            assert!(!marks.is_empty(), "every one of these renders some ink");
        }
    }

    #[test]
    fn a_settled_background_block_reaches_the_window_not_at_all() {
        assert!(
            project(
                Role::Trunk,
                &Record::Display(Display::Done {
                    id: None,
                    title: "building".into(),
                    elapsed_ms: None,
                    outcome: DoneOutcome::Ok,
                })
            )
            .is_none()
        );
    }

    #[test]
    fn wire_format_pins_the_tool_call_shape() {
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
            router.route_transient(AgentId::new(7), &Transient::Token("hi".into()))
        else {
            panic!("the first signal, from whatever id, is the root's own");
        };
        assert_eq!(text, "hi");
    }

    #[test]
    fn a_helpers_token_and_state_are_dropped_but_its_process_card_folds_in() {
        let mut router = Router::default();
        router.route_transient(AgentId::new(0), &Transient::Token("root warms up".into()));

        assert!(
            router
                .route_transient(
                    AgentId::new(1),
                    &Transient::Token("a helper's prose".into())
                )
                .is_none()
        );

        let context = Record::Display(Display::Context { rows: Vec::new() });
        let Some(SynodEvent::ProcessCard { marks }) = router.route_fact(AgentId::new(1), &context)
        else {
            panic!("a helper's structural facts still fold into the dial");
        };
        assert!(
            !marks.is_empty(),
            "a context survey always renders some ink"
        );
    }

    #[test]
    fn an_unreported_usage_routes_to_no_event() {
        let mut router = Router::default();
        router.route_transient(AgentId::new(0), &Transient::State(AgentState::Ready));
        let usage = exarch::provider::Usage {
            unreported: 1,
            ..exarch::provider::Usage::default()
        };
        let fact = Record::Forensic(Forensic::UsageDelta {
            usage,
            generation: None,
            provider: None,
        });
        for id in [0, 1] {
            assert!(router.route_fact(AgentId::new(id), &fact).is_none());
        }
    }

    #[test]
    fn a_helpers_usage_still_counts_toward_the_one_bill() {
        let mut router = Router::default();
        router.route_transient(AgentId::new(0), &Transient::State(AgentState::Ready));

        let usage = exarch::provider::Usage {
            input: 7,
            output: 3,
            cache_creation: None,
            cache_read: None,
            dollars: 0.125,
            unmetered: false,
            unreported: 0,
        };
        let fact = Record::Forensic(Forensic::UsageDelta {
            usage,
            generation: None,
            provider: None,
        });
        let Some(SynodEvent::Usage { input, .. }) = router.route_fact(AgentId::new(1), &fact)
        else {
            panic!("a helper's usage must still reach the window");
        };
        assert_eq!(input, 7);
    }

    fn busy_count(event: Option<SynodEvent>) -> Option<u32> {
        let SynodEvent::Helpers { busy } = event? else {
            panic!("expected a Helpers event");
        };
        Some(busy)
    }

    fn ready_router() -> Router {
        let mut router = Router::default();
        router.route_transient(AgentId::new(7), &Transient::State(AgentState::Ready));
        router
    }

    #[test]
    fn births_and_deaths_count_each_helper_once() {
        let mut router = ready_router();
        let first = AgentId::new(8);
        let second = AgentId::new(9);
        assert!(router.route_born(AgentId::new(7)).is_none());
        assert_eq!(busy_count(router.route_born(first)), Some(1));
        assert!(router.route_born(first).is_none());
        assert_eq!(busy_count(router.route_born(second)), Some(2));
        assert_eq!(
            busy_count(router.route_transient(first, &Transient::Died)),
            Some(1)
        );
        assert!(router.route_transient(first, &Transient::Died).is_none());
        assert_eq!(
            busy_count(router.route_transient(second, &Transient::Died)),
            Some(0)
        );
        assert!(
            router
                .route_transient(AgentId::new(7), &Transient::Died)
                .is_none()
        );
    }

    #[test]
    fn ready_settles_a_resident_helper_before_it_dies() {
        let mut router = ready_router();
        let helper = AgentId::new(8);
        assert_eq!(busy_count(router.route_born(helper)), Some(1));
        assert_eq!(
            busy_count(router.route_transient(helper, &Transient::State(AgentState::Ready))),
            Some(0)
        );
        assert!(router.route_transient(helper, &Transient::Died).is_none());
    }

    #[test]
    fn repeated_states_emit_only_changes_to_the_busy_set() {
        let mut router = ready_router();
        let helper = AgentId::new(8);
        let ready = Transient::State(AgentState::Ready);
        assert!(router.route_transient(helper, &ready).is_none());
        for _ in 0..2 {
            assert_eq!(
                busy_count(
                    router.route_transient(helper, &Transient::State(AgentState::AwaitingModel))
                ),
                Some(1)
            );
            for state in [
                AgentState::AwaitingModel,
                AgentState::Evaluating,
                AgentState::Evicting,
                AgentState::WaitingOnAgents,
            ] {
                assert!(
                    router
                        .route_transient(helper, &Transient::State(state))
                        .is_none()
                );
            }
            assert_eq!(busy_count(router.route_transient(helper, &ready)), Some(0));
            assert!(router.route_transient(helper, &ready).is_none());
        }
    }

    #[test]
    fn two_helpers_settle_in_either_order() {
        for order in [[8, 9], [9, 8]] {
            let mut router = ready_router();
            for (id, expected) in [(8, 1), (9, 2)] {
                assert_eq!(
                    busy_count(router.route_born(AgentId::new(id))),
                    Some(expected)
                );
            }
            for (id, expected) in order.into_iter().zip([1, 0]) {
                assert_eq!(
                    busy_count(
                        router.route_transient(
                            AgentId::new(id),
                            &Transient::State(AgentState::Ready)
                        )
                    ),
                    Some(expected)
                );
                assert!(
                    router
                        .route_transient(AgentId::new(id), &Transient::Died)
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn a_second_conversation_routes_its_nonzero_root_through_the_callback() {
        for root in [0, 7] {
            let mut events = Vec::new();
            let mut narrator = Narrator::new(|event| events.push(event));
            narrator.transient(AgentId::new(root), &Transient::Token("root prose".into()));
            narrator.transient(
                AgentId::new(root + 1),
                &Transient::Token("helper prose".into()),
            );
            assert!(
                matches!(events.as_slice(), [SynodEvent::Token { text }] if text == "root prose")
            );
        }
    }

    #[test]
    fn subagent_done_becomes_a_named_helper_done_whatever_its_outcome() {
        let mut router = Router::default();
        router.route_transient(AgentId::new(0), &Transient::State(AgentState::Ready));

        let done = |outcome| {
            Record::Display(Display::SubagentDone {
                name: "letters".to_string(),
                outcome,
                elapsed_ms: 2_000,
            })
        };
        let Some(SynodEvent::HelperDone {
            name,
            ok,
            elapsed_secs,
        }) = router.route_fact(AgentId::new(0), &done(Settlement::Finished))
        else {
            panic!("expected a HelperDone event");
        };
        assert_eq!(name, "letters");
        assert!(ok);
        assert!((elapsed_secs - 2.0).abs() < f64::EPSILON);

        let Some(SynodEvent::HelperDone { ok, .. }) = router.route_fact(
            AgentId::new(0),
            &done(Settlement::Failed {
                reason: "boom".to_string(),
            }),
        ) else {
            panic!("expected a HelperDone event");
        };
        assert!(!ok);
    }

    #[test]
    fn a_seam_fault_reaches_the_window_as_an_error() {
        let mut router = Router::default();
        router.route_transient(AgentId::new(0), &Transient::State(AgentState::Ready));

        let Some(SynodEvent::Error { message }) = router.route_transient(
            AgentId::new(0),
            &Transient::Fault {
                text: "record.jsonl: permission denied".to_string(),
            },
        ) else {
            panic!("a plumbing fault must still reach the window");
        };
        assert_eq!(message, "record.jsonl: permission denied");
    }
}
