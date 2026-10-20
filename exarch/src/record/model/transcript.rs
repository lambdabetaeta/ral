//! The transcript door: locate the items a read or grep names under the
//! session lock, then read turns back through their [`Pointer`]s with the
//! lock gone — each record checked against its [`Locus`] digest before it
//! is parsed.

use super::state::admissible_prefix;
use super::table::Item;
use super::{
    Body, Context, ItemKind, Linked, Location, Pointer, Span, Standing, Turn, chat_messages,
    not_recorded_refusal_turn,
};
use crate::provider::tools::{COMMAND, RAL_TOOL};
use crate::record::{
    Entry, GrepAnswer, GrepHit, Locus, Protocol, Record, Recorded, Refusal, Role,
    TranscriptMessage, TranscriptPart, TranscriptTurn, TurnKind,
};
use genai::chat::{Binary, BinarySource, ChatMessage, ContentPart, CustomPart, ToolCall};
use regex::Regex;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// One handle on a record log, carrying the model fold's syscall-site allow.
fn open_log(path: &Path) -> io::Result<File> {
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:model-fold-pointer-read] reads record.jsonl back by Locus for a departed turn and the model's `exarch-transcript` verb alike; surfaced as a Display::HarnessCall, not the model's own data I/O"
    )]
    File::open(path)
}

fn read_at(reader: &mut File, locus: &Locus) -> io::Result<Protocol> {
    let bytes = locus.bytes();
    let length = usize::try_from(bytes.end.saturating_sub(bytes.start))
        .map_err(|_| io::Error::other("a record's byte range is too large to read"))?;
    let _ = reader.seek(SeekFrom::Start(bytes.start))?;
    let mut buf = vec![0; length];
    reader.read_exact(&mut buf)?;
    let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
    // Before the parse, not after: a stale range can hold a well-formed
    // record, and refusing it as bad JSON would name the wrong fault.
    if Locus::digest_of(line) != locus.digest() {
        return Err(io::Error::other(
            "a record did not hash to the locus that named it: the log at this \
             path is no longer the file those byte ranges were measured in; a \
             copied session directory or an edited log",
        ));
    }
    let entry: Entry<Record> = serde_json::from_slice(line).map_err(io::Error::other)?;
    match entry.record {
        Record::Protocol(p) => Ok(p),
        Record::Display(_) | Record::Forensic(_) => Err(io::Error::other(
            "a pointer named a non-protocol record: a turn's address no longer \
             names the records it was measured over",
        )),
    }
}

/// Where one turn's records are: in hand, or at the address to read them
/// back by.
enum Source {
    Held(Vec<Protocol>),
    Located(Pointer),
}

/// One turn as a door read places it: whose it was, and where its records
/// lie.
struct Placed {
    id: u64,
    role: Role,
    source: Source,
}

impl Placed {
    /// The whole of "seek in the context, then the file": one turn's records
    /// where they are held, or the address to read them back by.
    fn of(turn: &Turn) -> Self {
        let source = match &turn.body {
            Body::Here {
                records,
                origin: None,
            } => Source::Held(
                records
                    .iter()
                    .map(|recorded| recorded.value().clone())
                    .collect(),
            ),
            Body::Here {
                origin: Some(at), ..
            }
            | Body::There { at, .. } => Source::Located(at.clone()),
        };
        Self {
            id: turn.id,
            role: turn.role,
            source,
        }
    }
}

/// What a search covers: a turn's material, or a residue's note.
enum Searchable {
    Turn(TurnKind, Placed),
    Note(Span, String),
}

/// One turn's own material, wherever the lineage keeps it.
fn turn_messages(records: &[Protocol]) -> Vec<ChatMessage> {
    records.iter().flat_map(chat_messages).collect()
}

/// A placed turn's material, read back through `open`: consecutive turns
/// naming one log share the one descriptor, so each file is read in a single
/// pass in locus order.
///
/// # Errors
/// Names the turn and the file that will not read back.
fn material(open: &mut Option<(PathBuf, File)>, turn: &Placed) -> Result<Vec<ChatMessage>, String> {
    match &turn.source {
        Source::Held(records) => Ok(turn_messages(records)),
        Source::Located(Pointer { source, loci }) => read_pointer(open, source, loci)
            .map(|records| turn_messages(&records))
            .map_err(|error| read_back_refusal(turn.id, source, &error)),
    }
}

/// What a search does where a turn's file will not read back: refuse by
/// path, or pass over the turn. A narrowing answers for what it named; a
/// search of the whole transcript passes over what it cannot reach.
#[derive(Clone, Copy)]
enum Unreadable {
    Skip,
    Refuse,
}

