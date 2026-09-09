//! The transcript door: locate the turns a read or grep names under the
//! session lock, then read them back through their [`Pointer`]s with the
//! lock gone — each record checked against its [`Locus`] digest before it
//! is parsed.

use super::state::State;
use super::{
    Body, Context, Pointer, into_chat_messages, not_recorded_refusal, not_recorded_refusal_turn,
};
use crate::agent::event::{
    GrepAnswer, GrepHit, TranscriptExchange, TranscriptMessage, TranscriptPart,
};
use crate::record::{Entry, Locus, Protocol, Record};
use genai::chat::{Binary, BinarySource, ChatMessage, ContentPart, CustomPart, ToolCall};
use regex::Regex;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// One handle on a record log, carrying the model fold's syscall-site allow.
fn open_log(path: &Path) -> io::Result<File> {
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:model-fold-pointer-read] reads record.jsonl back by Locus for a departed turn and the model's `transcript` verb alike; surfaced as a Display::HarnessCall, not the model's own data I/O"
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
            "a record did not hash to the locus that named it — the log at this \
             path is no longer the file those byte ranges were measured in; a \
             rotated segment, a copied session directory, or an edited log",
        ));
    }
    let entry: Entry = serde_json::from_slice(line).map_err(io::Error::other)?;
    match entry.record {
        Record::Protocol(p) => Ok(p),
        Record::Display(_) | Record::Forensic(_) => Err(io::Error::other(
            "a pointer named a non-protocol record — a turn's address no longer \
             names the records it was measured over",
        )),
    }
}

/// One turn's records, or the address to read them back by.
enum Sourced {
    Held(Vec<Protocol>),
    Located(Pointer),
}

/// One turn's own material, whatever became of the exchange holding it.
fn turn_messages(records: &[Protocol]) -> Vec<ChatMessage> {
    records
        .iter()
        .cloned()
        .flat_map(into_chat_messages)
        .collect()
}

/// One exchange a door read reaches: where each of its turns lies — a fork
/// can leave one exchange's turns in two files, so placement is per turn.
struct ExchangeRead {
    exchange: u64,
    turns: Vec<(u64, Sourced)>,
}

/// What a read does where a turn's file will not read back: refuse by path,
/// or pass over the turn. A narrowing answers for what it named; a search of
/// the whole transcript passes over what it cannot reach, the head marker
/// having already told the model which turns it cannot have back.
#[derive(Clone, Copy)]
enum Unreadable {
    Skip,
    Refuse,
}

/// Where every turn a door read names lies, owned outright: the read borrows
/// nothing from the structure, so the desk can drop the session lock before
/// it touches a file.
pub(crate) struct TranscriptRead {
    exchanges: Vec<ExchangeRead>,
    unreadable: Unreadable,
}

impl TranscriptRead {
    /// One [`TranscriptExchange`] per exchange the read touched, narrowed to
    /// [`TranscriptPart`]s rather than the provider's own content parts.
    ///
    /// # Errors
    /// Refuses a turn whose file will not read back.
    pub(crate) fn exchanges(self) -> Result<Vec<TranscriptExchange>, String> {
        let mut read = Vec::with_capacity(self.exchanges.len());
        self.walk(|exchange, turns| {
            read.push(TranscriptExchange {
                exchange,
                turns: turns.iter().map(|(turn, _)| *turn).collect(),
                messages: turns
                    .iter()
                    .flat_map(|(_, records)| turn_messages(records))
                    .map(|message| transcript_message(&message))
                    .collect(),
            });
        })?;
        Ok(read)
    }

    /// One hit per matching line, oldest first and bounded by [`GREP_HITS`].
    /// Searched turn by turn, so every hit names the turn it lies in.
    ///
    /// # Errors
    /// Refuses a file that will not read back.
    pub(crate) fn grep(self, pattern: &Regex) -> Result<GrepAnswer, String> {
        let mut tally = Tally::default();
        self.walk(|exchange, turns| {
            for (turn, records) in turns {
                grep_messages(
                    pattern,
                    exchange,
                    *turn,
                    &turn_messages(records),
                    &mut tally,
                );
            }
        })?;
        Ok(tally.answer())
    }

