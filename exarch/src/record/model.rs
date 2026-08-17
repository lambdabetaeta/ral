//! The model fold: the provider-facing projection `AgentLog`, built off the
//! [`Protocol`] records of `record.jsonl` — the same `step`, whether it is
//! applied inline on the attend thread right after [`super::Emitter::emit`]
//! returns, or by [`super::replay`] from disk.
//!
//! [`Display`](super::Display) and [`Forensic`](super::Forensic) records pass
//! through untouched: this fold projects the protocol subsequence alone, so
//! its indices — and the [`Span`]s built from them — never see a display or
//! forensic record interleaved on the log between two protocol ones.
//!
//! No recorded protocol record is ever discarded: an evicted index keeps only
//! its [`Stamp`], and [`Ledger::fold_events`] reads it back through that
//! `Stamp`'s byte range one record at a time — never coalesced into a run —
//! so the from-scratch refold in [`resume`] is a bijection with the
//! incrementally maintained view, record by record.

use super::log::Log;
use super::{Fold, Protocol, Record, Recorded, Refusal, Stamp};
use crate::agent::event::{
    CompactionPlan, ContextOp, ContextSpanKind, ContextSurvey, ContextSurveyItem, QuiesceReason,
    ToolResult, summary_prompt, validate_result_ids,
};
use genai::chat::{ChatMessage, ChatRole, ToolResponse};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Marker type implementing [`Fold`] for the model view; carries no state of
/// its own; [`Memo`] is where the projection lives.
pub struct Model;

impl Fold for Model {
    type Memo = Memo;

    fn step(memo: &mut Memo, record: &Recorded<Record>) -> Result<(), Refusal> {
        match record.value() {
            Record::Protocol(p) => {
                step_protocol(memo, record.stamp().clone(), p);
                Ok(())
            }
            Record::Display(_) | Record::Forensic(_) => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    ReadyForUser,
    AwaitingAssistantAfterUser,
    AwaitingToolResults {
        pending_ids: Vec<String>,
    },
    AwaitingAssistantAfterToolResults,
}

/// A closed prefix of exchanges, folded into one summary the model reads in
/// place of the spans it replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    pub through_exchange: u64,
    pub text: String,
}

/// One exchange's (or imported context's) contiguous run of protocol records,
/// named by its exchange id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub id: u64,
    pub events: Range<usize>,
}

/// The model's addressable window onto the protocol subsequence: an optional
/// digest standing in for everything it folded, plus the closed and live
/// spans still readable by exchange id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub digest: Option<Digest>,
    pub spans: Vec<Span>,
}

/// One resident protocol record, or the [`Stamp`] to read it back by once a
/// context edit evicts its span.
#[derive(Default)]
struct Ledger {
    len: usize,
    resident: BTreeMap<usize, Recorded<Protocol>>,
    freed: BTreeMap<usize, Stamp>,
    /// `record.jsonl`'s own path, known once this fold is seeded from disk
    /// ([`resume`]) — the only thing a freed index needs to read itself back.
    source: Option<PathBuf>,
}

impl Ledger {
    fn append(&mut self, record: Recorded<Protocol>) -> usize {
        let index = self.len;
        self.len += 1;
        let _ = self.resident.insert(index, record);
        index
    }

    fn len(&self) -> usize {
        self.len
    }

    fn event(&self, index: usize) -> Option<&Protocol> {
        self.resident.get(&index).map(Recorded::value)
    }

    fn resident_events(&self, range: Range<usize>) -> Option<Vec<&Protocol>> {
        let events: Vec<&Protocol> = self
            .resident
            .range(range.clone())
            .map(|(_, recorded)| recorded.value())
            .collect();
        (events.len() == range.len()).then_some(events)
    }

    fn free(&mut self, index: usize) {
        if let Some(recorded) = self.resident.remove(&index) {
            let _ = self.freed.insert(index, recorded.stamp().clone());
        }
    }

    fn evict(&mut self, ranges: &[Range<usize>]) {
        for index in ranges.iter().cloned().flatten() {
            self.free(index);
        }
    }

    /// Every protocol record this fold has ever seen, resident or not — the
    /// freed ones read back one at a time through their own `Stamp`.
    fn fold_events(&self) -> io::Result<Vec<(usize, Protocol)>> {
        let mut events: Vec<(usize, Protocol)> = self
            .resident
            .iter()
            .map(|(index, recorded)| (*index, recorded.value().clone()))
            .collect();
        if !self.freed.is_empty() {
            let Some(path) = self.source.as_ref() else {
                return Err(io::Error::other(
                    "the freed record ledger has no readable record.jsonl",
                ));
            };
            #[allow(
                clippy::disallowed_methods,
                reason = "[io-door:silent:model-fold-freed-read] reads one evicted protocol record back off record.jsonl by its own Stamp; output infra, not turn-time data I/O"
            )]
            let mut file = File::open(path)?;
            for (index, stamp) in &self.freed {
                events.push((*index, read_freed(&mut file, stamp)?));
            }
            events.sort_unstable_by_key(|(index, _)| *index);
        }
        Ok(events)
    }
}

