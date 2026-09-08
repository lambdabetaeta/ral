//! The one fold: [`Context::step`] judges a record and applies it, and a
//! context edit is the one step that moves a turn from here to there —
//! its legality and its planning live beside it.

use super::state::{admissible, advance};
use super::{
    Context, Held, Turn, into_chat_messages, message_bytes, message_label,
    not_recorded_refusal_turn, opening_line,
};
use crate::agent::event::{ContextOp, TurnKind};
use crate::record::{Protocol, Record, Recorded, Refusal};
use std::collections::HashSet;

impl Context {
    /// Resolve a user rewind into the whole visible suffix beginning at its
    /// anchor. The anchor is checked before the suffix is derived.
    ///
    /// # Errors
    /// Refuses an absent anchor or one that has already left the context.
    pub(crate) fn rewind_exchanges(&self, anchor: u64) -> Result<Vec<u64>, String> {
        let mut exchanges: Vec<u64> = Vec::new();
        for turn in self.resident() {
            if turn.exchange >= anchor && exchanges.last() != Some(&turn.exchange) {
                exchanges.push(turn.exchange);
            }
        }
        if exchanges.first() != Some(&anchor) {
            if self.turn(anchor).is_some() {
                return Err(self.departed_refusal(anchor));
            }
            return Err(rewind_unknown_refusal(anchor, self.last_context_exchange()));
        }
        Ok(exchanges)
    }

    /// # Errors
    /// Refuses an unnamed edit, an unaddressable target, or the work in hand.
    pub(crate) fn validate_edit(&self, op: &ContextOp) -> Result<(), String> {
        match op {
            ContextOp::Evict { through, .. } => self.validate_cut(*through),
            ContextOp::Drop { exchanges } => {
                if exchanges.is_empty() {
                    return Err("a context edit must name at least one exchange".into());
                }
                let mut named = HashSet::with_capacity(exchanges.len());
                for &exchange in exchanges {
                    if !named.insert(exchange) {
                        return Err(format!("exchange {exchange} was named more than once"));
                    }
                    self.validate_droppable(exchange)?;
                }
                Ok(())
            }
        }
    }

    /// A cut names a turn still in the context, and never the newest one: an
    /// eviction exists to keep the work in hand.
    fn validate_cut(&self, through: u64) -> Result<(), String> {
        let Some(turn) = self.turn(through) else {
            return Err(not_recorded_refusal_turn(through, self.reach()));
        };
        if !turn.is_resident() {
            return Err(self.departed_turn_refusal(through));
        }
        if self
            .resident()
            .last()
            .is_some_and(|last| last.id == through)
        {
            return Err(format!(
                "{through} is the newest turn; an eviction keeps the work in hand"
            ));
        }
        Ok(())
    }

    /// An exchange is droppable iff some turn of it is still in the context
    /// and it is not the one being written.
    fn validate_droppable(&self, exchange: u64) -> Result<(), String> {
        if !self.resident().any(|turn| turn.exchange == exchange) {
            if self.exchange_turns(exchange).is_empty() {
                return Err(format!(
                    "exchange {exchange} is not present in your context"
                ));
            }
            return Err(self.departed_refusal(exchange));
        }
        if self.is_live_exchange(exchange) {
            return Err(format!(
                "exchange {exchange} is the one you are in — a context edit may only name closed exchanges"
            ));
        }
        Ok(())
    }

    fn departed_refusal(&self, exchange: u64) -> String {
        match self.resident().next() {
            Some(first) => format!(
                "exchange {exchange} has already left your context — the earliest still in it is {}",
                first.exchange
            ),
            None => format!(
                "exchange {exchange} has already left your context — no exchange is still in it"
            ),
        }
    }

    fn departed_turn_refusal(&self, turn: u64) -> String {
        match self.resident().next() {
            Some(first) => format!(
                "turn {turn} has already left your context — the earliest still in it is {}",
                first.id
            ),
            None => {
                format!("turn {turn} has already left your context — no turn is still in it")
            }
        }
    }

