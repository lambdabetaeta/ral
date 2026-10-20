//! The turns, and the residues evictions left over them. The `Vec`s are
//! private to this module, so a turn or a residue leaves the context only
//! through [`Table::evict`] or [`Table::withdraw`], which stamp where it
//! went, and neither is ever removed.

use super::{
    Body, Departure, Linked, Location, Pointer, Residue, Span, Standing, Turn, message_bytes,
    resident_messages,
};
use crate::record::{Protocol, Recorded};
use crate::record::{Role, TurnKind};
use std::cmp::Reverse;
use std::path::{Path, PathBuf};

pub(super) struct Table {
    turns: Vec<Turn>,
    /// One per eviction, in creation order. Laminar: any two spans are nested
    /// or disjoint, a later one never strictly inside an earlier one.
    residues: Vec<Standing>,
    /// Own `record.jsonl`: where a turn first recorded here points once it
    /// leaves.
    ///
    /// Reading a completed record by byte range from the file this process is
    /// still appending to is safe: a `Locus` exists only once the seam has
    /// written the whole record under its lock.
    source: PathBuf,
}

/// One item of the frontier or the index: a turn, or a residue.
#[derive(Clone, Copy)]
pub(super) enum Item<'a> {
    Turn(&'a Turn),
    Residue(&'a Standing),
}

impl Item<'_> {
    pub(super) fn span(&self) -> Span {
        match self {
            Self::Turn(turn) => Span::single(turn.id),
            Self::Residue(standing) => standing.residue.span,
        }
    }

    /// Where the item is, as the table keeps it.
    pub(super) fn location(&self) -> Location {
        match self {
            Self::Turn(turn) => turn.location(),
            Self::Residue(standing) => standing.location(),
        }
    }

    fn is_resident(&self) -> bool {
        self.location() == Location::Resident
    }
}

/// What holds an item spanning `span` once it has departed, not withdrawn:
/// the smallest of the `later` spans containing it, `later` being those of
/// the residues recorded after the item. For a turn that is every residue,
/// since a span takes only turns already recorded; for a residue, those
/// after it in creation order. Equal spans spell the same address, so no
/// further tie-break is needed.
///
/// The fold never asks: it stamps each item's holder as the item departs.
/// Only a link asks, since it carries no residue's location — [`Table::install`]
/// derives each one, and the judge checks each row's against it.
pub(super) fn holder(span: Span, later: impl IntoIterator<Item = Span>) -> Option<Span> {
    later
        .into_iter()
        .filter(|&outer| span.within(outer))
        .min_by_key(|outer| outer.width())
}

impl Table {
    pub(super) fn new(source: PathBuf) -> Self {
        Self {
            turns: Vec::new(),
            residues: Vec::new(),
            source,
        }
    }

    /// A table standing where a fold would have left one, for the parent's
    /// tests: they assert that the projections agree with a structure, which
    /// means stating the structure rather than folding a log to reach it.
    /// Every residue seeded is resident.
    #[cfg(test)]
    pub(super) fn seeded(source: PathBuf, turns: Vec<Turn>, residues: Vec<Residue>) -> Self {
        Self {
            turns,
            residues: residues
                .into_iter()
                .map(|residue| Standing {
                    residue,
                    departure: None,
                })
                .collect(),
            source,
        }
    }

    /// Every turn this lineage recorded, in id order.
    pub(super) fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Every residue, in creation order.
    pub(super) fn residues(&self) -> impl Iterator<Item = &Residue> {
        self.residues.iter().map(|standing| &standing.residue)
    }

    pub(super) fn source(&self) -> &Path {
        &self.source
    }

    pub(super) fn turn(&self, id: u64) -> Option<&Turn> {
        self.turns.iter().find(|turn| turn.id == id)
    }

    /// The widest residue `id` lies strictly inside of, past its `from`:
    /// what an eviction's end or a rewind's anchor would cut through.
    pub(super) fn straddling(&self, id: u64) -> Option<&Residue> {
        self.residues()
            .filter(|residue| residue.span.from() < id && id <= residue.span.to())
            .max_by_key(|residue| residue.span.width())
    }

