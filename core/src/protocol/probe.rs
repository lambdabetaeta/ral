//! The probe vocabulary: what a front-end may ask of an engine at a run
//! boundary, typed once, and the rows the engine answers in: data, rendered
//! engine-side, never a live handle.
//!
//! An answer outside its probe's shape is the engine breaking the protocol,
//! and severs the carrier that heard it ([`crate::carrier`]).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::capability::GrantStack;
use crate::first_order::datum::{Datum, tag, untag};
use crate::first_order::{Bytes, FOValue, Finite};
use crate::ty::{Ty, Typed, closed_record};
use crate::types::{Done, Owner, Sample, Value};
use crate::{record, variant};

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
    /// The session's own grant stack, without any run's ceiling.
    Grants,
    #[cfg(feature = "test-util")]
    WorkerCount,
}

/// Where a listed worker is in its lifecycle: a consumed worker has left the
/// registry, so no row names one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerState {
    Running,
    Stopping,
    Ready(Done),
}

variant!(typed WorkerState {
    Running: "running",
    Stopping: "stopping",
    Ready(Done): "ready",
});

/// One row of the worker table: data, never a handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRow {
    pub id: u64,
    pub title: String,
    pub owner: Owner,
    pub state: WorkerState,
    /// Unix seconds.
    pub started: i64,
    /// Seconds, so far or in total.
    pub elapsed: Finite,
    /// Unix seconds.
    pub settled: Option<i64>,
    /// Ral calls until a settled row's retention expires; `None` while it
    /// runs, or with no retention armed.  A host figure: the probe's only.
    pub retention_left: Option<u64>,
}

record!(WorkerRow {
    id: "id",
    title: "title",
    owner: "owner",
    state: "state",
    started: "started",
    elapsed: "elapsed",
    settled: "settled",
    retention_left: "retention-left",
});

/// The ral record [`WorkerRow::ral_value`] builds, not the probe's encoding.
impl Typed for WorkerRow {
    fn ty() -> Ty {
        closed_record(&[
            ("id", u64::ty()),
            ("title", String::ty()),
            ("owner", Owner::ty()),
            ("state", WorkerState::ty()),
            ("started", i64::ty()),
            ("elapsed", Finite::ty()),
            ("settled", Option::<i64>::ty()),
        ])
    }
}

impl WorkerRow {
    /// The row as ral reads it: every field but the host's `retention_left`.
    pub fn ral_value(&self) -> Value {
        let FOValue::Map { mut entries } = self.clone().encode() else {
            unreachable!("a record encodes as a map");
        };
        entries.retain(|(k, _)| k != "retention-left");
        Value::from(FOValue::Map { entries })
    }
}

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
    pub state: Sample<()>,
    pub title: String,
}

record!(HandleRow {
    state: "state",
    title: "title",
});

impl Datum for Sample<()> {
    fn encode(self) -> FOValue {
        let label = match self {
            Self::Running(()) => "running",
            Self::Stopping => "stopping",
            Self::Ready(()) => "ready",
            Self::Consumed => "consumed",
        };
        tag(label, None)
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("running", None)) => Ok(Self::Running(())),
            Some(("stopping", None)) => Ok(Self::Stopping),
            Some(("ready", None)) => Ok(Self::Ready(())),
            Some(("consumed", None)) => Ok(Self::Consumed),
            _ => Err(format!(
                "expected `running, `stopping, `ready or `consumed, got {}",
                v.shape()
            )),
        }
    }
}

/// Opaque to ral, as a seed is: the stack crosses in the codec `Run.caps` rides.
impl Datum for GrantStack {
    fn encode(self) -> FOValue {
        Bytes::from(serde_json::to_vec(&self).expect("a grant stack is plain data")).encode()
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        serde_json::from_slice(&Bytes::decode(v)?)
            .map_err(|e| format!("expected an encoded grant stack: {e}"))
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
        assert!(why.contains("`title"), "{why}");
    }

    /// A grant stack crosses a probe layer for layer, exec keys and all.
    #[test]
    fn a_grant_stack_round_trips() {
        use crate::capability::{Capabilities, Evidence, ExecGrant, ExecKey, Net};
        let mut stack = GrantStack::root();
        stack.push(Capabilities {
            exec: Some(ExecGrant::from_iter([(
                ExecKey::Name("git".into()),
                Evidence::ALLOW,
            )])),
            net: Some(Net::Offline),
            ..Capabilities::root()
        });
        assert_eq!(GrantStack::decode(&stack.clone().encode()), Ok(stack));
    }

    /// Every row a new reading answers in survives its own round trip.
    #[test]
    fn rows_round_trip() {
        for state in [
            Sample::Running(()),
            Sample::Stopping,
            Sample::Ready(()),
            Sample::Consumed,
        ] {
            let row = BindingRow {
                name: "h".into(),
                scheme: Some("Int".into()),
                preview: "1".into(),
                handle: Some(HandleRow {
                    state,
                    title: "block".into(),
                }),
            };
            assert_eq!(BindingRow::decode(&row.clone().encode()), Ok(row));
        }
        for state in [
            WorkerState::Running,
            WorkerState::Stopping,
            WorkerState::Ready(Done::Ok),
        ] {
            let row = WorkerRow {
                id: 1,
                title: "job".into(),
                owner: Owner::Session,
                state,
                started: 100,
                elapsed: Finite::new(0.5).unwrap(),
                settled: Some(101),
                retention_left: Some(2),
            };
            assert_eq!(WorkerRow::decode(&row.clone().encode()), Ok(row));
        }
    }
}
