//! Resource probe rows and their card rendering.

use super::{Field, FieldVal, Mark, Role, Span};
use std::fmt;

/// How an accumulator is kept bounded — or that it is not.  Stated even where
/// the enforcement lands later, the row then carrying `cap: None` and a note
/// saying so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Policy {
    Coalesce,
    Queue,
    Reject,
    Evict,
    Reap,
    Warn,
    Unbounded,
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Coalesce => "coalesce",
            Self::Queue => "queue",
            Self::Reject => "reject",
            Self::Evict => "evict",
            Self::Reap => "reap",
            Self::Warn => "warn",
            Self::Unbounded => "none (unbounded)",
        })
    }
}

/// One probed accumulator, one row per figure: what the fold renders.  A probe
/// fold is an interactive diagnostic, read when it is run; no session keeps a
/// pressure history.
#[derive(Clone, Debug)]
pub(crate) struct ProbeRow {
    pub name: String,
    /// Its size now, in the unit the name implies (a count, bytes, seconds).
    pub current: u64,
    /// The enforced bound, when one is armed; `None` for a decided-but-
    /// unenforced cap and for a genuinely unbounded figure alike.
    pub cap: Option<u64>,
    pub policy: Policy,
    /// A free clause: the nearest time-to-reap, the probed path.
    pub note: Option<String>,
}

impl ProbeRow {
    pub(crate) fn new(
        name: impl Into<String>,
        current: u64,
        cap: Option<u64>,
        policy: Policy,
        note: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            current,
            cap,
            policy,
            note,
        }
    }
}

/// Render `rows` as one aligned [`Mark::Fields`] matrix: the figure (with
/// `/cap` when one is armed), then policy and note as muted ink.
pub(crate) fn rows_mark(rows: &[ProbeRow]) -> Mark {
    let fields = rows
        .iter()
        .map(|row| {
            let mut spans = Vec::new();
            let figure = match row.cap {
                Some(cap) => format!("{}/{}", row.current, cap),
                None => row.current.to_string(),
            };
            spans.push(Span {
                role: None,
                text: figure,
            });
            spans.push(Span {
                role: Some(Role::Muted),
                text: format!("  {}", row.policy),
            });
            if let Some(note) = &row.note {
                spans.push(Span {
                    role: Some(Role::Muted),
                    text: format!(" ({note})"),
                });
            }
            Field {
                label: row.name.clone(),
                value: FieldVal::Inline(spans),
            }
        })
        .collect();
    Mark::Fields { rows: fields }
}