fn read_freed(reader: &mut File, stamp: &Stamp) -> io::Result<Protocol> {
    let bytes = stamp.bytes();
    let length = usize::try_from(bytes.end.saturating_sub(bytes.start))
        .map_err(|_| io::Error::other("a freed record range is too large to read"))?;
    let _ = reader.seek(SeekFrom::Start(bytes.start))?;
    let mut buf = vec![0; length];
    reader.read_exact(&mut buf)?;
    let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
    let entry: super::Entry = serde_json::from_slice(line).map_err(io::Error::other)?;
    match entry.record {
        Record::Protocol(p) => Ok(p),
        Record::Display(_) | Record::Forensic(_) => Err(io::Error::other(
            "a freed model-fold slot pointed at a non-protocol record — the ledger's own invariant broke",
        )),
    }
}

/// The model fold's memo: the running protocol-view projection, per-record
/// residency, and the ledger those two are built from.
#[derive(Default)]
pub struct Memo {
    ledger: Ledger,
    state: State,
    view: View,
    max_exchange: u64,
    newest_edit: Option<usize>,
}

impl Memo {
    /// Attach `record.jsonl`'s own path, so a later eviction can read its
    /// freed record back — set once, at resume, by whoever seeded this memo
    /// from disk; a live memo with no eviction ever needs it.
    pub fn attach_source(&mut self, path: PathBuf) {
        self.ledger.source = Some(path);
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::ReadyForUser)
    }

    pub fn can_compact(&self) -> bool {
        self.is_ready()
    }

    pub fn current_exchange(&self) -> Option<u64> {
        (self.max_exchange != 0).then_some(self.max_exchange)
    }

    pub fn last_view_exchange(&self) -> Option<u64> {
        self.view.spans.last().map(|span| span.id).or_else(|| {
            self.view
                .digest
                .as_ref()
                .map(|digest| digest.through_exchange)
        })
    }

    pub fn log_len(&self) -> usize {
        self.ledger.len()
    }

    pub fn token_measure_is_stale(&self, measured_at: usize) -> bool {
        self.newest_edit.is_some_and(|index| index >= measured_at)
    }

    pub fn is_live_exchange(&self, id: u64) -> bool {
        !self.is_ready() && self.max_exchange == id
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

    /// The stub records a [`QuiesceReason`] quiesce needs to drive the state
    /// back to ready, for the caller to record through the seam itself —
    /// this fold only knows how to compute them, never to author them.
    pub(crate) fn quiesce_stubs(&self, reason: QuiesceReason) -> Vec<Protocol> {
        stubs_to_ready(&self.state, reason)
    }

    /// The parent context a `mnemon` child inherits. A spawn from inside a
    /// tool batch leaves the parent waiting on the very call that made the
    /// child, so that unfinished assistant frame is dropped: the child
    /// starts from the request context, not a dangling tool protocol.
    pub(crate) fn inherited_context_messages(&self) -> Vec<ChatMessage> {
        let omit_tail_assistant = matches!(self.state, State::AwaitingToolResults { .. });
        self.project_view(omit_tail_assistant)
    }

    /// The provider-facing message list — recomputed from the protocol
    /// subsequence on every call, never accumulator state: `span_messages`'s
    /// `omit`/`repair_end` flags depend on which span is currently last, so
    /// they must be free to change retroactively when an edit removes the
    /// tail.
    pub fn model_messages(&self) -> Vec<ChatMessage> {
        self.project_view(false)
    }

    fn project_view(&self, omit_tail_assistant: bool) -> Vec<ChatMessage> {
        let spans = &self.view.spans;
        let mut messages = Vec::new();
        if let Some(digest) = &self.view.digest {
            messages.push(ChatMessage::user(summary_prompt(&digest.text)));
        }
        for (index, span) in spans.iter().enumerate() {
            let omit = omit_tail_assistant && index + 1 == spans.len();
            let repair_end = index + 1 < spans.len() && !omit;
            messages.extend(self.span_messages(span, omit, repair_end));
        }
        messages
    }

    fn span_messages(
        &self,
        span: &Span,
        omit_tail_assistant: bool,
        repair_end: bool,
    ) -> Vec<ChatMessage> {
        let end = span.events.end - usize::from(omit_tail_assistant);
        let events = self
            .ledger
            .resident_events(span.events.start..end)
            .expect("view spans are resident in the ledger");
        if matches!(events.first(), Some(Protocol::ContextMessage { .. })) {
            return events
                .into_iter()
                .cloned()
                .flat_map(into_chat_messages)
                .collect();
        }

        let mut state = State::default();
        let mut messages = Vec::new();
        for event in events {
            if !admissible(&state, event) && stub_repairable(event) {
                append_aborted_stubs(&mut state, &mut messages);
            }
            messages.extend(into_chat_messages(event.clone()));
            state = advance(&state, event);
        }
        if repair_end && !matches!(state, State::ReadyForUser) {
            append_aborted_stubs(&mut state, &mut messages);
        }
        messages
    }

    fn span_message_bytes(&self, span: &Span) -> usize {
        self.span_message_bytes_with_repair(span, true)
    }

    fn span_message_bytes_with_repair(&self, span: &Span, repair_end: bool) -> usize {
        self.span_messages(span, false, repair_end)
            .iter()
            .map(|message| serde_json::to_string(message).map_or(0, |s| s.len()))
            .sum()
    }

    fn messages_for_spans(&self, spans: &[Span]) -> Vec<ChatMessage> {
        let mut messages = Vec::new();
        if let Some(digest) = &self.view.digest {
            messages.push(ChatMessage::user(summary_prompt(&digest.text)));
        }
        for span in spans {
            messages.extend(self.span_messages(span, false, true));
        }
        messages
    }

    /// The window every read query of the model view answers over — private
    /// to the fold; `AgentLog` reads the shape it needs through the queries
    /// below, never this reference directly.
    pub fn view(&self) -> &View {
        &self.view
    }

    fn digest_reach(&self) -> Option<u64> {
        self.view
            .digest
            .as_ref()
            .map(|digest| digest.through_exchange)
    }

    /// Slots still owned by the model view: the append-only ledger retains
    /// what an edit removes, so this is the host's resource probe's own
    /// count, not [`Self::log_len`].
    pub(crate) fn event_count(&self) -> usize {
        let events = self
            .view
            .spans
            .iter()
            .map(|span| span.events.len())
            .sum::<usize>();
        events + usize::from(self.view.digest.is_some())
    }

    /// Approximate context size in serialised model-view bytes — the
    /// fallback compaction trigger when the model's context window is
    /// unknown.
    pub(crate) fn history_bytes(&self) -> usize {
        self.model_messages()
            .iter()
            .map(|m| serde_json::to_string(m).map_or(0, |s| s.len()))
            .sum()
    }

    /// # Panics
    /// Panics if a view span is not resident in the ledger.
    pub(crate) fn context_survey(&self) -> ContextSurvey {
        let mut survey = ContextSurvey::default();
        if let Some(digest) = &self.view.digest {
            let item = ContextSurveyItem {
                exchange: digest.through_exchange,
                kind: ContextSpanKind::Digest,
                opening: opening_line(&digest.text),
                bytes: serde_json::to_string(&ChatMessage::user(summary_prompt(&digest.text)))
                    .map_or(0, |text| text.len()),
                steps: 0,
                live: false,
            };
            survey.add(item);
        }
        for span in &self.view.spans {
            let events = self
                .ledger
                .resident_events(span.events.clone())
                .expect("view spans are resident in the ledger");
            let kind = match events.first() {
                Some(Protocol::ContextMessage { .. }) => ContextSpanKind::Import,
                _ => ContextSpanKind::Exchange,
            };
            let live = self.is_live_exchange(span.id);
            let item = ContextSurveyItem {
                exchange: span.id,
                kind,
                opening: opening_line_for_events(&events),
                bytes: self.span_message_bytes_with_repair(span, !live),
                steps: events
                    .iter()
                    .filter(|event| matches!(event, Protocol::StepStarted { .. }))
                    .count(),
                live,
            };
            survey.add(item);
        }
        survey
    }

    /// Read named, closed spans in their model-view order.
    ///
    /// # Errors
    /// Refuses an empty list, a duplicate name, a missing or folded exchange,
    /// or the exchange still in progress.
    pub(crate) fn read_context(&self, exchanges: &[u64]) -> Result<String, String> {
        if exchanges.is_empty() {
            return Err("context-read must name at least one exchange".into());
        }
        self.validate_named_exchanges(exchanges)?;
        Ok(render_context_transcript(
            &self.ledger,
            &self.view,
            &exchanges.iter().copied().collect(),
        ))
    }

    /// Resolve a user rewind into the whole visible suffix beginning at its
    /// anchor. The anchor is checked before the suffix is derived.
    ///
    /// # Errors
    /// Refuses an absent anchor or one strictly inside the current digest.
    pub(crate) fn rewind_exchanges(&self, anchor: u64) -> Result<Vec<u64>, String> {
        if let Some(reach) = self.digest_reach() {
            if anchor < reach {
                return Err(folded_exchange_refusal(anchor, reach));
            }
            if anchor == reach {
                let mut exchanges = vec![anchor];
                exchanges.extend(
                    self.view
                        .spans
                        .iter()
                        .filter(|span| span.id >= anchor)
                        .map(|span| span.id),
                );
                return Ok(exchanges);
            }
        }
        if !self.view.spans.iter().any(|span| span.id == anchor) {
            return Err(rewind_unknown_exchange_refusal(
                anchor,
                self.last_view_exchange(),
            ));
        }
        Ok(self
            .view
            .spans
            .iter()
            .filter(|span| span.id >= anchor)
            .map(|span| span.id)
            .collect())
    }

    /// # Errors
    /// Refuses an unnamed edit, an unaddressable target, or a live exchange.
    pub(crate) fn validate_edit(&self, op: &ContextOp) -> Result<(), String> {
        match op {
            ContextOp::Fold {
                through_exchange, ..
            } => self.validate_closed_present(*through_exchange),
            ContextOp::Drop { exchanges } => {
                if exchanges.is_empty() {
                    return Err("a context edit must name at least one exchange".into());
                }
                self.validate_named_exchanges(exchanges)
            }
        }
    }

    /// Each named exchange must be addressable and closed, and named once.
    fn validate_named_exchanges(&self, exchanges: &[u64]) -> Result<(), String> {
        let mut named = HashSet::with_capacity(exchanges.len());
        for &exchange in exchanges {
            if !named.insert(exchange) {
                return Err(format!("exchange {exchange} was named more than once"));
            }
            self.validate_closed_present(exchange)?;
        }
        Ok(())
    }

    /// An exchange is addressable iff it survives in the view — as a span, or
    /// as the digest named by its reach — and closed iff it is not the live
    /// one.
    fn validate_closed_present(&self, exchange: u64) -> Result<(), String> {
        if let Some(reach) = self.digest_reach() {
            if exchange < reach {
                return Err(folded_exchange_refusal(exchange, reach));
            }
            if exchange == reach {
                return Ok(());
            }
        }
        if !self.view.spans.iter().any(|span| span.id == exchange) {
            return Err(unknown_exchange_refusal(exchange));
        }
        if self.is_live_exchange(exchange) {
            return Err(live_exchange_refusal(exchange));
        }
        Ok(())
    }

    pub(crate) fn plan_compaction(
        &self,
        keep_budget_bytes: usize,
        before_exchange: Option<u64>,
    ) -> Option<CompactionPlan> {
        let spans = &self.view.spans;
        let cap = before_exchange.map(|id| {
            spans
                .iter()
                .position(|span| span.id >= id)
                .unwrap_or(spans.len())
        });
        let mut suffix_start = cap.unwrap_or(spans.len());
        if suffix_start == 0 {
            return None;
        }

        let mut suffix_bytes = spans[suffix_start..]
            .iter()
            .map(|span| self.span_message_bytes(span))
            .sum::<usize>();
        for index in (0..suffix_start).rev() {
            let bytes = self.span_message_bytes(&spans[index]);
            let Some(total) = suffix_bytes.checked_add(bytes) else {
                break;
            };
            if total > keep_budget_bytes {
                break;
            }
            suffix_bytes = total;
            suffix_start = index;
        }
        if suffix_start == 0 {
            return None;
        }

        Some(CompactionPlan {
            through_exchange: spans[suffix_start - 1].id,
            prefix_messages: self.messages_for_spans(&spans[..suffix_start]),
        })
    }
}