    /// Hand every located exchange's turns to `visit`, in transcript order,
    /// holding at most one open file: consecutive turns naming one log share
    /// it, so each file is read in a single pass in locus order.
    fn walk(self, mut visit: impl FnMut(u64, &[(u64, Vec<Protocol>)])) -> Result<(), String> {
        let unreadable = self.unreadable;
        let mut open: Option<(PathBuf, File)> = None;
        for ExchangeRead { exchange, turns } in self.exchanges {
            let mut read = Vec::with_capacity(turns.len());
            for (turn, sourced) in turns {
                match sourced {
                    Sourced::Held(records) => read.push((turn, records)),
                    Sourced::Located(Pointer { source, loci }) => {
                        match read_pointer(&mut open, &source, &loci) {
                            Ok(records) => read.push((turn, records)),
                            Err(error) => match unreadable {
                                Unreadable::Refuse => {
                                    return Err(read_back_refusal(turn, &source, &error));
                                }
                                Unreadable::Skip => {}
                            },
                        }
                    }
                }
            }
            visit(exchange, &read);
        }
        Ok(())
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

impl Context {
    /// Read closed turns in transcript order — the exchanges named outright,
    /// the turns a range covers, or both — as one [`TranscriptExchange`] per
    /// exchange touched, addressed by its own `exchange` field rather than by
    /// argument position.
    ///
    /// What comes back is each turn's own material, wherever the lineage
    /// keeps it: in the context, departed to this log's file, or first
    /// recorded in an ancestor's.
    ///
    /// Both halves in one call — [`Self::locate_read`] then
    /// [`TranscriptRead::exchanges`]; the desk keeps them apart so only the
    /// first runs under the session lock.
    ///
    /// # Errors
    /// Refuses a read that names nothing, a duplicate name, an exchange or
    /// range this lineage never recorded, the turn still being written, or a
    /// turn whose file will not read back.
    pub(crate) fn read_transcript(
        &self,
        exchanges: &[u64],
        turns: Option<(u64, u64)>,
    ) -> Result<Vec<TranscriptExchange>, String> {
        self.locate_read(exchanges, turns)?.exchanges()
    }

    /// [`Self::read_transcript`]'s first half: resolve every turn the read
    /// names under the caller's lock, borrowing nothing, so the read itself
    /// can run once that lock is gone.
    ///
    /// # Errors
    /// Refuses whatever [`Self::read_transcript`] refuses at location time.
    pub(crate) fn locate_read(
        &self,
        exchanges: &[u64],
        turns: Option<(u64, u64)>,
    ) -> Result<TranscriptRead, String> {
        if exchanges.is_empty() && turns.is_none() {
            return Err(
                "transcript `read` must name what to read — `exchanges: [n, …]`, \
                 `turns: [from, to]`, or both"
                    .into(),
            );
        }
        let selected = self.select_read(exchanges, turns)?;
        let mut read = Vec::with_capacity(selected.len());
        for (exchange, turns) in selected {
            let mut located = Vec::with_capacity(turns.len());
            for turn in turns {
                located.push((turn, self.sourced(turn)?));
            }
            read.push(ExchangeRead {
                exchange,
                turns: located,
            });
        }
        Ok(TranscriptRead {
            exchanges: read,
            unreadable: Unreadable::Refuse,
        })
    }

    /// Which turns a read names, grouped by exchange in transcript order: the
    /// named exchanges' own turns, and every turn a range covers.
    ///
    /// # Errors
    /// Refuses an exchange [`Self::validate_read`] refuses, a range reaching
    /// past what is recorded, and one reaching the turn still being written.
    fn select_read(
        &self,
        exchanges: &[u64],
        turns: Option<(u64, u64)>,
    ) -> Result<Vec<(u64, Vec<u64>)>, String> {
        self.validate_read(exchanges)?;
        if let Some((from, to)) = turns {
            if Some(to) > self.reach() {
                return Err(not_recorded_refusal_turn(to, self.reach()));
            }
            if let Some(open) = self
                .unclosed_turn()
                .filter(|open| (from..=to).contains(open))
                && let Some(turn) = self.turn(open)
            {
                return Err(self.live_exchange_refusal(turn.exchange));
            }
        }
        let selected = self.select(exchanges, turns);
        // A validated exchange always has turns of its own, so an empty
        // selection means the range itself began past the reach.
        match (selected.is_empty(), turns) {
            (true, Some((from, _))) => Err(not_recorded_refusal_turn(from, self.reach())),
            _ => Ok(selected),
        }
    }

    /// Every turn a door names — one whose exchange is named, or that a range
    /// covers — grouped by exchange in id order. The turn still being written
    /// is never selected: its exchange's earlier turns have closed, and it
    /// has not.
    fn select(&self, exchanges: &[u64], turns: Option<(u64, u64)>) -> Vec<(u64, Vec<u64>)> {
        let unclosed = self.unclosed_turn();
        let mut selected: Vec<(u64, Vec<u64>)> = Vec::new();
        let named = self.table.turns().iter().filter(|turn| {
            Some(turn.id) != unclosed
                && (exchanges.contains(&turn.exchange)
                    || turns.is_some_and(|(from, to)| (from..=to).contains(&turn.id)))
        });
        for turn in named {
            match selected.last_mut() {
                Some((exchange, ids)) if *exchange == turn.exchange => ids.push(turn.id),
                _ => selected.push((turn.exchange, vec![turn.id])),
            }
        }
        selected
    }

    /// The turn still being written, when the protocol is mid-exchange: the
    /// one turn no door reads back, its exchange's earlier turns having
    /// closed.
    fn unclosed_turn(&self) -> Option<u64> {
        (!matches!(self.state, State::ReadyForUser))
            .then(|| self.reach())
            .flatten()
    }

    /// The whole of "seek in the context, then the file": one turn's records
    /// where they are held, or the address to read them back by.
    ///
    /// # Errors
    /// Refuses a turn this lineage never recorded.
    fn sourced(&self, turn: u64) -> Result<Sourced, String> {
        let Some(found) = self.turn(turn) else {
            return Err(not_recorded_refusal_turn(turn, self.reach()));
        };
        Ok(match &found.body {
            Body::Here {
                records,
                origin: None,
            } => Sourced::Held(
                records
                    .iter()
                    .map(|recorded| recorded.value().clone())
                    .collect(),
            ),
            Body::Here {
                origin: Some(at), ..
            }
            | Body::There { at, .. } => Sourced::Located(at.clone()),
        })
    }

    /// Search the closed turns' text: the ones a narrowing names, or the
    /// whole transcript. Each file is read in one pass in locus order, and
    /// every hit names the turn it lies in.
    ///
    /// An unreadable turn is refused when a narrowing named it and passed
    /// over when the whole transcript was searched: the head marker has
    /// already told the model which of its turns it cannot have back.
    ///
    /// Both halves in one call — [`Self::locate_grep`] then
    /// [`TranscriptRead::grep`]; the desk keeps them apart so only the first
    /// runs under the session lock.
    ///
    /// # Errors
    /// Refuses a narrowing [`Self::validate_read`] or [`Self::select_read`]
    /// refuses, and any turn it names and cannot reach.
    pub(crate) fn grep_transcript(
        &self,
        pattern: &Regex,
        exchanges: Option<&[u64]>,
        turns: Option<(u64, u64)>,
    ) -> Result<GrepAnswer, String> {
        self.locate_grep(exchanges, turns)?.grep(pattern)
    }

    /// [`Self::grep_transcript`]'s first half: where every searchable turn
    /// lies, resolved under the caller's lock and borrowing nothing.
    ///
    /// A turn is searched when a narrowing names its exchange or covers its
    /// id; with no narrowing at all, the whole transcript is.
    ///
    /// # Errors
    /// Refuses whatever [`Self::grep_transcript`] refuses at location time.
    pub(crate) fn locate_grep(
        &self,
        exchanges: Option<&[u64]>,
        turns: Option<(u64, u64)>,
    ) -> Result<TranscriptRead, String> {
        if exchanges.is_some_and(<[u64]>::is_empty) && turns.is_none() {
            return Err(
                "transcript `grep`: `exchanges` names no exchange — omit it to search the \
                 whole transcript"
                    .into(),
            );
        }
        let narrowed = exchanges.is_some() || turns.is_some();
        let searchable = if narrowed {
            self.select_read(exchanges.unwrap_or_default(), turns)?
        } else {
            self.select(&self.every_exchange(), None)
        };
        let mut read = Vec::with_capacity(searchable.len());
        for (exchange, turns) in searchable {
            let mut located = Vec::with_capacity(turns.len());
            for turn in turns {
                located.push((turn, self.sourced(turn)?));
            }
            read.push(ExchangeRead {
                exchange,
                turns: located,
            });
        }
        Ok(TranscriptRead {
            exchanges: read,
            unreadable: if narrowed {
                Unreadable::Refuse
            } else {
                Unreadable::Skip
            },
        })
    }

    /// Every exchange the structure holds — how an unnarrowed `` `grep ``
    /// names the whole transcript.
    fn every_exchange(&self) -> Vec<u64> {
        let mut exchanges: Vec<u64> = self
            .table
            .turns()
            .iter()
            .map(|turn| turn.exchange)
            .collect();
        exchanges.dedup();
        exchanges
    }

    /// Every named exchange must be closed, named once, and one this lineage
    /// recorded.
    fn validate_read(&self, exchanges: &[u64]) -> Result<(), String> {
        let mut named = HashSet::with_capacity(exchanges.len());
        for &exchange in exchanges {
            if !named.insert(exchange) {
                return Err(format!("exchange {exchange} was named more than once"));
            }
            if self.is_live_exchange(exchange) {
                return Err(self.live_exchange_refusal(exchange));
            }
            if self.exchange_turns(exchange).is_empty() {
                return Err(not_recorded_refusal(exchange, self.reach()));
            }
        }
        Ok(())
    }

    /// The exchange still being written is refused by name — and told which
    /// of its own turns have closed, since those are readable.
    fn live_exchange_refusal(&self, exchange: u64) -> String {
        let turns = self.exchange_turns(exchange);
        let closed = &turns[..turns.len().saturating_sub(1)];
        match (closed.first(), closed.last()) {
            (Some(from), Some(to)) => format!(
                "exchange {exchange} is still in progress — its closed turns {from}–{to} are readable with `transcript `read [turns: [{from}, {to}]]`"
            ),
            _ => format!("exchange {exchange} is still in progress — no turn of it has closed yet"),
        }
    }
}

/// Hits one `` `grep `` answers with; `total` still counts them all.
const GREP_HITS: usize = 100;

/// What a hit's line is clipped to, in bytes.
const GREP_TEXT_BYTES: usize = 200;

/// Transcript order: turn, then message within it, then line within that.
type HitKey = (u64, usize, usize);

/// The oldest [`GREP_HITS`] matches in transcript order, and how many there
/// were in all — bounded, so a pattern that matches everything costs one
/// answer rather than the whole transcript.
#[derive(Default)]
struct Tally {
    hits: Vec<(HitKey, GrepHit)>,
    total: usize,
}

impl Tally {
    fn offer(&mut self, key: HitKey, hit: GrepHit) {
        self.total += 1;
        let at = self.hits.partition_point(|(seen, _)| *seen < key);
        if at < GREP_HITS {
            self.hits.insert(at, (key, hit));
            self.hits.truncate(GREP_HITS);
        }
    }

    fn answer(self) -> GrepAnswer {
        GrepAnswer {
            hits: self.hits.into_iter().map(|(_, hit)| hit).collect(),
            total: self.total,
        }
    }
}

/// One hit per matching line of one turn's messages.
fn grep_messages(
    pattern: &Regex,
    exchange: u64,
    turn: u64,
    messages: &[ChatMessage],
    tally: &mut Tally,
) {
    for (position, message) in messages.iter().enumerate() {
        for (index, line) in searched_text(message).lines().enumerate() {
            if pattern.is_match(line) {
                tally.offer(
                    (turn, position, index),
                    GrepHit {
                        exchange,
                        turn,
                        role: message.role.clone(),
                        line: index + 1,
                        text: line[..ral_core::text::floor_char_boundary(line, GREP_TEXT_BYTES)]
                            .to_string(),
                    },
                );
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
    let source = if call.fn_name == crate::shell_eval::tools::ral::NAME {
        call.fn_arguments
            .get("cmd")
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