    /// The cut an eviction would make to spend no more than `keep` bytes on
    /// what stays, `None` when nothing is old enough to shed.
    ///
    /// Candidates are every turn in the context but the last, so the work in
    /// hand is unnameable rather than merely unlikely. The last turn's own
    /// weight is spent up front, and its user turn's with it: a user turn
    /// stays while its exchange has an assistant turn in context, so that one
    /// is never a saving. For the same reason a user turn that overshoots
    /// cannot leave alone — its whole exchange does, the cut falling through
    /// the exchange's newest resident turn.
    pub(crate) fn plan_eviction(&self, keep: usize) -> Option<u64> {
        let resident: Vec<&Turn> = self.resident().collect();
        let (last, candidates) = resident.split_last()?;
        let mut spent = last.bytes;
        let paid = (!last.is_user()).then_some(last.exchange);
        if let Some(user) = paid.and_then(|id| self.turn(id)) {
            spent = spent.saturating_add(user.bytes);
        }
        for turn in candidates.iter().rev() {
            if paid == Some(turn.id) {
                continue;
            }
            let total = spent.saturating_add(turn.bytes);
            if total > keep {
                let through = if turn.is_user() {
                    candidates
                        .iter()
                        .rev()
                        .find(|t| t.exchange == turn.id)
                        .map_or(turn.id, |t| t.id)
                } else {
                    turn.id
                };
                // A cut that would take nothing is no plan: the work in hand
                // and the user turn it belongs to already fill the budget.
                return self.table.takes(through).then_some(through);
            }
            spent = total;
        }
        None
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
    /// written at this point in the log.
    pub(crate) fn step(&mut self, record: Recorded<Protocol>) -> Result<(), Refusal> {
        let index = self.len;
        self.judge(index, record.value())?;
        self.len = index + 1;
        self.state = advance(&self.state, record.value());
        match record.value() {
            Protocol::Inherited { turns, notes } => {
                self.table.install(turns, notes);
                return Ok(());
            }
            Protocol::ContextEdited { op, .. } => {
                self.apply_context_op(op);
                self.newest_edit = Some(index);
                return Ok(());
            }
            Protocol::UserPrompt { .. }
            | Protocol::ContextMessage { .. }
            | Protocol::AssistantMessage { .. }
            | Protocol::ToolResults { .. } => {}
        }
        let Some(at) = self.place(record.value()) else {
            return Ok(());
        };
        let bytes = message_bytes(&into_chat_messages(record.value().clone()));
        self.table.absorb(at, record, bytes);
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
        if let Protocol::Inherited { turns, notes } = protocol {
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
            let past = turns.iter().find(
                |linked| matches!(linked.row.held, Held::Evicted { cut } if cut >= notes.len()),
            );
            if let Some(linked) = past {
                return Err(foreign(format!(
                    "record {n} says turn {} left at an eviction the link does not carry a note for, of the {} it carries; was this log's opening record hand-edited?",
                    linked.row.id,
                    notes.len()
                )));
            }
        }
        if let Some(turn) = self.extends(protocol).filter(|turn| !turn.is_resident()) {
            let id = turn.id;
            return Err(foreign(
                if matches!(protocol, Protocol::ContextMessage { .. }) {
                    format!(
                        "record {n} imports turn {id}, which the link says has already left the context — a fork re-records only what its parent still had"
                    )
                } else {
                    format!("record {n} extends turn {id}, which has already left the context")
                },
            ));
        }
        if let Protocol::ContextMessage { id, exchange, .. } = protocol
            && let Some(turn) = self.turn(*id)
            && turn.exchange != *exchange
        {
            return Err(foreign(format!(
                "record {n} imports turn {id} under exchange {exchange}, but the link places that turn in exchange {}",
                turn.exchange
            )));
        }
        Ok(())
    }

    /// An id the log had already moved past: a prompt is either a fresh
    /// exchange or steering on the one in hand, an assistant message is
    /// always freshly minted, and an imported message names a turn the link
    /// brought over.
    fn stale(&self, protocol: &Protocol) -> Option<u64> {
        let reach = self.reach();
        match protocol {
            Protocol::UserPrompt { exchange, .. } => Some(*exchange).filter(|id| {
                Some(*id) <= reach
                    && self.table.turns().last().map(|turn| turn.exchange) != Some(*id)
            }),
            Protocol::AssistantMessage { turn, .. } => Some(*turn).filter(|id| Some(*id) <= reach),
            Protocol::ContextMessage { id, .. } => {
                Some(*id).filter(|id| Some(*id) <= reach && self.turn(*id).is_none())
            }
            Protocol::ToolResults { .. }
            | Protocol::ContextEdited { .. }
            | Protocol::Inherited { .. } => None,
        }
    }

    /// The turn a record's material would extend, where it opens none — the
    /// newest row, or the row a [`Protocol::ContextMessage`] names.
    fn extends(&self, protocol: &Protocol) -> Option<&Turn> {
        match protocol {
            Protocol::UserPrompt { exchange, .. } => (Some(*exchange) <= self.reach())
                .then(|| self.table.turns().last())
                .flatten(),
            Protocol::ToolResults { .. } => self.table.turns().last(),
            Protocol::ContextMessage { id, .. } => self.turn(*id),
            Protocol::AssistantMessage { .. }
            | Protocol::ContextEdited { .. }
            | Protocol::Inherited { .. } => None,
        }
    }

    /// Which row a record's material lands on, opening one where the record
    /// bears an id the structure has not reached.
    ///
    /// A prompt past the reach opens an exchange; one at or below it is
    /// steering, and extends the assistant turn it answers. An imported
    /// message names its turn outright, the table having arrived whole with
    /// the link, and only a note that inherits nothing opens one of its own.
    fn place(&mut self, protocol: &Protocol) -> Option<usize> {
        let reach = self.reach();
        let last = self.table.turns().len().checked_sub(1);
        match protocol {
            Protocol::UserPrompt { exchange, text } => {
                if Some(*exchange) <= reach {
                    return last;
                }
                Some(
                    self.table
                        .open(*exchange, *exchange, TurnKind::Exchange, opening_line(text)),
                )
            }
            Protocol::AssistantMessage { turn, message, .. } => {
                let exchange = self.table.turns().last()?.exchange;
                Some(
                    self.table
                        .open(*turn, exchange, TurnKind::Exchange, message_label(message)),
                )
            }
            Protocol::ContextMessage {
                id,
                exchange,
                message,
            } => {
                if let Some(at) = self.table.turns().iter().position(|turn| turn.id == *id) {
                    return Some(at);
                }
                (Some(*id) > reach).then(|| {
                    self.table
                        .open(*id, *exchange, TurnKind::Import, message_label(message))
                })
            }
            Protocol::ToolResults { .. } => last,
            Protocol::ContextEdited { .. } | Protocol::Inherited { .. } => None,
        }
    }

    /// An edit is the one step that moves a turn from here to there; the
    /// table decides which turns a cut takes.
    pub(super) fn apply_context_op(&mut self, op: &ContextOp) {
        match op {
            ContextOp::Evict { through, note } => self.table.evict(*through, note.clone()),
            ContextOp::Drop { exchanges } => self.table.drop_exchanges(exchanges),
        }
    }
}

fn rewind_unknown_refusal(id: u64, last: Option<u64>) -> String {
    match last {
        Some(last) => {
            format!("exchange {id} is not present in your context — the last exchange is {last}")
        }
        None => format!(
            "exchange {id} is not present in your context — there is no last exchange to rewind"
        ),
    }
}