fn opening_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

fn opening_line_for_events(events: &[&Protocol]) -> String {
    for event in events {
        match event {
            Protocol::UserPrompt { text, .. } => return opening_line(text),
            Protocol::ContextMessage { message, .. } if message.role == ChatRole::User => {
                if let Some(text) = message.content.first_text() {
                    return opening_line(text);
                }
            }
            Protocol::ContextMessage { .. }
            | Protocol::SessionStarted { .. }
            | Protocol::SessionResumed { .. }
            | Protocol::SessionEnded
            | Protocol::StepStarted { .. }
            | Protocol::AssistantMessage { .. }
            | Protocol::ToolResults { .. }
            | Protocol::ContextEdited { .. } => {}
        }
    }
    String::new()
}

fn render_context_transcript(ledger: &Ledger, view: &View, named: &HashSet<u64>) -> String {
    let mut transcript = String::new();
    let mut first_section = true;
    if let Some(digest) = &view.digest
        && named.contains(&digest.through_exchange)
    {
        context_section(
            &mut transcript,
            &mut first_section,
            &format!("digest through {}", digest.through_exchange),
        );
        render_role(&mut transcript, "user", &digest.text);
    }
    for span in &view.spans {
        if !named.contains(&span.id) {
            continue;
        }
        context_section(
            &mut transcript,
            &mut first_section,
            &format!("exchange {}", span.id),
        );
        let events = ledger
            .resident_events(span.events.clone())
            .expect("view spans are resident in the ledger");
        for event in events {
            match event {
                Protocol::UserPrompt { text, .. } => render_role(&mut transcript, "user", text),
                Protocol::ContextMessage { message, .. }
                | Protocol::AssistantMessage { message, .. } => {
                    render_chat_message(&mut transcript, message);
                }
                Protocol::StepStarted { n, .. } => {
                    writeln!(transcript, "--- step {n} ---")
                        .expect("writing to a String never fails");
                }
                Protocol::ToolResults { results } => {
                    for result in results {
                        render_role(
                            &mut transcript,
                            "tool",
                            &format!("{}: {}", result.id, result.content),
                        );
                    }
                }
                Protocol::SessionStarted { .. }
                | Protocol::SessionResumed { .. }
                | Protocol::SessionEnded
                | Protocol::ContextEdited { .. } => {}
            }
        }
    }
    transcript
}

