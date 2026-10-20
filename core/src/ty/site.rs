//! A checked boundary's type, frozen as data: the graph a [`Site`] is.
//!
//! The walk that builds it and the door that admits against it are in
//! `types::site`; this is the vocabulary, with no `Value` or unifier in it.

use crate::source::Span;
use crate::sync::LockExt as _;
use crate::ty::Kind;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

pub(crate) type Id = usize;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Node {
    pub(crate) shape: Shape,
    /// The use that imposed this structure, or this kind.
    pub(crate) witness: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Shape {
    Unit,
    Bytes,
    Bool,
    Int,
    Float,
    String,
    List(Id),
    Map(Id),
    Handle(GradeShape, Id),
    Record(Fields),
    Variant(Fields),
    Thunk(Id),
    Free { var: u32, kind: Kind },
    Return(GradeShape, Id),
    Fun(Id, Id),
    FreeComp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum GradeShape {
    Value,
    Output,
    Free,
}

/// A row's labels in first-appearance order, and how it ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Fields {
    pub(crate) labels: Vec<(String, Id)>,
    pub(crate) tail: Tail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Tail {
    Closed,
    Open { deep: bool },
}

/// What a `comparable` variable was fixed to: the one run-time operation that
/// fails on two admissible values of different types is ordering a number
/// against text, so that is the one distinction a door must keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Side {
    Number,
    Text,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Number => "a number",
            Self::Text => "text",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Fixed {
    pub(crate) side: Side,
    pub(crate) pointer: String,
}

/// A compiled unit's name: unique in every process that meets it.  A baked
/// unit's id is minted in the build script and travels with its IR, a seed's
/// with its closures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct UnitId {
    process: u64,
    seq: u64,
}

impl UnitId {
    pub(crate) fn fresh() -> Self {
        static PROCESS: OnceLock<u64> = OnceLock::new();
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let process = *PROCESS.get_or_init(|| {
            let mut nonce = [0; 8];
            getrandom::fill(&mut nonce).expect("OS randomness");
            u64::from_le_bytes(nonce)
        });
        Self {
            process,
            seq: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// What a session's `comparable` variables were fixed to, per unit: shared
/// by the session's threads, snapshotted into a new session.
#[derive(Debug, Default)]
pub struct Fixings(pub(crate) Mutex<HashMap<(UnitId, u32), Fixed>>);

impl Fixings {
    pub(crate) fn snapshot(&self) -> Self {
        Self(Mutex::new(self.0.lock_ignore_poison().clone()))
    }
}

impl Serialize for Fixings {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let table = self.0.lock_ignore_poison();
        serializer.collect_seq(table.iter().map(|(&(unit, var), fixed)| (unit, var, fixed)))
    }
}

impl<'de> Deserialize<'de> for Fixings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let triples = Vec::<(UnitId, u32, Fixed)>::deserialize(deserializer)?;
        Ok(Self(Mutex::new(
            triples
                .into_iter()
                .map(|(unit, var, fixed)| ((unit, var), fixed))
                .collect(),
        )))
    }
}

/// The type a boundary's result was solved at, and what a door needs to admit
/// a value against it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Site {
    pub(crate) nodes: Vec<Node>,
    pub(crate) root: Id,
    pub(crate) unit: UnitId,
}
