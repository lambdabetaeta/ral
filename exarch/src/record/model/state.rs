//! Where the protocol rests and what may follow: the sequencing automaton
//! [`Context::step`] runs, and the readings of it the agent asks for.

use super::Context;
use crate::agent::event::{QuiesceReason, ToolResult, validate_result_ids};
use crate::record::Protocol;
use genai::chat::{ChatMessage, ChatRole};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum State {
    #[default]
    ReadyForUser,
    AwaitingAssistantAfterUser,
    AwaitingToolResults {
        pending_ids: Vec<String>,
    },
    AwaitingAssistantAfterToolResults,
}

impl Context {
    /// Whether a record that opens an exchange — a fresh
    /// [`Protocol::UserPrompt`], an imported [`Protocol::ContextMessage`] —
    /// is admissible here. Weaker than [`State::ReadyForUser`]: an exchange
    /// that never reached a reply is abandoned by the next prompt rather than
    /// closed by a fabricated one, so only outstanding tool calls hold the
    /// log.
    pub fn is_ready(&self) -> bool {
        admits_new_turn(&self.state)
    }

    /// An edit may land at any rest but a batch in flight: outstanding tool
    /// calls name the assistant frame their results answer, so nothing may
    /// come between. What keeps the work in hand is
    /// [`Self::plan_eviction`]'s own shape and [`Self::validate_edit`], not
    /// this.
    pub fn can_evict(&self) -> bool {
        self.is_ready()
    }

    /// The exchange still owed a reply — one in flight, or one abandoned
    /// without ever getting there. The door's concept alone, refusing to read
    /// the exchange still being written.
    pub fn is_live_exchange(&self, id: u64) -> bool {
        !matches!(self.state, State::ReadyForUser) && self.current_exchange() == Some(id)
    }

    pub(crate) fn is_awaiting_assistant(&self) -> bool {
        matches!(
            self.state,
            State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults
        )
    }

    pub(crate) fn is_awaiting_steering(&self) -> bool {
        matches!(self.state, State::AwaitingAssistantAfterToolResults)
    }

    pub(crate) fn pending_tool_results(&self) -> Option<Vec<String>> {
        match &self.state {
            State::AwaitingToolResults { pending_ids } => Some(pending_ids.clone()),
            State::ReadyForUser
            | State::AwaitingAssistantAfterUser
            | State::AwaitingAssistantAfterToolResults => None,
        }
    }

    /// A human-readable stand-in for `{:?}` on the private [`State`] — used
    /// only in refusal messages, never matched on.
    pub(crate) fn state_description(&self) -> String {
        match &self.state {
            State::ReadyForUser => "ReadyForUser".to_string(),
            State::AwaitingAssistantAfterUser => "AwaitingAssistantAfterUser".to_string(),
            State::AwaitingToolResults { pending_ids } => {
                format!("AwaitingToolResults {{ pending_ids: {pending_ids:?} }}")
            }
            State::AwaitingAssistantAfterToolResults => {
                "AwaitingAssistantAfterToolResults".to_string()
            }
        }
    }

    /// The records a [`QuiesceReason`] quiesce still owes, for the caller to
    /// record through the seam itself — this fold only knows how to compute
    /// them, never to author them.
    pub(crate) fn quiesce_records(&self, reason: QuiesceReason) -> Vec<Protocol> {
        quiesce_records(&self.state, reason, self.next_id())
    }
}

pub(super) fn advance(state: &State, protocol: &Protocol) -> State {
    match protocol {
        Protocol::UserPrompt { .. } => State::AwaitingAssistantAfterUser,
        Protocol::AssistantMessage {
            pending_tool_ids, ..
        } if !pending_tool_ids.is_empty() => State::AwaitingToolResults {
            pending_ids: pending_tool_ids.clone(),
        },
        Protocol::AssistantMessage { .. } => State::ReadyForUser,
        Protocol::ToolResults { .. } => State::AwaitingAssistantAfterToolResults,
        Protocol::ContextMessage { .. }
        | Protocol::ContextEdited { .. }
        | Protocol::Inherited { .. } => state.clone(),
    }
}

/// Protocol sequencing legality.
pub(super) fn admissible(state: &State, protocol: &Protocol) -> bool {
    match protocol {
        Protocol::UserPrompt { .. } | Protocol::ContextMessage { .. } => admits_new_turn(state),
        Protocol::AssistantMessage { message, .. } => {
            message.role == ChatRole::Assistant
                && matches!(
                    state,
                    State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults,
                )
        }
        Protocol::ToolResults { results } => {
            if let State::AwaitingToolResults { pending_ids } = state {
                validate_result_ids(pending_ids, results).is_ok()
            } else {
                false
            }
        }
        Protocol::ContextEdited { .. }
        // Sequencing-neutral; where a link may stand is its own rule in
        // [`Context::judge`], needing more than a [`State`].
        | Protocol::Inherited { .. } => true,
    }
}

/// Whether a record that opens an exchange may follow. Only outstanding tool
/// calls forbid it: an exchange the model never replied to is abandoned by
/// the next prompt, not closed by a fabricated reply.
fn admits_new_turn(state: &State) -> bool {
    !matches!(state, State::AwaitingToolResults { .. })
}

/// The longest prefix of `records` that owes no tool result — the only shape
/// a `mnemon` seed may take, the child's own launch prompt landing behind it.
pub(super) fn admissible_prefix<'a, 'b>(records: &'b [&'a Protocol]) -> &'b [&'a Protocol] {
    let mut state = State::default();
    let mut end = 0;
    for (index, record) in records.iter().enumerate() {
        state = advance(&state, record);
        if admits_new_turn(&state) {
            end = index + 1;
        }
    }
    &records[..end]
}

/// The answer a tool call that never ran is given.  It does not name what
/// ended the exchange, because it cannot vary on it: a cancel and a `reply`
/// each answer their own batch before they quiesce, so only
/// [`QuiesceReason::Aborted`] ever reaches this.  The cause is
/// `Forensic::Cancelled`'s or `Forensic::ProviderError`'s to carry.
const UNRUN_TOOL_CALL: &str = "[EXARCH // No result: the exchange ended before this call ran.]";

/// The records a quiesce still owes.
///
/// A tool call that never ran is owed an answer whatever ended the exchange:
/// the calls were really made, "not executed" is really the answer, and a
/// dangling tool-call block is not a legal request. A [`QuiesceReason::Replied`]
/// exchange is owed a capstone besides — it ended, and only a record can say
/// so, the fold being unable to tell a reply from an interruption at the
/// resting state the two share.
///
/// Nothing else is synthesised. An exchange that stopped before any reply is
/// left exactly as it lies, and [`Context::rendered`] reads it as its note
/// once it closes, so the model never reads a turn it never took.
fn quiesce_records(state: &State, reason: QuiesceReason, turn: u64) -> Vec<Protocol> {
    let mut records = Vec::new();
    let mut state = state.clone();
    if let State::AwaitingToolResults { pending_ids } = &state {
        let results = pending_ids
            .iter()
            .map(|id| ToolResult {
                id: id.clone(),
                content: UNRUN_TOOL_CALL.into(),
            })
            .collect();
        let record = Protocol::ToolResults { results };
        state = advance(&state, &record);
        records.push(record);
    }
    if matches!(reason, QuiesceReason::Replied) && !matches!(state, State::ReadyForUser) {
        records.push(Protocol::AssistantMessage {
            turn,
            message: ChatMessage::assistant("[EXARCH // Exchange ended: replied to parent.]"),
            pending_tool_ids: Vec::new(),
            stop_reason: Some("replied".into()),
        });
    }
    records
}