fn context_section(output: &mut String, first: &mut bool, title: &str) {
    if !*first {
        output.push('\n');
    }
    writeln!(output, "=== {title} ===").expect("writing to a String never fails");
    *first = false;
}

fn render_chat_message(output: &mut String, message: &ChatMessage) {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    };
    let mut rendered = false;
    for part in &message.content {
        let text = part.as_text().map_or_else(
            || serde_json::to_string(part).unwrap_or_else(|_| "<unrenderable content>".into()),
            str::to_string,
        );
        render_role(output, role, &text);
        rendered = true;
    }
    if !rendered {
        render_role(output, role, "");
    }
}

fn render_role(output: &mut String, role: &str, text: &str) {
    writeln!(output, "[{role}]").expect("writing to a String never fails");
    output.push_str(text);
    if !text.ends_with('\n') {
        output.push('\n');
    }
}

fn live_exchange_refusal(id: u64) -> String {
    format!("exchange {id} is the one you are in — a context edit may only name closed exchanges")
}

fn folded_exchange_refusal(id: u64, reach: u64) -> String {
    format!(
        "exchange {id} is folded into the digest through {reach} — name {reach} to drop the digest whole, or fold further"
    )
}

fn unknown_exchange_refusal(id: u64) -> String {
    format!("exchange {id} is not present in the current view")
}

