//! The one fold: [`Context::step`](crate::record::Fold::step) judges a record and applies it. An eviction
//! or a withdrawal is what moves a turn from here to there — the eviction's
//! rules and its planning live beside it.

use super::state::{admissible, advance, resting};
use super::table::{Item, holder};
use super::transcript::read_seeds;
use super::{
    Context, ContextStepFailure, ForeignRecord, Location, Span, Turn, chat_messages, message_bytes,
    message_label, message_role, not_recorded_refusal_turn, opening_line,
};
use crate::record::{Record, Recorded, Role, Transcript, TurnKind};
use ral_core::fact::{
    ContextFailure, ContextFailureCurrentlyWritten, ContextFailureStraddledEviction,
    ContextFailureWithdrawn, ContextOperation, WithdrawalEvidence,
};

impl Context {
    /// Whether an eviction may take the run `span`: the one place its rules
    /// live, for the writer and the judge alike.
    ///
    /// # Errors
    /// Refuses an end never recorded or withdrawn, a run holding the turn
    /// being written now, and one that cuts an eviction.
    pub(crate) fn resolve_cut(&self, span: Span) -> Result<(), ContextFailure> {
        for id in [span.from(), span.to()] {
            let Some(turn) = self.turn(id) else {
                return Err(not_recorded_refusal_turn(id, self.reach()));
            };
            if turn.is_withdrawn() {
                return Err(ContextFailure::Withdrawn(ContextFailureWithdrawn {
                    turn: id.into(),
                    evidence: WithdrawalEvidence::Unavailable,
                }));
            }
        }
        if let Some(id) = self.unclosed_turn().filter(|&id| span.contains(id)) {
            return Err(ContextFailure::CurrentlyWritten(
                ContextFailureCurrentlyWritten { turn: id.into() },
            ));
        }
        // `to` ends an item exactly when `to + 1` starts one.
        if let Some(straddled) = self
            .table
            .straddling(span.from())
            .or_else(|| self.table.straddling(span.to().saturating_add(1)))
            .map(|eviction| eviction.span)
        {
            return Err(ContextFailure::StraddledEviction(
                ContextFailureStraddledEviction {
                    requested: span.into(),
                    covering: straddled.into(),
                },
            ));
        }
        Ok(())
    }

    /// Whether `/rewind anchor` may be recorded now: the anchor is a turn the
    /// lineage recorded and has not yet withdrawn, it starts an item, and no
    /// batch is awaiting results.
    ///
    /// # Errors
    /// Names the bad anchor, or what the session is waiting for.
    pub(crate) fn rewindable(&self, anchor: u64) -> Result<(), ContextFailure> {
        let Some(turn) = self.turn(anchor) else {
            return Err(not_recorded_refusal_turn(anchor, self.reach()));
        };
        if turn.is_withdrawn() {
            let evidence = self
                .table
                .turns()
                .iter()
                .find(|turn| !turn.is_withdrawn())
                .map_or(WithdrawalEvidence::NothingRetained, |first| {
                    WithdrawalEvidence::EarliestRetained(first.id.into())
                });
            return Err(ContextFailure::Withdrawn(ContextFailureWithdrawn {
                turn: anchor.into(),
                evidence,
            }));
        }

        if let Some(eviction) = self.table.straddling(anchor) {
            let span = eviction.span;
            return Err(ContextFailure::StraddledEviction(
                ContextFailureStraddledEviction {
                    requested: Span::single(anchor).into(),
                    covering: span.into(),
                },
            ));
        }
        if !self.is_ready() {
            return Err(self.mutation_failure(ContextOperation::Rewind));
        }
        Ok(())
    }

    /// The span a forced eviction would take to spend no more
    /// than `keep` bytes on what stays; `None` when nothing is old enough to
    /// shed.
    ///
    /// A walk back over the frontier from the newest item, which is never a
    /// candidate: the first item that does not fit ends a prefix. A prefix
    /// plans nothing when it holds no material item or its only material item
    /// is an eviction: rewriting that without a note would save nothing.
    pub(crate) fn plan_forced_eviction(&self, keep: usize) -> Option<Span> {
        let items = self.table.frontier();
        let (last, candidates) = items.split_last()?;
        let mut spent = last.bytes();
        let mut through = None;
        for (index, item) in candidates.iter().enumerate().rev() {
            spent = spent.saturating_add(item.bytes());
            if spent > keep {
                through = Some(index);
                break;
            }
        }
        let through = through?;
        let mut material = candidates[..=through]
            .iter()
            .filter(|item| item.is_material());
        match (material.next(), material.next()) {
            (None, _) | (Some(Item::Eviction(_)), None) => return None,
            _ => {}
        }
        Some(candidates[0].span().hull(candidates[through].span()))
    }

