//! The turns and the cuts made in them. The `Vec`s are private to this
//! module, so a turn changes where it is only through [`Table::evict`] or
//! [`Table::drop_exchanges`] — the two functions that know which turns a cut
//! takes, and so the two that keep the first resident turn a user turn.

use super::{Body, Held, Linked, Pointer, Turn};
use crate::agent::event::TurnKind;
use crate::record::{Protocol, Recorded};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(super) struct Table {
    turns: Vec<Turn>,
    /// One note slot per eviction; [`Body::There`]'s `cut` indexes it.
    notes: Vec<Option<String>>,
    /// Own `record.jsonl`: where a turn first recorded here points once it
    /// leaves.
    ///
    /// Reading a completed record by byte range from the file this process is
    /// still appending to is safe: a `Locus` exists only once the seam has
    /// written the whole record under its lock.
    source: PathBuf,
}

impl Table {
    pub(super) fn new(source: PathBuf) -> Self {
        Self {
            turns: Vec::new(),
            notes: Vec::new(),
            source,
        }
    }

    /// A table standing where a fold would have left one, for the parent's
    /// tests: they assert that the projections agree with a structure, which
    /// means stating the structure rather than folding a log to reach it.
    #[cfg(test)]
    pub(super) fn seeded(source: PathBuf, turns: Vec<Turn>, notes: Vec<Option<String>>) -> Self {
        Self {
            turns,
            notes,
            source,
        }
    }

    /// Every turn this lineage recorded, in id order.
    pub(super) fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub(super) fn notes(&self) -> &[Option<String>] {
        &self.notes
    }

    pub(super) fn source(&self) -> &Path {
        &self.source
    }

    /// Open a fresh turn at the end of the table, and answer its index.
    pub(super) fn open(&mut self, id: u64, exchange: u64, kind: TurnKind, label: String) -> usize {
        self.turns.push(Turn {
            id,
            exchange,
            kind,
            label,
            bytes: 0,
            body: Body::Here {
                records: Vec::new(),
                origin: None,
            },
        });
        self.turns.len() - 1
    }

    /// Add a record to the turn at `at`, and its weight to that turn's.
    pub(super) fn absorb(&mut self, at: usize, record: Recorded<Protocol>, bytes: usize) {
        let turn = &mut self.turns[at];
        turn.bytes = turn.bytes.saturating_add(bytes);
        match &mut turn.body {
            Body::Here { records, .. } => records.push(record),
            // `judge` refuses a record landing on a departed row.
            Body::There { .. } => unreachable!("a judged record lands on a resident turn"),
        }
    }

    /// A link's own step: the parent's rows and notes become this log's, so
    /// the child's marker, survey and index are the same projections of the
    /// same structure. A resident row's weight is zeroed because the seed's
    /// records follow and the fold sums them as they land.
    pub(super) fn install(&mut self, turns: &[Linked], notes: &[Option<String>]) {
        self.turns = turns
            .iter()
            .map(|Linked { row, at }| {
                let (bytes, body) = match row.held {
                    Held::Resident => (
                        0,
                        Body::Here {
                            records: Vec::new(),
                            origin: Some(at.clone()),
                        },
                    ),
                    Held::Evicted { cut } => (
                        row.bytes,
                        Body::There {
                            at: at.clone(),
                            cut: Some(cut),
                        },
                    ),
                    Held::Dropped => (
                        row.bytes,
                        Body::There {
                            at: at.clone(),
                            cut: None,
                        },
                    ),
                };
                Turn {
                    id: row.id,
                    exchange: row.exchange,
                    kind: row.kind,
                    label: row.label.clone(),
                    bytes,
                    body,
                }
            })
            .collect();
        self.notes = notes.to_vec();
    }

    /// Whether a cut at `through` would take anything.
    pub(super) fn takes(&self, through: u64) -> bool {
        !self.cut_departures(through).is_empty()
    }

    /// Move every turn the cut takes from here to there, and record the
    /// model's note in the slot the departed rows now index.
    pub(super) fn evict(&mut self, through: u64, note: Option<String>) {
        let leaving = self.cut_departures(through);
        let cut = Some(self.notes.len());
        self.depart(&leaving, cut);
        self.notes.push(note);
    }

    /// Move every resident turn of `exchanges` from here to there. A drop
    /// leaves no marker, so the departed rows index no note.
    pub(super) fn drop_exchanges(&mut self, exchanges: &[u64]) {
        let leaving: Vec<u64> = self
            .turns
            .iter()
            .filter(|turn| turn.is_resident() && exchanges.contains(&turn.exchange))
            .map(|turn| turn.id)
            .collect();
        self.depart(&leaving, None);
    }

    /// A cut at `through` takes every turn in the context at or below it, but
    /// a user turn whose exchange still has an assistant turn above the cut:
    /// that one stays with its survivors.
    fn cut_departures(&self, through: u64) -> Vec<u64> {
        self.turns
            .iter()
            .filter(|turn| turn.is_resident() && turn.id <= through)
            .filter(|turn| !(turn.is_user() && self.has_survivor(turn.exchange, through)))
            .map(|turn| turn.id)
            .collect()
    }

    fn has_survivor(&self, exchange: u64, through: u64) -> bool {
        self.turns
            .iter()
            .any(|turn| turn.exchange == exchange && turn.is_resident() && turn.id > through)
    }

    /// The address a departing turn keeps: its `origin` where it has one, else
    /// this log's own file and the loci its records were measured at.
    fn depart(&mut self, leaving: &[u64], cut: Option<usize>) {
        let leaving: HashSet<u64> = leaving.iter().copied().collect();
        let source = self.source.clone();
        for turn in &mut self.turns {
            if !leaving.contains(&turn.id) {
                continue;
            }
            let at = match &mut turn.body {
                Body::Here { records, origin } => origin.take().unwrap_or_else(|| Pointer {
                    source: source.clone(),
                    loci: records
                        .iter()
                        .map(|recorded| recorded.locus().clone())
                        .collect(),
                }),
                Body::There { .. } => continue,
            };
            turn.body = Body::There { at, cut };
        }
    }
}
