//! A checked boundary's type, frozen as data: the graph a [`Site`] is.
//!
//! The walk that builds it and the door that admits against it are in
//! `types::site`; this is the vocabulary, with no `Value` or unifier in it.

use crate::source::Span;
use crate::ty::Kind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

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
    Handle(Id),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Fixed {
    pub(crate) side: Side,
    pub(crate) pointer: String,
}

/// What the unit's `comparable` variables were fixed to.
///
/// One per unit, shared by every [`Site`] the unit builds, so two decodes the
/// program treats as one `comparable` type agree for as long as the compiled
/// program lives.  A `SerialThunk` ships a copy to an engine child; what the
/// child fixes stays there.
#[derive(Debug)]
pub struct Fixings {
    pub(crate) id: u64,
    pub(crate) table: Mutex<HashMap<u32, Fixed>>,
}

impl Fixings {
    pub(crate) fn new() -> Arc<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id =
            u64::from(std::process::id()) << 32 | u64::from(NEXT.fetch_add(1, Ordering::Relaxed));
        Arc::new(Self {
            id,
            table: Mutex::default(),
        })
    }
}

impl PartialEq for Fixings {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

/// Wire form of a unit's fixings: the table, under its id, so every site of
/// one unit that reaches a process meets the one `Arc` there.
mod shared {
    use super::{Fixed, Fixings, HashMap, Mutex, OnceLock, Weak};
    use crate::sync::LockExt as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::sync::Arc;

    type Live = Mutex<HashMap<u64, Weak<Fixings>>>;

    fn live() -> &'static Live {
        static LIVE: OnceLock<Live> = OnceLock::new();
        LIVE.get_or_init(Live::default)
    }

    pub(super) fn serialize<S: Serializer>(
        fixings: &Arc<Fixings>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let table = fixings.table.lock_ignore_poison();
        (fixings.id, &*table).serialize(serializer)
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "lookup and registration are one step under the lock"
    )]
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<Fixings>, D::Error> {
        let (id, table) = <(u64, HashMap<u32, Fixed>)>::deserialize(deserializer)?;
        let mut live = live().lock_ignore_poison();
        live.retain(|_, weak| weak.strong_count() > 0);
        if let Some(known) = live.get(&id).and_then(Weak::upgrade) {
            return Ok(known);
        }
        let fixings = Arc::new(Fixings {
            id,
            table: Mutex::new(table),
        });
        live.insert(id, Arc::downgrade(&fixings));
        Ok(fixings)
    }
}

/// The type a boundary's result was solved at, and what a door needs to admit
/// a value against it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Site {
    pub(crate) nodes: Vec<Node>,
    pub(crate) root: Id,
    #[serde(with = "shared")]
    pub(crate) fixings: Arc<Fixings>,
}