fn rewind_unknown_exchange_refusal(id: u64, last: Option<u64>) -> String {
    match last {
        Some(last) => format!(
            "exchange {id} is not present in the current view — the last exchange is {last}"
        ),
        None => format!(
            "exchange {id} is not present in the current view — there is no last exchange to rewind"
        ),
    }
}

fn advance(state: &State, protocol: &Protocol) -> State {
    match protocol {
        Protocol::UserPrompt { .. } => State::AwaitingAssistantAfterUser,
        Protocol::AssistantMessage {
            pending_tool_ids, ..
        } if !pending_tool_ids.is_empty() => State::AwaitingToolResults {
            pending_ids: pending_tool_ids.clone(),
        },
        Protocol::AssistantMessage { .. } => State::ReadyForUser,
        Protocol::ToolResults { .. } => State::AwaitingAssistantAfterToolResults,
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::ContextMessage { .. }
        | Protocol::StepStarted { .. }
        | Protocol::ContextEdited { .. } => state.clone(),
    }
}

fn into_chat_messages(protocol: Protocol) -> Vec<ChatMessage> {
    match protocol {
        Protocol::UserPrompt { text, .. } => vec![ChatMessage::user(text)],
        Protocol::ContextMessage { message, .. } | Protocol::AssistantMessage { message, .. } => {
            vec![message]
        }
        Protocol::ToolResults { results } => results
            .into_iter()
            .map(|r| ChatMessage::from(ToolResponse::new(&r.id, &r.content)))
            .collect(),
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::StepStarted { .. }
        | Protocol::ContextEdited { .. } => Vec::new(),
    }
}

fn record_exchange(protocol: &Protocol) -> Option<u64> {
    match protocol {
        Protocol::UserPrompt { exchange, .. } | Protocol::ContextMessage { exchange, .. } => {
            Some(*exchange)
        }
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::StepStarted { .. }
        | Protocol::AssistantMessage { .. }
        | Protocol::ToolResults { .. }
        | Protocol::ContextEdited { .. } => None,
    }
}

