//! The probe vocabulary: what a front-end may ask of an engine at a run
//! boundary, typed once, and the rows the engine answers in: data, rendered
//! engine-side, never a live handle.
//!
//! An answer outside its probe's shape is the engine breaking the protocol,
//! and severs the carrier that heard it ([`crate::carrier`]).

use crate::fact::{DecodeCause, DecodeFailure, JsonInputFailure};
use crate::ir::ValClos;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::authority::grant::value::LayerValue;
use crate::authority::policy::{GrantLayer, GrantStack};
use crate::data::datum::Datum;
use crate::data::{Bytes, FOValue, Finite};
use crate::handle::Sample;
use crate::shell::runtime::{Done, Owner};
use crate::ty::{Ty, Typed, closed_record};
use crate::{memoized_ty, record, variant};

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
    /// The session's grant stack as data: each layer's value, never a
    /// position or an identity.
    Grants,
    #[cfg(feature = "test-util")]
    WorkerCount,
    /// How many layers the shell's stack holds, the ambient root included:
    /// what a test reads to see a frame leak.
    #[cfg(feature = "test-util")]
    GrantDepth,
}

/// Paths are JSON strings; decoding never inspects the filesystem.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum RawProbe {
    BindingCount,
    LeasedBindingCount,
    LargestBindingBytes,
    EnvVar(String),
    Cwd,
    Home,
    BuiltinNames,
    PathBytes(String),
    Workers,
    SessionEnded,
    CompletionNames,
    Bindings,
    PathEntries(String),
    Grants,
    #[cfg(feature = "test-util")]
    WorkerCount,
    #[cfg(feature = "test-util")]
    GrantDepth,
}

impl RawProbe {
    pub(crate) fn validate(self) -> Probe {
        match self {
            Self::BindingCount => Probe::BindingCount,
            Self::LeasedBindingCount => Probe::LeasedBindingCount,
            Self::LargestBindingBytes => Probe::LargestBindingBytes,
            Self::EnvVar(value) => Probe::EnvVar(value),
            Self::Cwd => Probe::Cwd,
            Self::Home => Probe::Home,
            Self::BuiltinNames => Probe::BuiltinNames,
            Self::PathBytes(value) => Probe::PathBytes(value.into()),
            Self::Workers => Probe::Workers,
            Self::SessionEnded => Probe::SessionEnded,
            Self::CompletionNames => Probe::CompletionNames,
            Self::Bindings => Probe::Bindings,
            Self::PathEntries(value) => Probe::PathEntries(value.into()),
            Self::Grants => Probe::Grants,
            #[cfg(feature = "test-util")]
            Self::WorkerCount => Probe::WorkerCount,
            #[cfg(feature = "test-util")]
            Self::GrantDepth => Probe::GrantDepth,
        }
    }

    pub(crate) fn from_value(value: &Probe) -> Result<Self, crate::fact::JsonEncodeFailure> {
        Ok(match value {
            Probe::BindingCount => Self::BindingCount,
            Probe::LeasedBindingCount => Self::LeasedBindingCount,
            Probe::LargestBindingBytes => Self::LargestBindingBytes,
            Probe::EnvVar(value) => Self::EnvVar(value.clone()),
            Probe::Cwd => Self::Cwd,
            Probe::Home => Self::Home,
            Probe::BuiltinNames => Self::BuiltinNames,
            Probe::PathBytes(value) => {
                Self::PathBytes(super::json::path_text(value).map_err(|e| e.payload("PathBytes"))?)
            }
            Probe::Workers => Self::Workers,
            Probe::SessionEnded => Self::SessionEnded,
            Probe::CompletionNames => Self::CompletionNames,
            Probe::Bindings => Self::Bindings,
            Probe::PathEntries(value) => Self::PathEntries(
                super::json::path_text(value).map_err(|e| e.payload("PathEntries"))?,
            ),
            Probe::Grants => Self::Grants,
            #[cfg(feature = "test-util")]
            Probe::WorkerCount => Self::WorkerCount,
            #[cfg(feature = "test-util")]
            Probe::GrantDepth => Self::GrantDepth,
        })
    }
}

