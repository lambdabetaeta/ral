//! The turns, and the evictions covering them. The `Vec`s are
//! private to this module, so a turn or an eviction leaves the context only
//! through [`Table::evict`] or [`Table::withdraw`], which stamp where it
//! went, and neither is ever removed.

use super::{
    Body, Eviction, Linked, Location, Pointer, Span, Turn, message_bytes, resident_messages,
};
use crate::record::{AgentId, Recorded, Transcript};
use crate::record::{Role, TurnKind};
use ral_core::own::OwnDir;
use std::cmp::Reverse;

pub(super) struct Table {
    turns: Vec<Turn>,
    /// One per eviction, in creation order. Laminar: any two spans are nested
    /// or disjoint, a later one never strictly inside an earlier one.
    evictions: Vec<(Eviction, Location)>,
    /// Own session, whose `record.jsonl` is where a turn first recorded here
    /// points once it leaves.
    ///
    /// Reading a completed record by byte range from the file this process is
    /// still appending to is safe: a `Locus` exists only once the seam has
    /// written the whole record under its lock.
    source: AgentId,
    /// The run's `sessions/` directory, held: what every pointer's session is
    /// resolved against.
    sessions: OwnDir,
}

/// One item of the frontier or the index: a turn, or an eviction.
#[derive(Clone, Copy)]
pub(super) enum Item<'a> {
    Turn(&'a Turn),
    Eviction(&'a (Eviction, Location)),
}

impl Item<'_> {
    pub(super) fn span(&self) -> Span {
        match self {
            Self::Turn(turn) => Span::single(turn.id),
            Self::Eviction((eviction, _)) => eviction.span,
        }
    }

    /// Where the item is, as the table keeps it.
    pub(super) fn location(&self) -> Location {
        match self {
            Self::Turn(turn) => turn.location(),
            Self::Eviction((_, location)) => *location,
        }
    }

    pub(super) fn is_material(&self) -> bool {
        match self {
            Self::Eviction(_) => true,
            Self::Turn(turn) => !turn.records().is_empty(),
        }
    }

    fn is_resident(&self) -> bool {
        self.location() == Location::Resident
    }
}

/// What holds an item spanning `span` once it has departed, not withdrawn:
/// the smallest of the `later` spans containing it, `later` being those of
/// the evictions recorded after the item. For a turn that is every eviction,
/// since a span takes only turns already recorded; for an eviction, those
/// after it in creation order. Equal spans spell the same address, so no
/// further tie-break is needed.
///
/// The fold never asks: it stamps each item's holder as the item departs.
/// Only a link asks, since it carries no eviction's location — [`Table::install`]
/// derives each one, and the judge checks each row's against it.
pub(super) fn holder(span: Span, later: impl IntoIterator<Item = Span>) -> Option<Span> {
    later
        .into_iter()
        .filter(|&outer| span.within(outer))
        .min_by_key(|outer| outer.width())
}

impl Table {
    pub(super) fn new(source: AgentId, sessions: OwnDir) -> Self {
        Self {
            turns: Vec::new(),
            evictions: Vec::new(),
            source,
            sessions,
        }
    }

    /// A table standing where a fold would have left one, for the parent's
    /// tests: they assert that the projections agree with a structure, which
    /// means stating the structure rather than folding a log to reach it.
    /// Every eviction seeded is resident.
    #[cfg(test)]
    pub(super) fn seeded(
        source: AgentId,
        sessions: OwnDir,
        turns: Vec<Turn>,
        evictions: Vec<Eviction>,
    ) -> Self {
        Self {
            turns,
            evictions: evictions
                .into_iter()
                .map(|eviction| (eviction, Location::Resident))
                .collect(),
            source,
            sessions,
        }
    }

    /// Every turn this lineage recorded, in id order.
    pub(super) fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Every eviction, in creation order.
    pub(super) fn evictions(&self) -> impl Iterator<Item = &Eviction> {
        self.evictions.iter().map(|(eviction, _)| eviction)
    }

    pub(super) fn source(&self) -> AgentId {
        self.source
    }

    pub(super) fn sessions(&self) -> &OwnDir {
        &self.sessions
    }

    pub(super) fn turn(&self, id: u64) -> Option<&Turn> {
        self.turns.iter().find(|turn| turn.id == id)
    }

    /// The widest eviction `id` lies strictly inside of, past its `from`:
    /// what an eviction's end or a rewind's anchor would cut through.
    pub(super) fn straddling(&self, id: u64) -> Option<&Eviction> {
        self.evictions()
            .filter(|eviction| eviction.span.from() < id && id <= eviction.span.to())
            .max_by_key(|eviction| eviction.span.width())
    }

