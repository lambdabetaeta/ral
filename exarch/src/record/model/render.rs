//! The provider-facing projection: the context as an owned message list,
//! what it weighs, and the marker standing at each hole the evictions left.

use super::{Context, Held, OPENING_CHARS, Turn, message_bytes, resident_messages, runs};
use genai::chat::ChatMessage;

/// The table as the provider reads it: a maximal run of departed turns is one
/// hole, and every resident turn is itself. The one walk the rendering, the
/// weight and the record count all read, so the three agree by construction.
enum Piece<'a> {
    Hole(&'a [Turn]),
    Turn(&'a Turn),
}

fn pieces(turns: &[Turn]) -> Vec<Piece<'_>> {
    let mut pieces: Vec<Piece<'_>> = Vec::new();
    let mut hole: Option<usize> = None;
    for (index, turn) in turns.iter().enumerate() {
        match (turn.is_resident(), hole) {
            (false, None) => hole = Some(index),
            (false, Some(_)) => {}
            (true, opened) => {
                if let Some(from) = opened {
                    pieces.push(Piece::Hole(&turns[from..index]));
                    hole = None;
                }
                pieces.push(Piece::Turn(turn));
            }
        }
    }
    if let Some(from) = hole {
        pieces.push(Piece::Hole(&turns[from..]));
    }
    pieces
}

impl Context {
    /// The provider-facing context: each hole's marker where its turns stood,
    /// and every resident turn as it lies, in table order.
    ///
    /// A turn interrupted short of a reply renders whole too — it is the work
    /// the model did, and the next prompt following its last tool results is
    /// the interruption's own mark. Nothing is synthesised in its
    /// place: a quiesce has already answered any call that never ran, so no
    /// dangling tool-call block can be here.
    pub fn rendered(&self) -> Vec<ChatMessage> {
        pieces(self.table.turns())
            .iter()
            .flat_map(|piece| match piece {
                Piece::Hole(hole) => vec![ChatMessage::user(render_marker(self, hole))],
                Piece::Turn(turn) => resident_messages(turn.records()),
            })
            .collect()
    }

    /// Approximate context size in serialised bytes — the fallback eviction
    /// trigger when the model's context window is unknown. Renders no turn:
    /// each marker's weight, plus each resident turn's own sum.
    pub(crate) fn history_bytes(&self) -> usize {
        pieces(self.table.turns())
            .iter()
            .map(|piece| match piece {
                Piece::Hole(hole) => message_bytes(std::slice::from_ref(&ChatMessage::user(
                    render_marker(self, hole),
                ))),
                Piece::Turn(turn) => turn.bytes,
            })
            .fold(0, usize::saturating_add)
    }

    /// Records still owned by the context, for the host's resource probe: the
    /// structure retains what an eviction removes, so this is not
    /// [`Self::log_len`].
    pub(crate) fn event_count(&self) -> usize {
        pieces(self.table.turns())
            .iter()
            .map(|piece| match piece {
                Piece::Hole(_) => 1,
                Piece::Turn(turn) => turn.records().len(),
            })
            .sum()
    }
}

/// Turns a marker draws a row each for; everything older collapses into one
/// line naming the range.
const MARKER_ROWS: usize = 40;

/// The turn-id column both a row and the collapse line are set in.
const MARKER_ID: usize = 4;

/// The column a row's role is set in.
const MARKER_ROLE: usize = 9;

/// One hole's marker: the one user-voice message standing where its turns
/// were, indexing each of them so the model can ask for any of them back.
///
/// Correct by construction: a turn's body says which cut took it, so nothing
/// here re-derives that from an id window. A pure function of the hole and
/// the structure around it — no clock, no path, no ordering-unstable
/// container — so the head hole's marker stays byte-stable between two cuts
/// made behind it, and the provider's prompt cache survives from the hole
/// forward.
///
/// In the user's voice, bracketed: the harness may state a fact about the
/// conversation, never speak in the model's own voice — a placeholder in the
/// assistant's voice is read back as something it chose to say, and imitated.
pub(super) fn render_marker(context: &Context, hole: &[Turn]) -> String {
    // Grouped by cut, so each note follows the rows it belongs to.
    let by_cut: Vec<Vec<&Turn>> = (0..context.notes().len())
        .map(|cut| {
            hole.iter()
                .filter(|turn| turn.held() == Held::Evicted { cut })
                .collect()
        })
        .collect();
    let rows: Vec<&Turn> = by_cut.iter().flatten().copied().collect();
    let ids: Vec<u64> = hole.iter().map(|turn| turn.id).collect();
    let departed = match ids.as_slice() {
        [one] => format!("Turn {one} has left your context."),
        _ => format!("Turns {} have left your context.", runs(&ids)),
    };
    let mut lines = vec![format!("[EXARCH // {departed} {}", whereabouts(hole))];
    let collapsed = rows.len().saturating_sub(MARKER_ROWS);
    if collapsed > 0 {
        let range = format!("{}–{}", rows[0].id, rows[collapsed - 1].id);
        lines.push(format!(
            "{range:>MARKER_ID$}  ({collapsed} earlier turns — exarch-transcript `index)"
        ));
    }
    let mut seen = 0usize;
    for (cut, drawn) in by_cut.iter().enumerate() {
        let start = collapsed.saturating_sub(seen).min(drawn.len());
        seen += drawn.len();
        if start >= drawn.len() {
            continue;
        }
        for turn in &drawn[start..] {
            lines.push(marker_row(turn));
        }
        // `Debug`-quoted, so no note — the model's own, or one inherited off
        // an ancestor's link — can add a line to the marker.
        if let Some(note) = context.notes()[cut].as_deref() {
            lines.push(format!("Your note: {note:?}"));
        }
    }
    format!("{}]", lines.join("\n"))
}

/// How the hole's own turns are read back: `` `read `` names them by the
/// range this hole spans, and the other two doors reach the whole
/// transcript.
fn whereabouts(hole: &[Turn]) -> String {
    let (from, past) = match (hole.first(), hole.last()) {
        (Some(first), Some(last)) => (first.id, last.id + 1),
        _ => return String::new(),
    };
    let reads = if past - from == 1 {
        format!("reads turn {from} back as material")
    } else {
        format!("reads turns {from} through {} back as material", past - 1)
    };
    format!(
        "They are still readable: `exarch-transcript `read [turns: !{{range {from} {past}}}]` {reads}, \
         `exarch-transcript `grep [pattern: 're']` searches every turn ever recorded, and \
         `exarch-transcript `index` lists them."
    )
}

fn marker_row(turn: &Turn) -> String {
    format!(
        "{:>MARKER_ID$}  {:<MARKER_ROLE$} {:<OPENING_CHARS$}{:>5} KB",
        turn.id,
        turn.role.as_str(),
        turn.label,
        turn.bytes / 1024
    )
}
