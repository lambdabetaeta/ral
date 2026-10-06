//! The probe vocabulary: what a front-end may ask of an engine at a run
//! boundary, typed once, and the rows the engine answers in: data, rendered
//! engine-side, never a live handle.
//!
//! An answer outside its probe's shape is the engine breaking the protocol,
//! and severs the carrier that heard it ([`crate::carrier`]).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::first_order::FOValue;
use crate::first_order::datum::{Datum, tag, untag};
use crate::record;
use crate::types::{HandleState, LeaseClass};

/// One read of session state: the payload of `Frame::Probe`.
///
/// Each variant states what it takes; a payload its probe does not take is
/// not representable, so the engine has nothing to refuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Probe {
    BindingCount,
    LeasedBindingCount,
    LargestBindingBytes,
    /// One variable of the engine's environment overlay.
    EnvVar(String),
    Cwd,
    Home,
    BuiltinNames,
    /// The recursive byte size of a path, resolved against the engine's cwd.
    PathBytes(PathBuf),
    Workers,
    SessionEnded,
    CompletionNames,
    Bindings,
    /// The entries of a directory, resolved against the engine's cwd.
    PathEntries(PathBuf),
    #[cfg(feature = "test-util")]
    WorkerCount,
    #[cfg(feature = "test-util")]
    GrantDepth,
}

/// One row of the worker table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRow {
    pub id: u64,
    pub cmd: String,
    pub class: LeaseClass,
    pub running: bool,
    pub up_secs: u64,
    pub idle_secs: u64,
    /// Ral calls until a settled row's retention expires; `None` while it
    /// runs, or with no retention armed.
    pub retention_left: Option<u64>,
}

record!(WorkerRow {
    id: "id",
    cmd: "cmd",
    class: "class",
    running: "running",
    up_secs: "up-secs",
    idle_secs: "idle-secs",
    retention_left: "retention-left",
});

/// One binding in scope, rendered.
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
/// probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionNames {
    pub bindings: Vec<String>,
    pub handlers: Vec<String>,
}

record!(CompletionNames {
    bindings: "bindings",
    handlers: "handlers",
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

    /// Every probe survives the frame it rides, payload and all.
    #[test]
    fn a_probe_crosses_a_frame_whole() {
        use crate::protocol::{DispatchId, Frame};
        for probe in [
            Probe::Cwd,
            Probe::EnvVar("HOME".into()),
            Probe::PathBytes(PathBuf::from("a/b")),
        ] {
            let frame = Frame::Probe(DispatchId(1), probe);
            let json = serde_json::to_string(&frame).expect("a frame encodes");
            assert_eq!(
                serde_json::from_str::<Frame>(&json).expect("it decodes"),
                frame
            );
        }
    }

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