/// Where every turn a read names lies, owned outright: the read borrows
/// nothing from the structure, so the desk can drop the session lock before
/// it touches a file.
///
/// A fork can leave consecutive turns in two files, so placement is per turn.
pub(crate) struct TranscriptRead {
    turns: Vec<Placed>,
}

impl TranscriptRead {
    /// One [`TranscriptTurn`] per turn the read touched, narrowed to
    /// [`TranscriptPart`]s rather than the provider's own content parts.
    ///
    /// # Errors
    /// Refuses a turn whose file will not read back.
    pub(crate) fn turns(self) -> Result<Vec<TranscriptTurn>, String> {
        let mut open = None;
        self.turns
            .into_iter()
            .map(|turn| {
                Ok(TranscriptTurn {
                    turn: turn.id,
                    role: turn.role,
                    messages: material(&mut open, &turn)?
                        .iter()
                        .map(transcript_message)
                        .collect(),
                })
            })
            .collect()
    }
}

/// Where every item a search covers lies, in index order, owned outright as
/// [`TranscriptRead`] is.
pub(crate) struct TranscriptSearch {
    items: Vec<Searchable>,
    unreadable: Unreadable,
}

impl TranscriptSearch {
    /// One hit per matching line, in index order and bounded by
    /// [`GREP_HITS`]. Searched item by item, a note as the one line it is,
    /// so every hit names the item it lies in.
    ///
    /// # Errors
    /// Refuses a file that will not read back, where a narrowing named it.
    pub(crate) fn grep(self, pattern: &Regex) -> Result<GrepAnswer, String> {
        let Self { items, unreadable } = self;
        let mut open = None;
        let mut tally = Tally::default();
        for item in items {
            let (span, kind, messages) = match item {
                Searchable::Note(span, note) => {
                    (span, ItemKind::Residue, vec![ChatMessage::user(note)])
                }
                Searchable::Turn(kind, turn) => match material(&mut open, &turn) {
                    Ok(messages) => (Span::single(turn.id), ItemKind::Turn(kind), messages),
                    Err(refusal) => match unreadable {
                        Unreadable::Refuse => return Err(refusal),
                        Unreadable::Skip => continue,
                    },
                },
            };
            grep_messages(pattern, span, kind, &messages, &mut tally);
        }
        Ok(tally.answer())
    }
}

/// Read one turn's records off `source`, reusing the descriptor already open
/// when consecutive turns name one file — so a lineage of many files costs
/// one descriptor, never two.
fn read_pointer(
    open: &mut Option<(PathBuf, File)>,
    source: &Path,
    loci: &[Locus],
) -> io::Result<Vec<Protocol>> {
    if open
        .as_ref()
        .is_none_or(|(path, _)| path.as_path() != source)
    {
        // Closed before the next is opened.
        drop(open.take());
        let file = open_log(source)?;
        *open = Some((source.to_path_buf(), file));
    }
    let (_, file) = open.as_mut().expect("just opened, or already open");
    loci.iter().map(|locus| read_at(file, locus)).collect()
}

/// Each resident row of a link read back through its pointer, and cut to its
/// admissible prefix, so no dangling `tool_use` reaches the child's wire.
pub(super) fn read_seeds(turns: &[Linked]) -> Result<Vec<Vec<Recorded<Protocol>>>, Refusal> {
    let mut open = None;
    turns
        .iter()
        .filter(|linked| linked.row.location == Location::Resident)
        .map(|Linked { row, at }| {
            let read = read_pointer(&mut open, &at.source, &at.loci).map_err(|error| {
                Refusal::Unreadable(io::Error::other(format!(
                    "turn {} was inherited from {}, which no longer reads back ({error}); was that session's directory moved, copied or edited?",
                    row.id,
                    at.source.display()
                )))
            })?;
            let mut records: Vec<_> = at
                .loci
                .iter()
                .cloned()
                .zip(read)
                .map(|(locus, protocol)| Recorded::new(locus, protocol))
                .collect();
            records.truncate(admissible_prefix(&records));
            Ok(records)
        })
        .collect()
}

impl Context {
    /// Read the turns named, in transcript order, as one [`TranscriptTurn`]
    /// each — addressed by its own `turn` field rather than by argument
    /// position.
    ///
    /// What comes back is each turn's own material, wherever the lineage
    /// keeps it: in the context, departed to this log's file, or first
    /// recorded in an ancestor's.
    ///
    /// Both halves in one call — [`Self::locate_read`] then
    /// [`TranscriptRead::turns`] — for a test; the desk keeps them apart so
    /// only the first runs under the session lock.
    ///
    /// # Errors
    /// Refuses a read that names no turn, a turn this lineage never recorded,
    /// the turn being written now, or one whose file will not read back.
    #[cfg(test)]
    pub(crate) fn read_transcript(&self, turns: &[u64]) -> Result<Vec<TranscriptTurn>, String> {
        self.locate_read(turns)?.turns()
    }