/// Protocol sequencing legality — a runtime guard for data read off disk,
/// never consulted by [`Fold::step`] itself: live authorship is already
/// typestate-correct, so the only place this can fail is [`validate_resume`],
/// walking a file this process did not just write.
fn admissible(state: &State, protocol: &Protocol) -> bool {
    match protocol {
        Protocol::UserPrompt { .. } => matches!(
            state,
            State::ReadyForUser | State::AwaitingAssistantAfterToolResults,
        ),
        Protocol::ContextMessage { .. } => matches!(state, State::ReadyForUser),
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
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::StepStarted { .. }
        | Protocol::ContextEdited { .. } => true,
    }
}

fn stub_repairable(protocol: &Protocol) -> bool {
    matches!(
        protocol,
        Protocol::UserPrompt { .. } | Protocol::ContextMessage { .. }
    )
}

/// The stub records role alternation needs to drive `state` back to
/// [`State::ReadyForUser`] — the one remedy a crash-truncated tail gets on
/// resume, mirroring what a live `quiesce` would have recorded.
fn stubs_to_ready(state: &State, reason: QuiesceReason) -> Vec<Protocol> {
    let (tool_stub, assistant_stub, stop_label) = match reason {
        QuiesceReason::Cancelled => (
            "cancelled before tool execution",
            "(cancelled by user)",
            "cancelled",
        ),
        QuiesceReason::Aborted => (
            "no result: exchange aborted before tool execution",
            "(no reply: exchange aborted)",
            "aborted",
        ),
        QuiesceReason::Replied => (
            "no result: exchange ended on reply",
            "(exchange ended: replied to parent)",
            "replied",
        ),
    };
    let mut stubs = Vec::new();
    let mut state = state.clone();
    if let State::AwaitingToolResults { pending_ids } = &state {
        let results = pending_ids
            .iter()
            .map(|id| ToolResult {
                id: id.clone(),
                content: tool_stub.into(),
            })
            .collect();
        let stub = Protocol::ToolResults { results };
        state = advance(&state, &stub);
        stubs.push(stub);
    }
    if !matches!(state, State::ReadyForUser) {
        stubs.push(Protocol::AssistantMessage {
            message: ChatMessage::assistant(assistant_stub),
            pending_tool_ids: Vec::new(),
            stop_reason: Some(stop_label.into()),
        });
    }
    stubs
}

fn append_aborted_stubs(state: &mut State, messages: &mut Vec<ChatMessage>) {
    for stub in stubs_to_ready(state, QuiesceReason::Aborted) {
        *state = advance(state, &stub);
        messages.extend(into_chat_messages(stub));
    }
}

/// One step of the fold: append to the ledger, advance the view, and free
/// whatever no surviving span needs any more — the law that no recorded
/// protocol record is ever truly discarded holds because `free` keeps the
/// evicted index's `Stamp`, never drops it.
fn step_protocol(memo: &mut Memo, stamp: Stamp, protocol: &Protocol) {
    let index = memo.ledger.append(Recorded::new(stamp, protocol.clone()));
    memo.state = advance(&memo.state, protocol);
    record_event_span(&mut memo.view, &mut memo.max_exchange, protocol, index);
    let removed = if let Protocol::ContextEdited { op, .. } = protocol {
        let removed = removed_event_ranges(&memo.view, op);
        apply_context_op(&mut memo.view, op);
        memo.newest_edit = Some(index);
        removed
    } else {
        Vec::new()
    };
    memo.ledger.evict(&removed);
    let in_view = memo
        .view
        .spans
        .iter()
        .any(|span| span.events.contains(&index));
    if memo.max_exchange > 0 && !in_view {
        memo.ledger.free(index);
    }
}

fn removed_event_ranges(view: &View, op: &ContextOp) -> Vec<Range<usize>> {
    match op {
        ContextOp::Fold {
            through_exchange, ..
        } => view
            .spans
            .iter()
            .filter(|span| span.id <= *through_exchange)
            .map(|span| span.events.clone())
            .collect(),
        ContextOp::Drop { exchanges } => view
            .spans
            .iter()
            .filter(|span| exchanges.contains(&span.id))
            .map(|span| span.events.clone())
            .collect(),
    }
}

fn apply_context_op(view: &mut View, op: &ContextOp) {
    match op {
        ContextOp::Fold {
            through_exchange,
            digest,
        } => {
            view.digest = Some(Digest {
                through_exchange: *through_exchange,
                text: digest.clone(),
            });
            view.spans.retain(|span| span.id > *through_exchange);
        }
        ContextOp::Drop { exchanges } => {
            let remove_digest = view
                .digest
                .as_ref()
                .is_some_and(|digest| exchanges.contains(&digest.through_exchange));
            if remove_digest {
                view.digest = None;
            }
            view.spans.retain(|span| !exchanges.contains(&span.id));
        }
    }
}

