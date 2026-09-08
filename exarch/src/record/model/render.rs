//! The provider-facing projection: the context as an owned [`Rendered`],
//! what it weighs, and the head marker standing where an evicted prefix
//! was.

use super::{Context, Held, OPENING_CHARS, Turn, message_bytes, resident_messages};
use crate::record::{Protocol, Recorded};
use genai::chat::ChatMessage;
use std::collections::HashSet;

/// The context as an owned value: the messages the provider is sent, and what
/// they weigh by the structure's own per-turn sums.
#[derive(Clone, Default)]
pub struct Rendered {
    messages: Vec<ChatMessage>,
    bytes: usize,
}

impl Rendered {
    fn push(&mut self, messages: Vec<ChatMessage>, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes);
        self.messages.extend(messages);
    }

    pub fn messages(&self) -> impl Iterator<Item = &ChatMessage> {
        self.messages.iter()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Serialised size, summed from the structure's own per-turn weights.
    pub fn byte_len(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(crate) fn for_test(messages: Vec<ChatMessage>) -> Self {
        let bytes = message_bytes(&messages);
        Self { messages, bytes }
    }

    /// One harness-voice message standing for something the context does not
    /// hold: the head marker, or an abandoned exchange's note.
    fn push_note(&mut self, text: String) {
        let message = ChatMessage::user(text);
        let bytes = message_bytes(std::slice::from_ref(&message));
        self.push(vec![message], bytes);
    }
}

impl Context {
    /// The context's exchanges, each with its resident turns, in id order.
    fn resident_exchanges(&self) -> Vec<(u64, Vec<&Turn>)> {
        let mut exchanges: Vec<(u64, Vec<&Turn>)> = Vec::new();
        for turn in self.resident() {
            match exchanges.last_mut() {
                Some((exchange, turns)) if *exchange == turn.exchange => turns.push(turn),
                _ => exchanges.push((turn.exchange, vec![turn])),
            }
        }
        exchanges
    }

    /// The provider-facing context: the marker where a cut has left one, then
    /// the exchanges with a resident turn, in order.
    ///
    /// The last of them is the exchange in hand and renders as it lies,
    /// whatever state it rests in — its last turn may still be growing. Each
    /// earlier exchange renders as its turns' messages if it is settled, else
    /// as its one [`abandoned_note`].
    pub fn rendered(&self) -> Rendered {
        let mut rendered = Rendered::default();
        if let Some(text) = render_head(self) {
            rendered.push_note(text);
        }
        let exchanges = self.resident_exchanges();
        let Some((live, closed)) = exchanges.split_last() else {
            return rendered;
        };
        for (_, turns) in closed {
            if is_settled(turns) {
                for turn in turns {
                    rendered.push(resident_messages(turn.records()), turn.bytes);
                }
            } else {
                rendered.push_note(abandoned_note(turns));
            }
        }
        for turn in &live.1 {
            rendered.push(resident_messages(turn.records()), turn.bytes);
        }
        rendered
    }

    /// Approximate context size in serialised bytes — the fallback eviction
    /// trigger when the model's context window is unknown. Renders no turn:
    /// the marker's weight, plus each resident exchange's own per-turn sums,
    /// or its note's weight where the exchange never settled.
    pub(crate) fn history_bytes(&self) -> usize {
        let mut bytes = render_head(self).map_or(0, note_bytes);
        let exchanges = self.resident_exchanges();
        let Some((live, closed)) = exchanges.split_last() else {
            return bytes;
        };
        for (_, turns) in closed {
            let weight = if is_settled(turns) {
                turns.iter().map(|turn| turn.bytes).sum()
            } else {
                note_bytes(abandoned_note(turns))
            };
            bytes = bytes.saturating_add(weight);
        }
        bytes.saturating_add(live.1.iter().map(|turn| turn.bytes).sum())
    }
}

/// What a note weighs as the one message it becomes.
fn note_bytes(text: String) -> usize {
    message_bytes(std::slice::from_ref(&ChatMessage::user(text)))
}

/// Whether an exchange's own fold comes to rest: a reply that called no tool,
/// or an import, which advances nothing and so rests where it lies. An
/// interrupted exchange does not, and reads as its note.
///
/// One look at the exchange's last record, since a turn's body holds material
/// alone. An exchange whose resident turns hold nothing — a fork's link left
/// a turn the seed never re-recorded — rests: there is no interruption to
/// report about material that is not there.
fn is_settled(turns: &[&Turn]) -> bool {
    match turns
        .iter()
        .rev()
        .flat_map(|turn| turn.records().iter().rev())
        .next()
        .map(Recorded::value)
    {
        Some(Protocol::AssistantMessage {
            pending_tool_ids, ..
        }) => pending_tool_ids.is_empty(),
        Some(Protocol::ContextMessage { .. }) | None => true,
        Some(
            Protocol::UserPrompt { .. }
            | Protocol::ToolResults { .. }
            | Protocol::ContextEdited { .. }
            | Protocol::Inherited { .. },
        ) => false,
    }
}

/// What the model reads in place of an exchange that never reached a reply.
///
/// In the user's voice, never the assistant's: the harness may state a fact
/// about the conversation, but must not put words in the model's mouth — a
/// placeholder in the assistant's own voice is read back as something it chose
/// to say, and imitated. It is cause-neutral because it has to be: a cancel
/// and an abort are told apart only by a `Forensic` record, and this fold
/// projects the protocol subsequence alone.
///
/// Whether tools were called is the fact that changes what to do next, since
/// their effects outlive the context the exchange lost. "Had been called" and
/// "any effects" are both hedged deliberately: a batch answered wholly by
/// `UNRUN_TOOL_CALL` is a call made and not run.
fn abandoned_note(turns: &[&Turn]) -> String {
    let called = turns
        .iter()
        .flat_map(|turn| turn.records())
        .any(|record| matches!(record.value(), Protocol::ToolResults { .. }));
    let effects = if called {
        "Tools had been called, so any effects on the shell and filesystem stand."
    } else {
        "No tool had been called."
    };
    format!(
        "[EXARCH // An exchange here was interrupted before any reply; its content is not in your context. {effects}]"
    )
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
/// Voice and bracket as [`abandoned_note`]: the harness may state a fact
/// about the conversation, never speak in the model's own voice.
pub(super) fn render_head(context: &Context) -> Option<String> {
    let fragments = fragments(&context.turns);
    if fragments.is_empty() {
        return None;
    }
    // Grouped by cut, so each note follows the rows it belongs to.
    let by_cut: Vec<Vec<&Fragment>> = (0..context.notes.len())
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
            lines.push(head_row(&context.turns, row));
        }
        // `Debug`-quoted, so no note — the model's own, or one inherited off
        // an ancestor's link — can add a line to the marker.
        if let Some(note) = context.notes[cut].as_deref() {
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
