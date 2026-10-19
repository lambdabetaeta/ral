//! The turns and the cuts made in them. The `Vec`s are private to this
//! module, so a turn leaves only through [`Table::evict`] or
//! [`Table::withdraw`], and no row is ever removed — the former departs exactly
//! the ids [`Context::resolve_cut`](super::Context::resolve_cut) handed it,
//! that being the one place the survivor rule lives.

use super::{Body, Held, Linked, Pointer, Turn, message_bytes, resident_messages};
use crate::record::{Protocol, Recorded};
use crate::record::{Role, TurnKind};
use std::path::{Path, PathBuf};

pub(super) struct Table {
    turns: Vec<Turn>,
    /// One note slot per eviction; an evicted [`Body::There`]'s `cut` indexes it.
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
    pub(super) fn open(&mut self, id: u64, role: Role, kind: TurnKind, label: String) -> usize {
        self.turns.push(Turn {
            id,
            role,
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
    /// same structure. A resident row holds `seeds`' next entry — its
    /// admissible prefix, read back from the parent's file — and weighs it.
    pub(super) fn install(
        &mut self,
        turns: &[Linked],
        notes: &[Option<String>],
        seeds: Vec<Vec<Recorded<Protocol>>>,
    ) {
        let mut seeds = seeds.into_iter();
        self.turns = turns
            .iter()
            .map(|Linked { row, at }| {
                let at = at.clone();
                let (bytes, body) = match row.held {
                    Held::Resident => {
                        let records = seeds.next().expect("one seed per resident row");
                        let bytes = message_bytes(&resident_messages(&records));
                        (
                            bytes,
                            Body::Here {
                                records,
                                origin: Some(at),
                            },
                        )
                    }
                    Held::Evicted { cut } => (row.bytes, Body::There { at, cut: Some(cut) }),
                    Held::Withdrawn => (row.bytes, Body::There { at, cut: None }),
                };
                Turn {
                    id: row.id,
                    role: row.role,
                    kind: row.kind,
                    label: row.label.clone(),
                    bytes,
                    body,
                }
            })
            .collect();
        self.notes = notes.to_vec();
    }

    /// `/clear` and `/rewind`: every turn from `from` on departs with no cut,
    /// and so with no marker. An evicted turn keeps its address and loses its
    /// cut, which is how a withdrawal leaves no note behind. Notes stay,
    /// since rows before `from` may still index them.
    pub(super) fn withdraw(&mut self, from: u64) {
        self.depart(|id| id >= from, None);
    }

    /// Move the turns `leaving` from here to there, and record the model's
    /// note in the slot the departed rows now index.
    pub(super) fn evict(&mut self, leaving: &[u64], note: Option<String>) {
        let cut = self.notes.len();
        self.depart(|id| leaving.contains(&id), Some(cut));
        self.notes.push(note);
    }

    /// The address a departing turn keeps: its `origin` where it has one, else
    /// this log's own file and the loci its records were measured at.
    fn depart(&mut self, leaving: impl Fn(u64) -> bool, cut: Option<usize>) {
        let source = self.source.clone();
        for turn in self.turns.iter_mut().filter(|turn| leaving(turn.id)) {
            let at = match &mut turn.body {
                Body::Here { records, origin } => origin.take().unwrap_or_else(|| Pointer {
                    source: source.clone(),
                    loci: records
                        .iter()
                        .map(|recorded| recorded.locus().clone())
                        .collect(),
                }),
                Body::There { cut: held, .. } => {
                    *held = cut;
                    continue;
                }
            };
            turn.body = Body::There { at, cut };
        }
    }
}