/// The partition half of the step: an id-bearing record extends the span
/// with its id at the tail or opens a new one past the running maximum;
/// every other record extends whichever span last grew.
fn record_event_span(view: &mut View, max_exchange: &mut u64, protocol: &Protocol, index: usize) {
    if let Some(id) = record_exchange(protocol) {
        let extended = view
            .spans
            .iter_mut()
            .find(|span| span.id == id && span.events.end == index)
            .map(|span| span.events.end = index + 1)
            .is_some();
        if !extended && id > *max_exchange {
            view.spans.push(Span {
                id,
                events: index..index + 1,
            });
        }
        *max_exchange = (*max_exchange).max(id);
    } else if *max_exchange != 0 {
        let id = *max_exchange;
        if let Some(span) = view
            .spans
            .iter_mut()
            .find(|span| span.id == id && span.events.end == index)
        {
            span.events.end = index + 1;
        }
    }
}

/// Re-derive `(state, view, max_exchange)` from the ledger's own content
/// alone, resident or freed — the `fold == memo` check [`resume`] runs
/// against the incrementally maintained memo, so an eviction that silently
/// lost or misplaced a record is caught rather than trusted.
fn refold(ledger: &Ledger) -> io::Result<(State, View, u64)> {
    let mut state = State::default();
    let mut view = View::default();
    let mut max_exchange = 0u64;
    for (index, protocol) in ledger.fold_events()? {
        state = advance(&state, &protocol);
        record_event_span(&mut view, &mut max_exchange, &protocol, index);
        if let Protocol::ContextEdited { op, .. } = &protocol {
            apply_context_op(&mut view, op);
        }
    }
    Ok((state, view, max_exchange))
}

/// The protocol sequencing law, replayed record by record beside the fold that
/// [`resume`] is building: interposing the stub records a live `quiesce` would
/// have written wherever a record is inadmissible but repairable, and refusing
/// outright wherever it is not — foreign data no live door could have produced.
///
/// Its `state` and `view` are its own, never the fold's, so admission stays an
/// independent judgement on the file rather than a self-agreement of the memo.
#[derive(Default)]
struct Admission {
    state: State,
    view: View,
    max_exchange: u64,
    index: usize,
}

impl Admission {
    /// # Errors
    /// Returns [`Refusal::Foreign`] for a record no live session could have
    /// written at this point in the log.
    fn step(&mut self, protocol: &Protocol) -> Result<(), Refusal> {
        let index = self.index;
        self.index += 1;
        if !admissible(&self.state, protocol) {
            if !stub_repairable(protocol) {
                return Err(Refusal::Foreign {
                    record: Box::new(Record::Protocol(protocol.clone())),
                    reason: format!(
                        "record {} is foreign protocol data, not a seam that quiesce can repair; was record.jsonl hand-edited or written by an incompatible exarch?",
                        index + 1
                    ),
                });
            }
            for stub in stubs_to_ready(&self.state, QuiesceReason::Aborted) {
                self.state = advance(&self.state, &stub);
            }
        }
        self.state = advance(&self.state, protocol);
        if let Some(id) = record_exchange(protocol) {
            let joins = id > self.max_exchange
                || self
                    .view
                    .spans
                    .iter()
                    .any(|span| span.id == id && span.events.end == index);
            if !joins {
                return Err(Refusal::Foreign {
                    record: Box::new(Record::Protocol(protocol.clone())),
                    reason: format!(
                        "record {} names exchange {id}, which the log had already moved past; no live session records a stale exchange id",
                        index + 1
                    ),
                });
            }
        }
        record_event_span(&mut self.view, &mut self.max_exchange, protocol, index);
        if let Protocol::ContextEdited { op, .. } = protocol {
            apply_context_op(&mut self.view, op);
        }
        Ok(())
    }
}

/// The bytes past the last complete JSONL line — a torn write from a session
/// that did not shut down cleanly.
struct CrashTail {
    bytes: Vec<u8>,
    complete_len: u64,
}

/// Scanned backwards from the end, a window at a time, rather than forwards
/// through the whole file: the tail is one torn record long however long the
/// log behind it is.
const CRASH_SCAN_WINDOW: u64 = 8 * 1024;

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:model-fold-crash-scan] scans record.jsonl's tail for a torn trailing write before resume folds it; output infra, not turn-time data I/O"
)]
fn find_crash_tail(path: &Path) -> io::Result<Option<CrashTail>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut complete_len = 0;
    let mut cursor = len;
    while cursor > 0 {
        let window = cursor.saturating_sub(CRASH_SCAN_WINDOW);
        let mut chunk = vec![0; usize::try_from(cursor - window).unwrap_or(usize::MAX)];
        let _ = file.seek(SeekFrom::Start(window))?;
        file.read_exact(&mut chunk)?;
        if let Some(last) = chunk.iter().rposition(|byte| *byte == b'\n') {
            complete_len = window + last as u64 + 1;
            break;
        }
        cursor = window;
    }
    if complete_len == len {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    let _ = file.seek(SeekFrom::Start(complete_len))?;
    let _ = file.read_to_end(&mut bytes)?;
    Ok(Some(CrashTail {
        bytes,
        complete_len,
    }))
}