impl Probe {
    /// Portable observation identity, available independently of producer gates.
    pub fn subject(&self) -> crate::fact::ProbeSubject {
        use crate::fact::ProbeSubject;
        match self {
            Self::BindingCount => ProbeSubject::BindingCount,
            Self::LeasedBindingCount => ProbeSubject::LeasedBindingCount,
            Self::LargestBindingBytes => ProbeSubject::LargestBindingBytes,
            Self::EnvVar(value) => ProbeSubject::EnvVar(value.clone()),
            Self::Cwd => ProbeSubject::Cwd,
            Self::Home => ProbeSubject::Home,
            Self::BuiltinNames => ProbeSubject::BuiltinNames,
            Self::PathBytes(value) => {
                ProbeSubject::PathBytes(crate::text::quote_native(value.as_os_str()))
            }
            Self::Workers => ProbeSubject::Workers,
            Self::SessionEnded => ProbeSubject::SessionEnded,
            Self::CompletionNames => ProbeSubject::CompletionNames,
            Self::Bindings => ProbeSubject::Bindings,
            Self::PathEntries(value) => {
                ProbeSubject::PathEntries(crate::text::quote_native(value.as_os_str()))
            }
            Self::Grants => ProbeSubject::Grants,
            #[cfg(feature = "test-util")]
            Self::WorkerCount => ProbeSubject::WorkerCount,
            #[cfg(feature = "test-util")]
            Self::GrantDepth => ProbeSubject::GrantDepth,
        }
    }
}

impl crate::data::json::JsonDecode for Probe {
    fn decode_json(value: crate::data::json::JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        Ok(crate::data::json::decode_structural::<RawProbe>(value)?.validate())
    }
}
impl crate::data::json::JsonEncode for Probe {
    fn encode_json(
        &self,
        format: crate::data::json::JsonFormat,
    ) -> Result<Vec<u8>, crate::fact::JsonEncodeFailure> {
        Ok(crate::data::json::serialize_structural(
            &RawProbe::from_value(self)?,
            format,
        ))
    }
}

/// Where a listed worker is in its lifecycle: a consumed worker has left the
/// registry, so no row names one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerState {
    Running,
    Stopping,
    Ready(Done),
}

impl WorkerState {
    /// Whether the result is still to be claimed.
    pub fn unclaimed(&self) -> bool {
        !matches!(self, Self::Stopping)
    }
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
        memoized_ty!(closed_record(&[
            ("id", u64::ty()),
            ("title", String::ty()),
            ("owner", Owner::ty()),
            ("state", WorkerState::ty()),
            ("started", i64::ty()),
            ("elapsed", Finite::ty()),
            ("settled", Option::<i64>::ty()),
        ]))
    }
}

impl WorkerRow {
    /// The row as ral reads it: every field but the host's `retention_left`.
    pub fn ral_value(&self) -> ValClos {
        let FOValue::Map { mut entries } = self.clone().encode() else {
            unreachable!("a record encodes as a map");
        };
        entries.retain(|(k, _)| k != "retention-left");
        ValClos::from(FOValue::Map { entries })
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
    pub state: HandleStatus,
    pub title: String,
}

record!(HandleRow {
    state: "state",
    title: "title",
});

/// A handle's worker sample, or withheld in a marshalled parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleStatus {
    Running,
    Stopping,
    Ready,
    Consumed,
    Withheld,
}

variant!(HandleStatus {
    Running: "running",
    Stopping: "stopping",
    Ready: "ready",
    Consumed: "consumed",
    Withheld: "withheld",
});

impl From<Sample<()>> for HandleStatus {
    fn from(sample: Sample<()>) -> Self {
        match sample {
            Sample::Running(()) => Self::Running,
            Sample::Stopping => Self::Stopping,
            Sample::Ready(()) => Self::Ready,
            Sample::Consumed => Self::Consumed,
        }
    }
}

/// Opaque to ral: the stack crosses in the codec `Root.grants` and a
/// marshalled parent use.
impl Datum for GrantStack {
    fn encode(self) -> FOValue {
        Bytes::from(crate::data::json::to_vec(&self).expect("grant paths use lossless OS strings"))
            .encode()
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        crate::data::json::from_slice(&Bytes::decode(v)?).map_err(|why| match why {
            JsonInputFailure::Syntax(cause) => DecodeFailure::new(DecodeCause::JsonSyntax(cause)),
            JsonInputFailure::Decode(cause) => cause,
        })
    }
}

/// What `Probe::Grants` answers: each layer a reader needs
/// ([`GrantLayer::is_shown`]), oldest first, as its value, and whether the
/// answering host holds a `` gpu: `withheld `` (Windows cannot).
#[derive(Debug, Clone, PartialEq)]
pub struct Grants {
    pub layers: Vec<LayerValue>,
    pub withholds_gpu: bool,
}

