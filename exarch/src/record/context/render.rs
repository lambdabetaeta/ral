//! The provider-facing projection: the frontier as an owned message list, and
//! what it weighs.

use super::table::Item;
use super::{Context, Eviction, message_bytes, resident_messages};
use genai::chat::ChatMessage;

impl Context {
    /// The provider-facing context: the frontier in id order, each eviction as
    /// its one message and every resident turn as it lies.
    ///
    /// A turn interrupted short of a reply renders whole too — it is the work
    /// the model did, and the next prompt following its last tool results is
    /// the interruption's own mark. Nothing is synthesised in its
    /// place: a quiesce has already answered any call that never ran, so no
    /// dangling tool-call block can be here.
    pub fn rendered(&self) -> Vec<ChatMessage> {
        self.table
            .frontier()
            .iter()
            .flat_map(Item::messages)
            .collect()
    }

    /// Approximate context size in serialised bytes — the fallback eviction
    /// trigger when the model's context window is unknown. Renders no turn.
    pub(crate) fn history_bytes(&self) -> usize {
        self.table
            .frontier()
            .iter()
            .map(Item::bytes)
            .fold(0, usize::saturating_add)
    }

    /// Records still owned by the context, for the host's resource probe: the
    /// structure retains what an eviction removes, so this is not
    /// [`Self::log_len`].
    pub(crate) fn event_count(&self) -> usize {
        self.table
            .frontier()
            .iter()
            .map(|item| match item {
                Item::Turn(turn) => turn.records().len(),
                Item::Eviction(..) => 1,
            })
            .sum()
    }
}

impl Item<'_> {
    fn messages(&self) -> Vec<ChatMessage> {
        match self {
            Self::Turn(turn) => resident_messages(turn.records()),
            Self::Eviction((eviction, _)) => {
                vec![ChatMessage::user(render_eviction(eviction))]
            }
        }
    }

    /// The item's weight as sent: a turn's own sum, or its eviction's message.
    pub(super) fn bytes(&self) -> usize {
        match self {
            Self::Turn(turn) => turn.bytes,
            Self::Eviction(..) => message_bytes(&self.messages()),
        }
    }
}

/// An eviction's one message: a pure function of the record, so it is
/// byte-stable and the provider's prompt cache survives it.
///
/// In the user's voice, bracketed: the harness may state a fact about the
/// conversation, never speak in the model's own voice. The note is
/// `Debug`-quoted, so it cannot add a line.
pub(super) fn render_eviction(eviction: &Eviction) -> String {
    let Eviction { span, note, .. } = eviction;
    let turns = span.capitalised();
    let (have, they, them) = if span.is_single() {
        ("has", "It is", "it")
    } else {
        ("have", "They are", "them")
    };
    let departed = match note {
        Some(note) => format!("{turns} {have} left your context.\nYour note: {note:?}"),
        None => format!("{turns} {have} left your context in a forced eviction, without a note."),
    };
    format!(
        "[EXARCH // {departed}\n{they} still readable: exarch-transcript `read [turns: !{{range {} {}}}] reads {them} back as material, exarch-transcript `index lists every turn and every note, and exarch-transcript `grep [pattern: 're', turns: `all] searches both.]",
        span.from(),
        span.to() + 1
    )
}

/// An eviction's label in rows: the note bare, or what stands for its absence.
pub(super) fn eviction_label(eviction: &Eviction) -> String {
    eviction
        .note
        .clone()
        .unwrap_or_else(|| "forced eviction, without a note".into())
}