    /// `Self::read_transcript`'s first half: resolve every turn the read
    /// names under the caller's lock, borrowing nothing, so the read itself
    /// can run once that lock is gone.
    ///
    /// # Errors
    /// Refuses whatever `Self::read_transcript` refuses at location time.
    pub(crate) fn locate_read(&self, turns: &[u64]) -> Result<TranscriptRead, String> {
        Ok(TranscriptRead {
            turns: self
                .readable(turns)?
                .into_iter()
                .filter_map(|id| self.turn(id).map(Placed::of))
                .collect(),
        })
    }

    /// The turns a read or a narrowed search names, in transcript order:
    /// deduplicated, each recorded, and none of them the one in hand.
    ///
    /// # Errors
    /// Refuses a read naming no turn, a turn never recorded, and the turn
    /// being written now.
    fn readable(&self, turns: &[u64]) -> Result<Vec<u64>, String> {
        let named: BTreeSet<u64> = turns.iter().copied().collect();
        if named.is_empty() {
            return Err("`turns` names no turn: `!{range a b}` builds a run of ids".into());
        }
        for &id in &named {
            if self.turn(id).is_none() {
                return Err(not_recorded_refusal_turn(id, self.reach()));
            }
            if self.unclosed_turn() == Some(id) {
                return Err(format!(
                    "turn {id} is being written now: it is the one turn the transcript cannot \
                     read back yet"
                ));
            }
        }
        Ok(named.into_iter().collect())
    }

    /// Search the closed turns' text and the residues' notes: the ones a
    /// narrowing names, or the whole transcript. Each file is read in one
    /// pass in locus order, and every hit names the item it lies in.
    ///
    /// An unreadable turn is refused when a narrowing named it and passed
    /// over when the whole transcript was searched.
    ///
    /// Both halves in one call — [`Self::locate_grep`] then
    /// [`TranscriptSearch::grep`] — for a test; the desk keeps them apart so
    /// only the first runs under the session lock.
    ///
    /// # Errors
    /// Refuses a narrowing `Self::read_transcript` refuses.
    #[cfg(test)]
    pub(crate) fn grep_transcript(
        &self,
        pattern: &Regex,
        turns: Option<&[u64]>,
    ) -> Result<GrepAnswer, String> {
        self.locate_grep(turns)?.grep(pattern)
    }

    /// `Self::grep_transcript`'s first half: where every searchable item
    /// lies, resolved under the caller's lock and borrowing nothing.
    ///
    /// With no narrowing at all, every turn bar the one in hand and every
    /// noted residue; with one, the turns named and every noted residue all
    /// of whose turns are named. A forced eviction's residue has no note to search.
    ///
    /// # Errors
    /// Refuses whatever `Self::grep_transcript` refuses at location time.
    pub(crate) fn locate_grep(&self, turns: Option<&[u64]>) -> Result<TranscriptSearch, String> {
        let (named, unreadable) = match turns {
            None => (None, Unreadable::Skip),
            Some([]) => {
                return Err("`turns` names no turn: omit it to search the whole transcript".into());
            }
            Some(narrowing) => (
                Some(
                    self.readable(narrowing)?
                        .into_iter()
                        .collect::<BTreeSet<u64>>(),
                ),
                Unreadable::Refuse,
            ),
        };
        let unclosed = self.unclosed_turn();
        let searched = |id: u64| match &named {
            Some(named) => named.contains(&id),
            None => Some(id) != unclosed,
        };
        let items = self
            .table
            .items()
            .into_iter()
            .filter_map(|item| match item {
                Item::Turn(turn) => {
                    searched(turn.id).then(|| Searchable::Turn(turn.kind, Placed::of(turn)))
                }
                Item::Residue(Standing { residue, .. }) => residue
                    .note
                    .clone()
                    .filter(|_| {
                        self.table
                            .turns()
                            .iter()
                            .filter(|turn| residue.span.contains(turn.id))
                            .all(|turn| searched(turn.id))
                    })
                    .map(|note| Searchable::Note(residue.span, note)),
            })
            .collect();
        Ok(TranscriptSearch { items, unreadable })
    }
}

/// Hits one `` `grep `` answers with; `total` still counts them all.
const GREP_HITS: usize = 100;

/// What a hit's line is clipped to, in bytes.
const GREP_TEXT_BYTES: usize = 200;

/// The first [`GREP_HITS`] matches, and how many there were in all —
/// bounded, so a pattern that matches everything costs one answer rather
/// than the whole transcript. Hits are offered in index order (item, then
/// message, then line), so the first kept are the first in that order.
#[derive(Default)]
struct Tally {
    hits: Vec<GrepHit>,
    total: usize,
}

