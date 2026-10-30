//! A span of turn ids: what an eviction stands for, what an item is named by,
//! and what an eviction takes.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A run of turn ids `[from, to]`, both ends inclusive; a turn's own span is
/// `from == to`. Never inverted: [`Span::new`] is the one way to name one,
/// and a recorded one is refused on the way in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Ends")]
pub struct Span {
    from: u64,
    to: u64,
}

/// A span's serialized form, admitted only through [`Span::new`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ends {
    from: u64,
    to: u64,
}

/// A run named last turn first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvertedSpan {
    from: u64,
    to: u64,
}

impl fmt::Display for InvertedSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { from, to } = self;
        write!(
            f,
            "`from` is {from} and `to` is {to}: a run names its first turn, then its last"
        )
    }
}

impl TryFrom<Ends> for Span {
    type Error = InvertedSpan;

    fn try_from(Ends { from, to }: Ends) -> Result<Self, InvertedSpan> {
        Self::new(from, to)
    }
}

impl Span {
    /// The run from turn `from` through turn `to`.
    ///
    /// # Errors
    /// Refuses `from > to`.
    pub fn new(from: u64, to: u64) -> Result<Self, InvertedSpan> {
        if from <= to {
            Ok(Self { from, to })
        } else {
            Err(InvertedSpan { from, to })
        }
    }

    /// Turn `id`'s own span.
    #[must_use]
    pub fn single(id: u64) -> Self {
        Self { from: id, to: id }
    }

    #[must_use]
    pub fn from(self) -> u64 {
        self.from
    }

    #[must_use]
    pub fn to(self) -> u64 {
        self.to
    }

    /// Whether the span is one turn's.
    #[must_use]
    pub fn is_single(self) -> bool {
        self.from == self.to
    }

    #[must_use]
    pub fn contains(self, id: u64) -> bool {
        (self.from..=self.to).contains(&id)
    }

    /// Whether this span lies within `outer`, equal spans included.
    #[must_use]
    pub fn within(self, outer: Self) -> bool {
        outer.from <= self.from && self.to <= outer.to
    }

    /// How many turns past `from` the span reaches.
    #[must_use]
    pub fn width(self) -> u64 {
        self.to - self.from
    }

    /// The least span covering both.
    #[must_use]
    pub fn hull(self, other: Self) -> Self {
        Self {
            from: self.from.min(other.from),
            to: self.to.max(other.to),
        }
    }

    /// The span as a sentence opens with it: `Turn 12`, or `Turns 12–40`.
    #[must_use]
    pub fn capitalised(self) -> impl fmt::Display {
        Spelled {
            span: self,
            nouns: ("Turn", "Turns"),
        }
    }
}

/// `turn 12`, or `turns 12–40`: the one spelling of a span.
impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Spelled {
            span: *self,
            nouns: ("turn", "turns"),
        }
        .fmt(f)
    }
}

/// A span spelled under one of its nouns, singular or plural.
struct Spelled {
    span: Span,
    nouns: (&'static str, &'static str),
}

impl fmt::Display for Spelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            span: Span { from, to },
            nouns: (one, many),
        } = self;
        if from == to {
            write!(f, "{one} {from}")
        } else {
            write!(f, "{many} {from}–{to}")
        }
    }
}
