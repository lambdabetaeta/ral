//! The one fold: [`Context::step`] judges a record and applies it. An eviction
//! or a withdrawal is what moves a turn from here to there — the eviction's
//! resolution and its planning live beside it.

use super::state::{admissible, advance, resting};
use super::transcript::read_seeds;
use super::{
    Context, Held, Turn, chat_messages, message_bytes, message_label, message_role,
    not_recorded_refusal_turn, opening_line, runs,
};
use crate::record::{Protocol, Record, Recorded, Refusal, Role, TurnKind};
use std::collections::BTreeSet;

impl Context {
    /// The set of resident turns an eviction naming `turns` actually takes:
    /// the one place the survivor rule lives, and the set the writer records
    /// so replay departs exactly these ids.
    ///
    /// Refused by name: a turn never recorded, one that has already left, and
    /// the turn being written now. Kept silently: a prompt one of whose
    /// answers the set leaves in the context — hence the invariant that every
    /// resident assistant turn has its prompt resident before it.
    ///
    /// # Errors
    /// Refuses an eviction that names no turn, one of the three above, and a
    /// set the survivor rule would empty.
    pub(crate) fn resolve_cut(&self, turns: &[u64]) -> Result<Vec<u64>, String> {
        // Order and repeats are the caller's business, not a refusal.
        let mut named: BTreeSet<u64> = turns.iter().copied().collect();
        if named.is_empty() {
            return Err("an eviction must name at least one turn".into());
        }
        for &id in &named {
            let Some(turn) = self.turn(id) else {
                return Err(not_recorded_refusal_turn(id, self.reach()));
            };
            if !turn.is_resident() {
                return Err(self.departed_turn_refusal(id));
            }
            if self.unclosed_turn() == Some(id) {
                return Err(format!(
                    "turn {id} is being written now: an eviction keeps the work in hand"
                ));
            }
        }
        // The survivor rule: a prompt one of whose answers the set leaves in
        // the context stays with it.
        let kept: Vec<u64> = named
            .iter()
            .copied()
            .filter(|&id| {
                self.turn(id).is_some_and(Turn::is_user)
                    && self
                        .answers(id)
                        .any(|turn| turn.is_resident() && !named.contains(&turn.id))
            })
            .collect();
        for id in &kept {
            let _ = named.remove(id);
        }
        if named.is_empty() {
            let user = *kept
                .first()
                .expect("the set emptied, so the rule kept a prompt back");
            let rest: Vec<u64> = self
                .answers(user)
                .filter(|turn| turn.is_resident())
                .map(|turn| turn.id)
                .collect();
            let (turns, are, them) = if rest.len() == 1 {
                ("turn", "is", "it")
            } else {
                ("turns", "are", "them")
            };
            return Err(format!(
                "turn {user} is the prompt whose {turns} {} {are} still in your context, and a \
                 prompt stays with {them}: name {them} too, or leave it",
                runs(&rest)
            ));
        }
        Ok(named.into_iter().collect())
    }

    /// Whether `/rewind anchor` may be recorded now: the anchor is a turn the
    /// lineage recorded and has not yet withdrawn, resident or evicted, and no
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
        if !self.is_ready() {
            return Err(format!(
                "cannot rewind while the session is {}",
                self.waiting_for()
            ));
        }
        Ok(())
    }

    fn departed_turn_refusal(&self, turn: u64) -> String {
        match self.resident().next() {
            Some(first) => format!(
                "turn {turn} has already left your context: the earliest still in it is {}",
                first.id
            ),
            None => {
                format!("turn {turn} has already left your context: no turn is still in it")
            }
        }
    }

    /// The cut the harness's pressure eviction would make to spend no more
    /// than `keep` bytes on what stays, resolved; `None` when nothing is old
    /// enough to shed.
    ///
    /// Candidates are every turn in the context but the last, so the work in
    /// hand is unnameable rather than merely unlikely. The last turn's own
    /// weight is spent up front, and its prompt's with it: a prompt stays
    /// while one of its answers is in context, so that one is never a saving.
    /// For the same reason a prompt that overshoots cannot leave alone — its
    /// answers go with it, the cut falling through the newest resident one.
    pub(crate) fn plan_eviction(&self, keep: usize) -> Option<Vec<u64>> {
        let resident: Vec<&Turn> = self.resident().collect();
        let (last, candidates) = resident.split_last()?;
        let mut spent = last.bytes;
        let paid = (!last.is_user()).then(|| self.prompt_of(last.id)).flatten();
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
                    self.answers(turn.id)
                        .filter(|t| t.is_resident())
                        .last()
                        .map_or(turn.id, |t| t.id)
                } else {
                    turn.id
                };
                let prefix: Vec<u64> = resident
                    .iter()
                    .filter(|turn| turn.id <= through)
                    .map(|turn| turn.id)
                    .collect();
                // A cut that takes nothing is no plan, and an empty set is
                // exactly what `resolve_cut` refuses.
                return self.resolve_cut(&prefix).ok();
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
    /// written at this point in the log, and [`Refusal::Unreadable`] for a
    /// link whose ancestor's file will not read back.
    pub(crate) fn step(&mut self, record: Recorded<Protocol>) -> Result<(), Refusal> {
        let index = self.len;
        self.judge(index, record.value())?;
        let (locus, protocol) = record.into_parts();
        let mut state = advance(&self.state, &protocol);
        match &protocol {
            Protocol::System { text } => self.system = text.as_str().into(),
            Protocol::Inherited { turns, notes } => {
                // Read first: a failure leaves the structure as it was.
                let seeds = read_seeds(turns)?;
                self.table.install(turns, notes, seeds);
            }
            Protocol::Evicted { cut, .. } => {
                self.table.evict(&cut.turns, cut.note.clone());
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
        if let Protocol::Evicted { cut, .. } = protocol {
            if cut.turns.is_empty() {
                return Err(foreign(format!(
                    "record {n} evicts no turn; was record.jsonl hand-edited?"
                )));
            }
            if let Some(id) = cut
                .turns
                .iter()
                .copied()
                .find(|id| !self.turn(*id).is_some_and(Turn::is_resident))
            {
                return Err(foreign(format!(
                    "record {n} evicts turn {id}, which is not in the context; was record.jsonl hand-edited?"
                )));
            }
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