    /// Replay/opening may read an inherited ancestor, before installing it.
    pub(crate) fn step_transcript(
        &mut self,
        record: Recorded<Transcript>,
    ) -> Result<(), ContextStepFailure> {
        self.judge(self.len, record.value())
            .map_err(ContextStepFailure::Foreign)?;
        if let Transcript::Inherited { turns, evictions } = record.value() {
            let seeds =
                read_seeds(self.table.sessions(), turns).map_err(ContextStepFailure::ReadBack)?;
            self.table.install(turns, evictions, seeds);
        }
        self.apply_transcript(record);
        Ok(())
    }

    /// Live authorship cannot read an ancestor or install an inheritance.
    pub(crate) fn step_live(&mut self, record: Recorded<Transcript>) -> Result<(), ForeignRecord> {
        if matches!(record.value(), Transcript::Inherited { .. }) {
            return Err(ForeignRecord {
                record: Box::new(Record::Transcript(record.into_value())),
                reason: "a live session cannot inherit another transcript".into(),
            });
        }
        self.judge(self.len, record.value())?;
        self.apply_transcript(record);
        Ok(())
    }

    fn apply_transcript(&mut self, record: Recorded<Transcript>) {
        let index = self.len;
        let (locus, protocol) = record.into_parts();
        let mut state = advance(&self.state, &protocol);
        match &protocol {
            Transcript::System { text } => self.system = text.as_str().into(),
            Transcript::Evicted(eviction) => {
                self.table.evict(eviction.clone());
                self.newest_edit = Some(index);
            }
            Transcript::Rewound { .. } | Transcript::Cleared => {
                let from = if let Transcript::Rewound { anchor } = protocol {
                    anchor
                } else {
                    0
                };
                self.table.withdraw(from);
                // The automaton rests where the newest resident turn leaves it.
                state = resting(self.resident().last().map_or(&[], Turn::records));
                self.newest_edit = Some(index);
            }
            Transcript::Inherited { .. }
            | Transcript::UserPrompt { .. }
            | Transcript::Steering { .. }
            | Transcript::Import { .. }
            | Transcript::AssistantMessage { .. }
            | Transcript::ToolResults { .. } => {}
        }
        self.len = index + 1;
        self.state = state;
        if let Some(at) = self.place(&protocol) {
            let bytes = message_bytes(&chat_messages(&protocol));
            self.table.absorb(at, Recorded::new(locus, protocol), bytes);
        }
    }

    /// What no live session could have written here.
    fn judge(&self, index: usize, protocol: &Transcript) -> Result<(), ForeignRecord> {
        let n = index + 1;
        let foreign = |reason: String| ForeignRecord {
            record: Box::new(Record::Transcript(protocol.clone())),
            reason,
        };
        if !admissible(&self.state, protocol) {
            return Err(foreign(format!(
                "record {n} is foreign protocol data, not a seam that quiesce can repair; was record.jsonl hand-edited or written by an incompatible exarch?"
            )));
        }
        if let Some(id) = self.stale(protocol) {
            return Err(foreign(format!(
                "record {n} names turn {id}, which the log had already moved past; no live session records a stale id"
            )));
        }
        if let Transcript::Inherited { turns, evictions } = protocol {
            // The position is the rule: a link stands as the first protocol
            // record of a fork's log, and nowhere else.
            if index != 0 {
                return Err(foreign(format!(
                    "record {n} links this log to an ancestor's transcript, which only a fork's opening may do; by here the log has a context of its own, and no live session inherits twice"
                )));
            }
            if let Some(pair) = turns
                .windows(2)
                .find(|pair| pair[0].row.id >= pair[1].row.id)
            {
                return Err(foreign(format!(
                    "record {n} links a transcript whose turn {} does not precede turn {} in id order; was this log's opening record hand-edited?",
                    pair[0].row.id, pair[1].row.id
                )));
            }
            let carried = |id: u64| turns.iter().any(|linked| linked.row.id == id);
            if let Some(span) = evictions
                .iter()
                .map(|eviction| eviction.span)
                .find(|span| !carried(span.from()) || !carried(span.to()))
            {
                return Err(foreign(format!(
                    "record {n} links an eviction over {span}, whose ends are not a run of turns the link carries; was this log's opening record hand-edited?"
                )));
            }
            let crossed = evictions.iter().enumerate().find_map(|(i, earlier)| {
                evictions[i + 1..]
                    .iter()
                    .find(|later| {
                        let (earlier, later) = (earlier.span, later.span);
                        !(earlier.within(later)
                            || earlier.to() < later.from()
                            || later.to() < earlier.from())
                    })
                    .map(|later| (earlier.span, later.span))
            });
            if let Some((earlier, later)) = crossed {
                return Err(foreign(format!(
                    "record {n} links an eviction over {earlier} and a later one over {later}, which no sequence of evictions leaves: a later eviction covers an earlier one whole or lies apart from it; was this log's opening record hand-edited?"
                )));
            }
            let contradicted = turns.iter().map(|linked| &linked.row).find(|row| {
                row.location != Location::Withdrawn
                    && row.location
                        != holder(
                            Span::single(row.id),
                            evictions.iter().map(|eviction| eviction.span),
                        )
                        .map_or(Location::Resident, Location::Evicted)
            });
            if let Some(row) = contradicted {
                return Err(foreign(format!(
                    "record {n} links turn {} as {}, which its evictions contradict; was this log's opening record hand-edited?",
                    row.id,
                    row.location.as_str()
                )));
            }
        }
        if let Transcript::Rewound { anchor } = protocol
            && let Some(gone) = match self.turn(*anchor) {
                None => Some("was never recorded"),
                Some(turn) => turn
                    .is_withdrawn()
                    .then_some("a /clear or /rewind had already withdrawn"),
            }
        {
            return Err(foreign(format!(
                "record {n} rewinds to turn {anchor}, which {gone}; was record.jsonl hand-edited?"
            )));
        }
        if let Transcript::Rewound { anchor } = protocol
            && let Some(eviction) = self.table.straddling(*anchor)
        {
            return Err(foreign(format!(
                "record {n} rewinds to turn {anchor}, inside the eviction over {}; was record.jsonl hand-edited?",
                eviction.span
            )));
        }
        if let Transcript::Evicted(eviction) = protocol
            && let Err(reason) = self.resolve_cut(eviction.span)
        {
            return Err(foreign(format!(
                "record {n} evicts {}, which no live session could: {}; was record.jsonl hand-edited?",
                eviction.span,
                reason.describe()
            )));
        }
        if matches!(protocol, Transcript::Steering { .. }) && self.table.turns().is_empty() {
            return Err(foreign(format!(
                "record {n} steers a turn, but no turn has been recorded; was record.jsonl hand-edited?"
            )));
        }
        if let Some(turn) = self.extends(protocol).filter(|turn| !turn.is_resident()) {
            return Err(foreign(format!(
                "record {n} extends turn {}, which has already left the context",
                turn.id
            )));
        }
        Ok(())
    }