    /// The context: every resident item, in id order. Resident items are
    /// disjoint, so index order is id order.
    pub(super) fn frontier(&self) -> Vec<Item<'_>> {
        self.items().into_iter().filter(Item::is_resident).collect()
    }

    /// Every turn and every residue: by `from`, a wider span first, a later
    /// residue first, the turn last.
    pub(super) fn items(&self) -> Vec<Item<'_>> {
        // Stable, and residues enter newest first: a rewrite precedes what it
        // rewrote.
        let mut items: Vec<Item<'_>> = self
            .residues
            .iter()
            .rev()
            .map(Item::Residue)
            .chain(self.turns.iter().map(Item::Turn))
            .collect();
        items.sort_by_key(|item| {
            let span = item.span();
            (
                span.from(),
                Reverse(span.to()),
                matches!(item, Item::Turn(_)),
            )
        });
        items
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

    /// A link's own step: the parent's rows and residues become this log's,
    /// so the child's context, survey and index are the same projections of
    /// the same structure. A resident row holds `seeds`' next entry — its
    /// admissible prefix, read back from the parent's file — and weighs it.
    pub(super) fn install(
        &mut self,
        turns: &[Linked],
        residues: &[Residue],
        seeds: Vec<Vec<Recorded<Protocol>>>,
    ) {
        let mut seeds = seeds.into_iter();
        self.turns = turns
            .iter()
            .map(|Linked { row, at }| {
                let at = at.clone();
                let (bytes, body) = match row.location.departure() {
                    None => {
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
                    Some(departure) => (row.bytes, Body::There { at, departure }),
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
        // The link carries no residue's location, so each is derived as the
        // fold stamped it: withdrawn with its `from`, since an anchor never
        // lies strictly inside a span; else under its holder, if any, since an
        // eviction takes every resident item it covers, and the first later
        // residue covering an item is the smallest.
        self.residues = residues
            .iter()
            .enumerate()
            .map(|(k, residue)| {
                let withdrawn = self
                    .turn(residue.span.from())
                    .is_some_and(Turn::is_withdrawn);
                Standing {
                    residue: residue.clone(),
                    departure: if withdrawn {
                        Some(Departure::Withdrawn)
                    } else {
                        holder(residue.span, residues[k + 1..].iter().map(|r| r.span))
                            .map(Departure::Under)
                    },
                }
            })
            .collect();
    }

    /// `/clear` and `/rewind`: every turn from `from` on, and every residue
    /// whose span opens there or later, is withdrawn, whatever residue held
    /// it; a departed turn keeps its address, and every residue stays
    /// recorded.
    pub(super) fn withdraw(&mut self, from: u64) {
        self.depart(|span| from <= span.from(), Departure::Withdrawn);
    }

    /// `residue` stands for its span, resident: every resident turn and
    /// residue within it departs under it, and one already departed stays
    /// under what holds it. A rewrite is no different: the residue it
    /// rewrites departs under it.
    pub(super) fn evict(&mut self, residue: Residue) {
        self.depart(
            |span| span.within(residue.span),
            Departure::Under(residue.span),
        );
        self.residues.push(Standing {
            residue,
            departure: None,
        });
    }

    /// Every item `leaving` names departs, stamped `departure`; one already
    /// departed keeps its stamp unless this is a withdrawal, which dominates.
    /// The address a departing turn keeps is its `origin` where it has one,
    /// else this log's own file and the loci its records were measured at.
    fn depart(&mut self, leaving: impl Fn(Span) -> bool, departure: Departure) {
        let dominates = departure == Departure::Withdrawn;
        for standing in self
            .residues
            .iter_mut()
            .filter(|standing| leaving(standing.residue.span))
        {
            if dominates || standing.departure.is_none() {
                standing.departure = Some(departure);
            }
        }
        let source = &self.source;
        for turn in self
            .turns
            .iter_mut()
            .filter(|turn| leaving(Span::single(turn.id)))
        {
            let at = match &mut turn.body {
                Body::Here { records, origin } => origin.take().unwrap_or_else(|| Pointer {
                    source: source.clone(),
                    loci: records
                        .iter()
                        .map(|recorded| recorded.locus().clone())
                        .collect(),
                }),
                Body::There { departure: was, .. } => {
                    if dominates {
                        *was = departure;
                    }
                    continue;
                }
            };
            turn.body = Body::There { at, departure };
        }
    }
}