    /// The context: every resident item, in id order. Resident items are
    /// disjoint, so index order is id order.
    pub(super) fn frontier(&self) -> Vec<Item<'_>> {
        self.items().into_iter().filter(Item::is_resident).collect()
    }

    /// Every turn and every eviction: by `from`, a wider span first, a later
    /// eviction first, the turn last.
    pub(super) fn items(&self) -> Vec<Item<'_>> {
        // Stable, and evictions enter newest first: a rewrite precedes what it
        // rewrote.
        let mut items: Vec<Item<'_>> = self
            .evictions
            .iter()
            .rev()
            .map(Item::Eviction)
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
    pub(super) fn absorb(&mut self, at: usize, record: Recorded<Transcript>, bytes: usize) {
        let turn = &mut self.turns[at];
        turn.bytes = turn.bytes.saturating_add(bytes);
        match &mut turn.body {
            Body::Here { records, .. } => records.push(record),
            // `judge` refuses a record landing on a departed row.
            Body::Evicted { .. } | Body::Withdrawn { .. } => {
                unreachable!("a judged record lands on a resident turn")
            }
        }
    }

    /// A link's own step: the parent's rows and evictions become this log's,
    /// so the child's context, survey and index are the same projections of
    /// the same structure. A resident row holds `seeds`' next entry — its
    /// admissible prefix, read back from the parent's file — and weighs it.
    pub(super) fn install(
        &mut self,
        turns: &[Linked],
        evictions: &[Eviction],
        seeds: Vec<Vec<Recorded<Transcript>>>,
    ) {
        let mut seeds = seeds.into_iter();
        self.turns = turns
            .iter()
            .map(|Linked { row, at }| {
                let at = at.clone();
                let (bytes, body) = match row.location {
                    Location::Resident => {
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
                    Location::Evicted(under) => (row.bytes, Body::Evicted { at, under }),
                    Location::Withdrawn => (row.bytes, Body::Withdrawn { at }),
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
        // The link carries no eviction's location, so each is derived as the
        // fold stamped it: withdrawn with its `from`, since an anchor never
        // lies strictly inside a span; else under its holder, if any, since an
        // eviction takes every resident item it covers, and the first later
        // eviction covering an item is the smallest.
        self.evictions = evictions
            .iter()
            .enumerate()
            .map(|(k, eviction)| {
                let withdrawn = self
                    .turn(eviction.span.from())
                    .is_some_and(Turn::is_withdrawn);
                (
                    eviction.clone(),
                    if withdrawn {
                        Location::Withdrawn
                    } else {
                        holder(eviction.span, evictions[k + 1..].iter().map(|r| r.span))
                            .map_or(Location::Resident, Location::Evicted)
                    },
                )
            })
            .collect();
    }

    /// `/clear` and `/rewind`: every turn from `from` on, and every eviction
    /// whose span opens there or later, is withdrawn, whatever eviction held
    /// it; a departed turn keeps its address, and every eviction stays
    /// recorded.
    pub(super) fn withdraw(&mut self, from: u64) {
        self.depart(|span| from <= span.from(), None);
    }

    /// `eviction` stands for its span, resident: every resident turn and
    /// eviction within it departs under it, and one already departed stays
    /// under what holds it. A rewrite is no different: the eviction it
    /// rewrites departs under it.
    pub(super) fn evict(&mut self, eviction: Eviction) {
        self.depart(|span| span.within(eviction.span), Some(eviction.span));
        self.evictions.push((eviction, Location::Resident));
    }

    /// Every item `leaving` names departs under `under`, or is withdrawn when
    /// `None`; one already departed keeps its stamp unless this is a
    /// withdrawal, which dominates.
    /// The address a departing turn keeps is its `origin` where it has one,
    /// else this log's own session and the loci its records were measured at.
    fn depart(&mut self, leaving: impl Fn(Span) -> bool, under: Option<Span>) {
        let location = under.map_or(Location::Withdrawn, Location::Evicted);
        let dominates = under.is_none();
        for (eviction, was) in &mut self.evictions {
            if leaving(eviction.span) && (dominates || *was == Location::Resident) {
                *was = location;
            }
        }
        let source = self.source;
        for turn in self
            .turns
            .iter_mut()
            .filter(|turn| leaving(Span::single(turn.id)))
        {
            let at = match &mut turn.body {
                Body::Here { records, origin } => origin.take().unwrap_or_else(|| Pointer {
                    source,
                    loci: records
                        .iter()
                        .map(|recorded| recorded.locus().clone())
                        .collect(),
                }),
                Body::Evicted { at, .. } | Body::Withdrawn { at } => {
                    if dominates {
                        turn.body = Body::Withdrawn { at: at.clone() };
                    }
                    continue;
                }
            };
            turn.body = match under {
                Some(under) => Body::Evicted { at, under },
                None => Body::Withdrawn { at },
            };
        }
    }
}