/// Move a torn tail to `record.jsonl.crash` and trim the live file back to
/// its last complete line, exactly mirroring the retired `events.jsonl`
/// quarantine.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:model-fold-crash-quarantine] sidecars and trims a torn record.jsonl tail before resume folds it; output infra, not turn-time data I/O"
)]
fn quarantine_tail(path: &Path, tail: &CrashTail) -> io::Result<()> {
    let mut sidecar_name = path.file_name().unwrap_or_default().to_os_string();
    sidecar_name.push(".crash");
    let sidecar = path.with_file_name(sidecar_name);
    let mut quarantine = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sidecar)?;
    quarantine.write_all(&tail.bytes)?;
    quarantine.flush()?;
    quarantine.sync_all()?;

    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(tail.complete_len)?;
    file.sync_all()?;
    eprintln!(
        "exarch: quarantined {} bytes from {} in {} before trimming the live log",
        tail.bytes.len(),
        path.display(),
        sidecar.display()
    );
    Ok(())
}

/// Fold `record.jsonl` into a fresh [`Memo`]: quarantine a torn tail, then
/// stream the log once — admitting each protocol record's sequencing and
/// folding it in the same pass — and check the fold against an independent
/// from-scratch refold of the ledger it built: the `fold == memo` law,
/// migrated whole from `AgentLog::resume`.
///
/// The pass is a stream rather than a collection, so what a resume holds is
/// the memo it is building — residency at the addressed view — and never the
/// log it is building it from.
///
/// Returns the folded memo alongside the `(model, label)` pair the head
/// record identifies the session by — `AgentLog::resume` reads its own
/// identity from here rather than re-deriving it, since a session's identity
/// is a fact about its first record, not a second thing to keep in step.
///
/// # Errors
/// Returns an error if the file cannot be read, quarantined, has no
/// `SessionStarted { session_id: 0, parent: None }` head record, or refolds
/// to a projection that disagrees with the one built incrementally.
pub fn resume(path: &Path) -> io::Result<(Memo, String, String)> {
    if let Some(tail) = find_crash_tail(path)? {
        quarantine_tail(path, &tail)?;
    }
    let mut memo = Memo::default();
    memo.attach_source(path.to_path_buf());
    let mut admission = Admission::default();
    let mut identity = None;
    for record in Log::read(path)? {
        let record = record?;
        let Record::Protocol(protocol) = record.value() else {
            continue;
        };
        if identity.is_none() {
            identity = Some(match protocol {
                Protocol::SessionStarted {
                    session_id: 0,
                    parent: None,
                    model,
                    label,
                    ..
                } => (model.clone(), label.clone()),
                Protocol::SessionStarted {
                    session_id, parent, ..
                } => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record starts session {session_id:?} with parent {parent:?}; expected SessionStarted {{ session_id: 0, parent: None }} — is this a child log?",
                        path.display()
                    )));
                }
                other @ (Protocol::SessionResumed { .. }
                | Protocol::SessionEnded
                | Protocol::UserPrompt { .. }
                | Protocol::ContextMessage { .. }
                | Protocol::StepStarted { .. }
                | Protocol::AssistantMessage { .. }
                | Protocol::ToolResults { .. }
                | Protocol::ContextEdited { .. }) => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record is {other:?}; expected SessionStarted {{ session_id: 0, parent: None }} — is this a copied child log?",
                        path.display()
                    )));
                }
            });
        }
        admission
            .step(protocol)
            .map_err(|refusal| io::Error::other(refusal.to_string()))?;
        step_protocol(&mut memo, record.stamp().clone(), protocol);
    }
    let Some((model, label)) = identity else {
        return Err(io::Error::other(format!(
            "cannot resume {}: no complete session records were found; is the file truncated?",
            path.display()
        )));
    };

    let (state, view, max_exchange) = refold(&memo.ledger)?;
    if (state, view, max_exchange) != (memo.state.clone(), memo.view.clone(), memo.max_exchange) {
        return Err(io::Error::other(
            "record.jsonl's from-scratch refold disagreed with the ledger's incremental projection",
        ));
    }
    Ok((memo, model, label))
}
