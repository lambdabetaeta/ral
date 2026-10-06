//! Binding patterns, shared by `let` and lambda parameters.

use super::Name;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

/// Binding pattern. There is no alternative to fall through to, so a shape
/// mismatch at bind time is an error rather than a failure to match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Pattern {
    /// `_` — discard the value.
    Wildcard,
    Name(Name),
    /// `[a, b, ...rest]`, where `rest` takes the tail as a new list.
    List {
        elems: Vec<Self>,
        rest: Option<Name>,
    },
    /// `[key: pat, …]`
    Map(Vec<MapPatternEntry>),
}

/// One entry of a [`Pattern::Map`]: a static key and the sub-pattern bound to
/// that field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapPatternEntry {
    pub(crate) key: String,
    pub(crate) pattern: Pattern,
}

impl Pattern {
    /// Every name this pattern binds, in pattern order.
    pub(crate) fn names(&self) -> Vec<&Name> {
        fn walk<'a>(pat: &'a Pattern, out: &mut Vec<&'a Name>) {
            match pat {
                Pattern::Wildcard => {}
                Pattern::Name(n) => out.push(n),
                Pattern::List { elems, rest } => {
                    for e in elems {
                        walk(e, out);
                    }
                    out.extend(rest);
                }
                Pattern::Map(entries) => {
                    for e in entries {
                        walk(&e.pattern, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(self, &mut out);
        out
    }

    /// The first name this pattern binds twice, if any. A pattern binds all
    /// its names at once, so a repeat is an ambiguity, not a shadow — the
    /// parser rejects it at both binder sites (`let` and lambda parameter).
    pub(crate) fn duplicate_name(&self) -> Option<&Name> {
        let mut seen = HashSet::new();
        self.names().into_iter().find(|n| !seen.insert(*n))
    }
}

/// The pattern as written: `_`, `x`, `[a b ...rest]`, `[host port: p]`.
impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wildcard => f.write_str("_"),
            Self::Name(n) => f.write_str(n),
            Self::List { elems, rest } => {
                let parts = elems
                    .iter()
                    .map(ToString::to_string)
                    .chain(rest.iter().map(|r| format!("...{r}")));
                write!(f, "[{}]", parts.collect::<Vec<_>>().join(" "))
            }
            Self::Map(entries) => {
                let parts = entries.iter().map(|e| match &e.pattern {
                    Self::Name(n) if **n == *e.key => e.key.clone(),
                    p => format!("{}: {p}", e.key),
                });
                write!(f, "[{}]", parts.collect::<Vec<_>>().join(" "))
            }
        }
    }
}
