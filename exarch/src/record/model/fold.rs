//! The one fold: [`Context::step`] judges a record and applies it. An eviction
//! or a withdrawal is what moves a turn from here to there — the eviction's
//! rules and its planning live beside it.

use super::state::{admissible, advance, resting};
use super::table::{Item, holder};
use super::transcript::read_seeds;
use super::{
    Context, Location, Span, Turn, chat_messages, message_bytes, message_label, message_role,
    not_recorded_refusal_turn, opening_line,
};
use crate::record::{Protocol, Record, Recorded, Refusal, Role, TurnKind};

impl Context {
    /// Whether an eviction may take the run `span`: the one place its rules
    /// live, for the writer and the judge alike.
    ///
    /// # Errors
    /// Refuses an end never recorded or withdrawn, a run holding the turn
    /// being written now, and one that cuts a residue.
    pub(crate) fn resolve_cut(&self, span: Span) -> Result<(), String> {
        for id in [span.from(), span.to()] {
            let Some(turn) = self.turn(id) else {
                return Err(not_recorded_refusal_turn(id, self.reach()));
            };
            if turn.is_withdrawn() {
                return Err(format!(
                    "turn {id} was rewound or cleared away: it is no longer in your context"
                ));
            }
        }
        if let Some(id) = self.unclosed_turn().filter(|&id| span.contains(id)) {
            return Err(format!(
                "turn {id} is being written now: an eviction keeps the work in hand"
            ));
        }
        // `to` ends an item exactly when `to + 1` starts one.
        if let Some(straddled) = self
            .table
            .straddling(span.from())
            .or_else(|| self.table.straddling(span.to().saturating_add(1)))
            .map(|residue| residue.span)
        {
            return Err(format!(
                "{straddled} stand together under one note: name from: {}, to: {} whole, or \
                 leave it",
                straddled.from(),
                straddled.to()
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
    pub(crate) fn rewindable(&self, anchor: u64) -> Result<(), String> {
        let Some(turn) = self.turn(anchor) else {
            return Err(not_recorded_refusal_turn(anchor, self.reach()));
        };
        if turn.is_withdrawn() {
            let earliest = match self.table.turns().iter().find(|turn| !turn.is_withdrawn()) {
                Some(first) => format!("the earliest you can rewind to is turn {}", first.id),
                None => "nothing has been said since the last /clear or /rewind".into(),
            };
            return Err(format!(
                "turn {anchor} was already rewound or cleared away; {earliest}"
            ));
        }
        if let Some(residue) = self.table.straddling(anchor) {
            let span = residue.span;
            return Err(format!(
                "turn {anchor} lies under the residue over {span}: rewind to turn {} or turn {}",
                span.from(),
                span.to() + 1
            ));
        }
        if !self.is_ready() {
            return Err(format!(
                "cannot rewind while the session is {}",
                self.waiting_for()
            ));
        }
        Ok(())
    }

    /// The span a forced eviction would take to spend no more
    /// than `keep` bytes on what stays; `None` when nothing is old enough to
    /// shed.
    ///
    /// A walk back over the frontier from the newest item, which is never a
    /// candidate: the first item that does not fit ends a prefix. A prefix
    /// that is one residue alone is no plan: rewriting it without a note
    /// would save nothing.
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
        if let [Item::Residue(..)] = candidates[..=through] {
            return None;
        }
        Some(candidates[0].span().hull(candidates[through].span()))
    }

    /// The one fold, judging each record before applying it — so a
    /// hand-edited or foreign file is refused by the same function that folds
    /// a live one.
    ///
    /// Live authorship is typestate-correct, so a refusal on the live path is
    /// a harness bug and propagates as [`Refusal`] through [`Fold::step`](crate::record::Fold::step).
    ///
    /// # Errors
    /// Returns [`Refusal::Foreign`] for a record no live session could have
    /// written at this point in the log, and [`Refusal::Unreadable`] for a
    /// link whose ancestor's file will not read back.
    pub(crate) fn step(&mut self, record: Recorded<Protocol>) -> Result<(), Refusal> {
        let index = self.len;
        self.judge(index, record.value())?;
        let (locus, protocol) = record.into_parts();
        let mut state = advance(&self.state, &protocol);
        match &protocol {
            Protocol::System { text } => self.system = text.as_str().into(),
            Protocol::Inherited { turns, residues } => {
                // Read first: a failure leaves the structure as it was.
                let seeds = read_seeds(self.table.sessions(), turns)?;
                self.table.install(turns, residues, seeds);
            }
            Protocol::Evicted { residue } => {
                self.table.evict(residue.clone());
                self.newest_edit = Some(index);
            }
            Protocol::Rewound { .. } | Protocol::Cleared => {
                let from = if let Protocol::Rewound { anchor } = protocol {
                    anchor
                } else {
                    0
                };
                self.table.withdraw(from);
                // The automaton rests where the newest resident turn leaves it.
                state = resting(self.resident().last().map_or(&[], Turn::records));
                self.newest_edit = Some(index);
            }
            Protocol::UserPrompt { .. }
            | Protocol::Steering { .. }
            | Protocol::Import { .. }
            | Protocol::AssistantMessage { .. }
            | Protocol::ToolResults { .. } => {}
        }
        self.len = index + 1;
        self.state = state;
        if let Some(at) = self.place(&protocol) {
            let bytes = message_bytes(&chat_messages(&protocol));
            self.table.absorb(at, Recorded::new(locus, protocol), bytes);
        }
        Ok(())
    }

    /// What no live session could have written here.
    fn judge(&self, index: usize, protocol: &Protocol) -> Result<(), Refusal> {
        let n = index + 1;
        let foreign = |reason: String| Refusal::Foreign {
            record: Box::new(Record::Protocol(protocol.clone())),
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
        if let Protocol::Inherited { turns, residues } = protocol {
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
            if let Some(span) = residues
                .iter()
                .map(|residue| residue.span)
                .find(|span| !carried(span.from()) || !carried(span.to()))
            {
                return Err(foreign(format!(
                    "record {n} links a residue over {span}, whose ends are not a run of turns the link carries; was this log's opening record hand-edited?"
                )));
            }
            let crossed = residues.iter().enumerate().find_map(|(i, earlier)| {
                residues[i + 1..]
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
                    "record {n} links a residue over {earlier} and a later one over {later}, which no sequence of evictions leaves: a later residue covers an earlier one whole or lies apart from it; was this log's opening record hand-edited?"
                )));
            }
            let contradicted = turns.iter().map(|linked| &linked.row).find(|row| {
                row.location != Location::Withdrawn
                    && row.location
                        != holder(
                            Span::single(row.id),
                            residues.iter().map(|residue| residue.span),
                        )
                        .map_or(Location::Resident, Location::Under)
            });
            if let Some(row) = contradicted {
                return Err(foreign(format!(
                    "record {n} links turn {} as {}, which its residues contradict; was this log's opening record hand-edited?",
                    row.id,
                    row.location.as_str()
                )));
            }
        }
        if let Protocol::Rewound { anchor } = protocol
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
        if let Protocol::Rewound { anchor } = protocol
            && let Some(residue) = self.table.straddling(*anchor)
        {
            return Err(foreign(format!(
                "record {n} rewinds to turn {anchor}, inside the residue over {}; was record.jsonl hand-edited?",
                residue.span
            )));
        }
        if let Protocol::Evicted { residue } = protocol
            && let Err(reason) = self.resolve_cut(residue.span)
        {
            return Err(foreign(format!(
                "record {n} evicts {}, which no live session could: {reason}; was record.jsonl hand-edited?",
                residue.span
            )));
        }
        if matches!(protocol, Protocol::Steering { .. }) && self.table.turns().is_empty() {
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
    fn stale(&self, protocol: &Protocol) -> Option<u64> {
        match protocol {
            Protocol::UserPrompt { turn, .. }
            | Protocol::AssistantMessage { turn, .. }
            | Protocol::Import { turn, .. } => Some(*turn).filter(|id| Some(*id) <= self.reach()),
            Protocol::System { .. }
            | Protocol::Steering { .. }
            | Protocol::ToolResults { .. }
            | Protocol::Evicted { .. }
            | Protocol::Rewound { .. }
            | Protocol::Cleared
            | Protocol::Inherited { .. } => None,
        }
    }

    /// The turn a record's material would extend, where it opens none: the
    /// newest row.
    fn extends(&self, protocol: &Protocol) -> Option<&Turn> {
        match protocol {
            Protocol::Steering { .. } | Protocol::ToolResults { .. } => self.table.turns().last(),
            Protocol::System { .. }
            | Protocol::UserPrompt { .. }
            | Protocol::Import { .. }
            | Protocol::AssistantMessage { .. }
            | Protocol::Evicted { .. }
            | Protocol::Rewound { .. }
            | Protocol::Cleared
            | Protocol::Inherited { .. } => None,
        }
    }

    /// Which row a record's material lands on, opening one where the record
    /// bears an id the structure has not reached.
    ///
    /// A prompt or an import opens a turn of its own; a steering line extends
    /// the newest.
    fn place(&mut self, protocol: &Protocol) -> Option<usize> {
        match protocol {
            Protocol::UserPrompt { turn, text, .. } => {
                Some(
                    self.table
                        .open(*turn, Role::User, TurnKind::Own, opening_line(text)),
                )
            }
            Protocol::AssistantMessage { turn, message, .. } => Some(self.table.open(
                *turn,
                Role::Assistant,
                TurnKind::Own,
                message_label(message),
            )),
            Protocol::Import { turn, message } => Some(self.table.open(
                *turn,
                message_role(message),
                TurnKind::Import,
                message_label(message),
            )),
            Protocol::Steering { .. } | Protocol::ToolResults { .. } => {
                self.table.turns().len().checked_sub(1)
            }
            Protocol::System { .. }
            | Protocol::Evicted { .. }
            | Protocol::Rewound { .. }
            | Protocol::Cleared
            | Protocol::Inherited { .. } => None,
        }
    }
}
