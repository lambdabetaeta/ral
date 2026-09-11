//! The provider-facing projection: the context as an owned message list,
//! what it weighs, and the head marker standing where an evicted prefix
//! was.

use super::{Context, Held, OPENING_CHARS, Turn, message_bytes, resident_messages};
use genai::chat::ChatMessage;
use std::collections::HashSet;

impl Context {
    /// The provider-facing context: the marker where a cut has left one, then
    /// every resident turn as it lies, in order.
    ///
    /// An exchange interrupted short of a reply renders whole too — it is the
    /// work the model did, and the next prompt following its last tool
    /// results is the interruption's own mark. Nothing is synthesised in its
    /// place: a quiesce has already answered any call that never ran, so no
    /// dangling tool-call block can be here.
    pub fn rendered(&self) -> Vec<ChatMessage> {
        let mut rendered: Vec<ChatMessage> = Vec::new();
        if let Some(text) = render_head(self) {
            rendered.push(ChatMessage::user(text));
        }
        for turn in self.resident() {
            rendered.extend(resident_messages(turn.records()));
        }
        rendered
    }

    /// Approximate context size in serialised bytes — the fallback eviction
    /// trigger when the model's context window is unknown. Renders no turn:
    /// the marker's weight, plus each resident turn's own sum.
    pub(crate) fn history_bytes(&self) -> usize {
        let head = render_head(self).map_or(0, |text| {
            message_bytes(std::slice::from_ref(&ChatMessage::user(text)))
        });
        self.resident()
            .fold(head, |bytes, turn| bytes.saturating_add(turn.bytes))
    }
}

/// Exchange fragments the head marker draws a row each for; everything older
/// collapses into one line naming the range.
const HEAD_ROWS: usize = 40;

/// The exchange-id column both a row and the collapse line are set in.
const HEAD_ID: usize = 4;

/// The turn-range column a row's assistant turns are set in.
const HEAD_TURNS: usize = 9;

/// One exchange's run of turns inside one cut — the row the head marker
/// draws. One exchange may appear in two fragments across two cuts.
struct Fragment {
    exchange: u64,
    /// Which eviction took these turns, indexing [`Context::notes`].
    cut: usize,
    ids: (u64, u64),
    bytes: usize,
}

impl Fragment {
    /// The assistant turns' id range, or the user turn's own id where the
    /// fragment is that turn alone.
    fn range(&self) -> String {
        let (mut from, to) = self.ids;
        if from == self.exchange && to > self.exchange {
            from = self.exchange + 1;
        }
        if from == to {
            return from.to_string();
        }
        format!("{from}–{to}")
    }
}

/// Every departed turn as the marker groups it: maximal runs of
/// table-adjacent rows sharing an exchange and a cut, in table order.
///
/// Adjacency is in the table, not among one cut's own rows: where a later cut
/// takes a survivor user turn under an exchange an earlier cut took the
/// middle of, the two are separate fragments — so each row is attributed to
/// the cut that actually took it, and no fragment spans a gap.
fn fragments(turns: &[Turn]) -> Vec<Fragment> {
    let mut fragments: Vec<Fragment> = Vec::new();
    let mut previous: Option<usize> = None;
    for (index, turn) in turns.iter().enumerate() {
        let Held::Evicted { cut } = turn.held() else {
            previous = None;
            continue;
        };
        let adjacent = previous.is_some_and(|before| before + 1 == index);
        match fragments.last_mut() {
            Some(open) if adjacent && open.cut == cut && open.exchange == turn.exchange => {
                open.ids.1 = turn.id;
                open.bytes = open.bytes.saturating_add(turn.bytes);
            }
            _ => fragments.push(Fragment {
                exchange: turn.exchange,
                cut,
                ids: (turn.id, turn.id),
                bytes: turn.bytes,
            }),
        }
        previous = Some(index);
    }
    fragments
}