    /// An id the log had already moved past: a prompt, an import and an
    /// assistant message each open a turn under a freshly minted id.
    fn stale(&self, protocol: &Transcript) -> Option<u64> {
        match protocol {
            Transcript::UserPrompt { turn, .. }
            | Transcript::AssistantMessage { turn, .. }
            | Transcript::Import { turn, .. } => Some(*turn).filter(|id| Some(*id) <= self.reach()),
            Transcript::System { .. }
            | Transcript::Steering { .. }
            | Transcript::ToolResults { .. }
            | Transcript::Evicted(_)
            | Transcript::Rewound { .. }
            | Transcript::Cleared
            | Transcript::Inherited { .. } => None,
        }
    }

    /// The turn a record's material would extend, where it opens none: the
    /// newest row.
    fn extends(&self, protocol: &Transcript) -> Option<&Turn> {
        match protocol {
            Transcript::Steering { .. } | Transcript::ToolResults { .. } => {
                self.table.turns().last()
            }
            Transcript::System { .. }
            | Transcript::UserPrompt { .. }
            | Transcript::Import { .. }
            | Transcript::AssistantMessage { .. }
            | Transcript::Evicted(_)
            | Transcript::Rewound { .. }
            | Transcript::Cleared
            | Transcript::Inherited { .. } => None,
        }
    }

    /// Which row a record's material lands on, opening one where the record
    /// bears an id the structure has not reached.
    ///
    /// A prompt or an import opens a turn of its own; a steering line extends
    /// the newest.
    fn place(&mut self, protocol: &Transcript) -> Option<usize> {
        match protocol {
            Transcript::UserPrompt { turn, text, .. } => {
                Some(
                    self.table
                        .open(*turn, Role::User, TurnKind::Own, opening_line(text)),
                )
            }
            Transcript::AssistantMessage { turn, message, .. } => Some(self.table.open(
                *turn,
                Role::Assistant,
                TurnKind::Own,
                message_label(message),
            )),
            Transcript::Import { turn, message } => Some(self.table.open(
                *turn,
                message_role(message),
                TurnKind::Import,
                message_label(message),
            )),
            Transcript::Steering { .. } | Transcript::ToolResults { .. } => {
                self.table.turns().len().checked_sub(1)
            }
            Transcript::System { .. }
            | Transcript::Evicted(_)
            | Transcript::Rewound { .. }
            | Transcript::Cleared
            | Transcript::Inherited { .. } => None,
        }
    }
}