record!(Grants {
    layers: "layers",
    withholds_gpu: "withholds-gpu",
});

impl Grants {
    /// `stack` as this host answers for it.
    pub fn of(stack: &GrantStack) -> Self {
        Self {
            layers: (stack.layers())
                .filter(|layer| layer.is_shown())
                .map(GrantLayer::value)
                .collect(),
            withholds_gpu: crate::authority::sandbox::WITHHOLDS_GPU,
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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::environ::EnvVars;

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
            let json = crate::data::json::to_vec(&frame).expect("a frame encodes");
            assert_eq!(
                crate::data::json::from_slice::<Frame>(&json).expect("it decodes"),
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
        assert_eq!(
            why,
            DecodeFailure::new(DecodeCause::MissingField("title".into()))
        );
    }

    /// A stack of real directories, exclusions and all: restrictive on exec,
    /// fs and net.
    fn stack_over(root: &std::path::Path) -> GrantStack {
        use crate::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy, Net};
        use crate::path::FrozenPath;
        let spell = |dir: &str| {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            FrozenPath::from_surface(crate::path::utf8(&root.join(dir)).unwrap())
        };
        let mut stack = GrantStack::root();
        stack.push(crate::test_helper::layer(Capabilities {
            exec: Some(ExecGrant::of([ExecKey::Dir(spell("bin"))], [])),
            fs: Some(FsPolicy {
                read_prefixes: vec![spell("work")],
                write_prefixes: vec![spell("scratch")],
                deny_paths: vec![spell("work/secret")],
            }),
            net: Some(Net::Offline),
            ..Capabilities::default()
        }));
        stack
    }

    /// A codec is pure: a stack deserialises with no walk, and what the
    /// receiver owes it is a call of its own.
    #[test]
    fn a_stack_decodes_without_walking() {
        let tmp = tempfile::tempdir().unwrap();
        let stack = stack_over(tmp.path());
        let encoded = stack.clone().encode();
        let before = crate::path::steps_taken();
        let decoded = GrantStack::decode(&encoded);
        assert_eq!(crate::path::steps_taken(), before, "a walk was taken");
        assert_eq!(decoded, Ok(stack));
    }

    /// What `Probe::Grants` answers crosses as grant values alone.
    #[test]
    fn the_grants_answer_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let grants = Grants::of(&stack_over(tmp.path()));
        assert_eq!(grants.layers.len(), 1, "the ambient root states nothing");
        assert_eq!(Grants::decode(&grants.clone().encode()), Ok(grants));
    }

    /// A layer that restricts nothing is answered only where its exec runs
    /// something.
    #[test]
    fn the_answer_holds_the_layers_a_reader_needs() {
        use crate::authority::grant::freeze::FreezeCtx;
        use crate::authority::policy::{Capabilities, EditorVerb, ExecGrant, ExecKey};
        use crate::path::FrozenPath;
        use strum::VariantArray;
        let tmp = tempfile::tempdir().unwrap();
        let every_verb = crate::test_helper::layer(Capabilities {
            editor: Some(EditorVerb::VARIANTS.iter().copied().collect()),
            ..Capabilities::default()
        });
        let dir = FrozenPath::from_surface(crate::path::utf8(tmp.path()).unwrap());
        let runs = Capabilities {
            exec: Some(ExecGrant::of([ExecKey::Dir(dir)], [])),
            ..Capabilities::default()
        };
        let ctx = FreezeCtx::at(EnvVars::new(), crate::test_helper::process_cwd());
        let fragment = crate::shell::grant::decode_fragment(runs.value().as_ref(), "test", &ctx);
        let runs = GrantLayer::root().extend(fragment.unwrap());
        assert!(!every_verb.is_restrictive() && !runs.is_restrictive());
        let stack = crate::test_helper::stack([every_verb, runs.clone()]);
        assert_eq!(Grants::of(&stack).layers, [runs.value()]);
    }

    /// Every row a new reading answers in survives its own round trip.
    #[test]
    fn rows_round_trip() {
        for state in [
            HandleStatus::from(Sample::Running(())),
            HandleStatus::from(Sample::Stopping),
            HandleStatus::from(Sample::Ready(())),
            HandleStatus::from(Sample::Consumed),
            HandleStatus::Withheld,
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
            WorkerState::Ready(Done::Ok(())),
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