/// The head marker: the one user-voice message standing where an evicted
/// prefix was, indexing every turn that has left so the model can ask for any
/// of them back. `None` when no turn has been evicted, which is the one
/// notion of "no marker" there is.
///
/// Correct by construction: a turn's body says which cut took it, so nothing
/// here re-derives that from an id window. A pure function of the structure —
/// no clock, no path, no ordering-unstable container — so between two edits
/// the provider's prompt cache sees a byte-stable message 0.
///
/// In the user's voice, bracketed: the harness may state a fact about the
/// conversation, never speak in the model's own voice — a placeholder in the
/// assistant's voice is read back as something it chose to say, and imitated.
pub(super) fn render_head(context: &Context) -> Option<String> {
    let fragments = fragments(context.table.turns());
    if fragments.is_empty() {
        return None;
    }
    // Grouped by cut, so each note follows the rows it belongs to.
    let by_cut: Vec<Vec<&Fragment>> = (0..context.notes().len())
        .map(|cut| {
            fragments
                .iter()
                .filter(|fragment| fragment.cut == cut)
                .collect()
        })
        .collect();
    let rows: Vec<&Fragment> = by_cut.iter().flatten().copied().collect();
    let departed = departed_sentence(context, &rows);
    let whereabouts = "They are still readable: `transcript `read [turns: [a, b]]` or \
                       `[exchanges: [n]]` reads them back as material, `transcript `grep \
                       [pattern: 're']` searches them all, `transcript `index` lists every \
                       turn the transcript holds.";
    let mut lines = vec![format!("[EXARCH // {departed} {whereabouts}")];
    let collapsed = rows.len().saturating_sub(HEAD_ROWS);
    if collapsed > 0 {
        let range = format!("{}–{}", rows[0].exchange, rows[collapsed - 1].exchange);
        lines.push(format!(
            "{range:>HEAD_ID$}  ({collapsed} earlier exchanges — transcript `index)"
        ));
    }
    let mut seen = 0usize;
    for (cut, drawn) in by_cut.iter().enumerate() {
        let start = collapsed.saturating_sub(seen).min(drawn.len());
        seen += drawn.len();
        if start >= drawn.len() {
            continue;
        }
        for row in &drawn[start..] {
            lines.push(head_row(context.table.turns(), row));
        }
        // `Debug`-quoted, so no note — the model's own, or one inherited off
        // an ancestor's link — can add a line to the marker.
        if let Some(note) = context.notes()[cut].as_deref() {
            lines.push(format!("Your note at eviction: {note:?}"));
        }
    }
    Some(format!("{}]", lines.join("\n")))
}

/// The marker's opening: which exchanges left whole, and which left only some
/// of their turns.
fn departed_sentence(context: &Context, rows: &[&Fragment]) -> String {
    let resident: HashSet<u64> = context.resident().map(|turn| turn.exchange).collect();
    let mut whole: Vec<u64> = Vec::new();
    let mut partial: Vec<String> = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();
    for row in rows {
        if !seen.insert(row.exchange) {
            continue;
        }
        if !resident.contains(&row.exchange) {
            whole.push(row.exchange);
            continue;
        }
        // A partially departed exchange is always a resident user turn over a
        // departed prefix of its assistant turns, so min–max is its range.
        let (from, to) = rows
            .iter()
            .filter(|other| other.exchange == row.exchange)
            .fold(row.ids, |(from, to), other| {
                (from.min(other.ids.0), to.max(other.ids.1))
            });
        partial.push(if from == to {
            format!("turn {from} of exchange {}", row.exchange)
        } else {
            format!("turns {from}–{to} of exchange {}", row.exchange)
        });
    }
    if let (Some(first), Some(last)) = (whole.first(), whole.last()) {
        let clause = if first == last {
            format!("Exchange {first} has")
        } else {
            format!("Exchanges {first}–{last} have")
        };
        let mut sentence = format!("{clause} left your context");
        if !partial.is_empty() {
            sentence += ", and ";
            sentence += &join_and(&partial);
        }
        sentence.push('.');
        return sentence;
    }
    let mut opening = join_and(&partial);
    let verb = if partial.len() == 1 && opening.starts_with("turn ") {
        "has"
    } else {
        "have"
    };
    if let Some(first) = opening.get(..1).map(str::to_uppercase) {
        opening.replace_range(..1, &first);
    }
    format!("{opening} {verb} left your context.")
}

fn join_and(phrases: &[String]) -> String {
    match phrases.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, head)) => format!("{}, and {last}", head.join(", ")),
    }
}

fn head_row(turns: &[Turn], fragment: &Fragment) -> String {
    let label = turns
        .iter()
        .find(|turn| turn.id == fragment.exchange)
        .map_or("", |turn| turn.label.as_str());
    format!(
        "{:>HEAD_ID$}  {:<OPENING_CHARS$}{:>HEAD_TURNS$} {:>5} KB",
        fragment.exchange,
        label,
        fragment.range(),
        fragment.bytes / 1024
    )
}
