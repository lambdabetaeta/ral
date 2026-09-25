//! The rows readings answer in: data, rendered engine-side, never a live
//! handle.

use crate::record;
use crate::serial::FOValue;
use crate::serial::datum::{Datum, tag, untag};
use crate::sync::LockExt as _;
use crate::types::{HandleState, LeaseClass, Shell, Value, WorkerEntry};

/// One row of the worker table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRow {
    pub id: u64,
    pub cmd: String,
    pub class: LeaseClass,
    pub running: bool,
    pub up_secs: u64,
    pub idle_secs: u64,
    /// The ral-call epoch at which the row was first seen settled.
    pub settled_epoch: Option<u64>,
}

record!(WorkerRow {
    id: "id",
    cmd: "cmd",
    class: "class",
    running: "running",
    up_secs: "up-secs",
    idle_secs: "idle-secs",
    settled_epoch: "settled-epoch",
});

impl WorkerRow {
    pub(super) fn of(entry: &WorkerEntry) -> Self {
        Self {
            id: entry.id.0,
            cmd: entry.cmd.clone(),
            class: entry.class,
            running: *entry.handle.state.lock_ignore_poison() == HandleState::Running,
            up_secs: entry.started.elapsed().unwrap_or_default().as_secs(),
            idle_secs: entry
                .handle
                .last_observed
                .lock_ignore_poison()
                .elapsed()
                .as_secs(),
            settled_epoch: entry.settled_epoch,
        }
    }
}

impl Datum for LeaseClass {
    fn encode(self) -> FOValue {
        match self {
            Self::Worker => "worker",
            Self::Durable => "durable",
        }
        .to_string()
        .encode()
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match v.as_str() {
            Some("worker") => Ok(Self::Worker),
            Some("durable") => Ok(Self::Durable),
            _ => Err(format!(
                "expected \"worker\" or \"durable\", got {}",
                v.shape()
            )),
        }
    }
}

/// One binding in scope, as a worksheet shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingRow {
    pub name: String,
    pub scheme: Option<String>,
    /// One line, truncated.
    pub preview: String,
    pub handle: Option<HandleRow>,
}

record!(BindingRow {
    name: "name",
    scheme: "scheme",
    preview: "preview",
    handle: "handle",
});

/// A bound worker handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleRow {
    pub state: HandleState,
    pub cmd: String,
}

record!(HandleRow {
    state: "state",
    cmd: "cmd",
});

impl BindingRow {
    /// Every binding in scope, a shadowed name once.
    pub(super) fn all(shell: &Shell) -> Vec<Self> {
        shell
            .env
            .fold_union(|b| {
                let handle = match &b.value {
                    Value::Handle(h) => Some(HandleRow {
                        state: *h.state.lock_ignore_poison(),
                        cmd: h.cmd.clone(),
                    }),
                    _ => None,
                };
                (
                    b.scheme.as_deref().map(crate::typecheck::fmt_scheme),
                    preview(&b.value),
                    handle,
                )
            })
            .into_iter()
            .map(|(name, (scheme, preview, handle))| Self {
                name,
                scheme,
                preview,
                handle,
            })
            .collect()
    }
}

fn preview(v: &Value) -> String {
    const CAP: usize = 40;
    let s = v.to_string().replace('\n', " ");
    if s.chars().count() <= CAP {
        return s;
    }
    s.chars().take(CAP - 1).chain(['…']).collect()
}

impl Datum for HandleState {
    fn encode(self) -> FOValue {
        let label = match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        };
        tag(label, None)
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("running", None)) => Ok(Self::Running),
            Some(("completed", None)) => Ok(Self::Completed),
            Some(("cancelled", None)) => Ok(Self::Cancelled),
            _ => Err(format!(
                "expected `running, `completed or `cancelled, got {}",
                v.shape()
            )),
        }
    }
}

/// The names a completer offers beside the builtins, which have their own
/// class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionNames {
    pub bindings: Vec<String>,
    pub handlers: Vec<String>,
}

record!(CompletionNames {
    bindings: "bindings",
    handlers: "handlers",
});

impl CompletionNames {
    pub(super) fn of(shell: &Shell) -> Self {
        let sorted = |mut names: Vec<String>| {
            names.sort();
            names.dedup();
            names
        };
        Self {
            bindings: sorted(
                shell
                    .env
                    .fold_union(|_| ())
                    .into_iter()
                    .map(|(n, ())| n)
                    .collect(),
            ),
            handlers: sorted(shell.handler_names().map(str::to_string).collect()),
        }
    }
}

/// What a source text types as, stage by stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spine {
    /// A pipeline: one typed row per stage.
    Stages(Vec<SpineStage>),
    TypeError(SpineError),
    /// Blank, still being typed, or no pipeline.
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpineStage {
    pub src: String,
    pub ty: String,
}

record!(SpineStage {
    src: "src",
    ty: "ty"
});

/// The first type error, in the words its full report uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpineError {
    /// A half-open char range into the source.
    pub span: Option<(usize, usize)>,
    pub code: String,
    pub headline: String,
    pub label: String,
    pub hint: Option<String>,
}

record!(SpineError {
    span: "span",
    code: "code",
    headline: "headline",
    label: "label",
    hint: "hint",
});

impl Datum for Spine {
    fn encode(self) -> FOValue {
        match self {
            Self::Stages(stages) => tag("stages", Some(stages.encode())),
            Self::TypeError(error) => tag("type-error", Some(error.encode())),
            Self::Empty => tag("empty", None),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("stages", Some(stages))) => Datum::decode(stages).map(Self::Stages),
            Some(("type-error", Some(error))) => Datum::decode(error).map(Self::TypeError),
            Some(("empty", None)) => Ok(Self::Empty),
            _ => Err(format!(
                "expected `stages, `type-error or `empty, got {}",
                v.shape()
            )),
        }
    }
}

/// A top-level `let`'s effect verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindEffect {
    pub name: String,
    pub effectful: bool,
}

record!(BindEffect {
    name: "name",
    effectful: "effectful",
});

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    pub name: String,
    pub dir: bool,
    pub exec: bool,
}

record!(PathEntry {
    name: "name",
    dir: "dir",
    exec: "exec",
});

#[cfg(test)]
mod tests {
    use super::*;

    /// A row missing a field is named, never defaulted.
    #[test]
    fn an_ill_shaped_worker_row_names_its_field() {
        let why = WorkerRow::decode(&FOValue::Map {
            entries: vec![("id".into(), FOValue::Int { value: 1 })],
        })
        .expect_err("a row with only an id");
        assert!(why.contains("`cmd"), "{why}");
    }

    /// Every row a new reading answers in survives its own round trip.
    #[test]
    fn rows_round_trip() {
        let spine = Spine::TypeError(SpineError {
            span: Some((1, 3)),
            code: "E1".into(),
            headline: "h".into(),
            label: "l".into(),
            hint: None,
        });
        assert_eq!(Spine::decode(&spine.clone().encode()), Ok(spine));
        let row = BindingRow {
            name: "h".into(),
            scheme: Some("Int".into()),
            preview: "1".into(),
            handle: Some(HandleRow {
                state: HandleState::Cancelled,
                cmd: "block".into(),
            }),
        };
        assert_eq!(BindingRow::decode(&row.clone().encode()), Ok(row));
    }
}