impl Tally {
    fn offer(&mut self, hit: GrepHit) {
        self.total += 1;
        if self.hits.len() < GREP_HITS {
            self.hits.push(hit);
        }
    }

    fn answer(self) -> GrepAnswer {
        GrepAnswer {
            hits: self.hits,
            total: self.total,
        }
    }
}

/// One hit per matching line of one item's messages.
fn grep_messages(
    pattern: &Regex,
    span: Span,
    kind: ItemKind,
    messages: &[ChatMessage],
    tally: &mut Tally,
) {
    for message in messages {
        for (index, line) in searched_text(message).lines().enumerate() {
            if pattern.is_match(line) {
                tally.offer(GrepHit {
                    span,
                    kind,
                    role: message.role.clone(),
                    line: index + 1,
                    text: line[..line.floor_char_boundary(GREP_TEXT_BYTES)].to_string(),
                });
            }
        }
    }
}

/// What a message contributes to a search — read off the same narrowing a
/// `` `read `` answers with, so nothing is searchable that is not readable.
/// A binary payload and a provider extension carry no text a pattern could
/// mean, and contribute none.
fn searched_text(message: &ChatMessage) -> String {
    transcript_message(message)
        .parts
        .into_iter()
        .filter_map(|part| match part {
            TranscriptPart::Text(text)
            | TranscriptPart::Result(text)
            | TranscriptPart::Reasoning(text)
            | TranscriptPart::Program { source: text, .. } => Some(text),
            TranscriptPart::Binary { .. } | TranscriptPart::Custom { .. } => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A [`ChatMessage`] as material: role kept verbatim, content narrowed to
/// [`TranscriptPart`]s so a reader never meets a serialization of a Rust
/// struct standing in for content.
fn transcript_message(message: &ChatMessage) -> TranscriptMessage {
    TranscriptMessage {
        role: message.role.clone(),
        parts: message.content.iter().filter_map(transcript_part).collect(),
    }
}

/// One arm per [`ContentPart`] variant — exhaustive, so a variant genai adds
/// later is a compile error here rather than a silent blob dump.
fn transcript_part(part: &ContentPart) -> Option<TranscriptPart> {
    match part {
        ContentPart::Text(text) => Some(TranscriptPart::Text(text.clone())),
        ContentPart::ToolCall(call) => Some(transcript_program(call)),
        ContentPart::ToolResponse(response) => {
            Some(TranscriptPart::Result(response.content.clone()))
        }
        ContentPart::ReasoningContent(text) => Some(TranscriptPart::Reasoning(text.clone())),
        ContentPart::Binary(binary) => Some(transcript_binary(binary)),
        ContentPart::Custom(custom) => Some(transcript_custom(custom)),
        // An opaque provider continuation token, carrying no information of
        // its own — it produces no part.
        ContentPart::ThoughtSignature(_) => None,
    }
}

/// The tool call IS the ral program the agent ran, so for exarch's one tool
/// this carries the script source rather than the raw arguments JSON; any
/// other tool carries its name and argument keys instead of their values.
fn transcript_program(call: &ToolCall) -> TranscriptPart {
    let keys = call
        .fn_arguments
        .as_object()
        .map(|args| args.keys().cloned().collect())
        .unwrap_or_default();
    let source = if call.fn_name == RAL_TOOL {
        call.fn_arguments
            .get(COMMAND)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    } else {
        String::new()
    };
    TranscriptPart::Program {
        tool: call.fn_name.clone(),
        source,
        keys,
    }
}

fn transcript_binary(binary: &Binary) -> TranscriptPart {
    TranscriptPart::Binary {
        content_type: binary.content_type.clone(),
        name: binary.name.clone().unwrap_or_default(),
        bytes: binary_payload_bytes(&binary.source),
    }
}

/// The decoded byte length of a binary payload. A URL source names no local
/// bytes at all, so it reports zero rather than the length of the URL text.
fn binary_payload_bytes(source: &BinarySource) -> usize {
    match source {
        BinarySource::Url(_) => 0,
        BinarySource::Base64(data) => {
            let padding = data.chars().rev().take_while(|&c| c == '=').count();
            (data.len() / 4) * 3 - padding
        }
    }
}

fn transcript_custom(custom: &CustomPart) -> TranscriptPart {
    let (provider, model) = custom
        .model_iden
        .as_ref()
        .map(|iden| (iden.adapter_kind.to_string(), iden.model_name.to_string()))
        .unwrap_or_default();
    TranscriptPart::Custom { provider, model }
}

fn read_back_refusal(turn: u64, path: &Path, error: &io::Error) -> String {
    format!(
        "turn {turn} could not be read back from {}: {error}",
        path.display()
    )
}
