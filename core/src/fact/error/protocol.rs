//! Portable protocol failure facts.

use super::decode::{
    DecodeCause, DecodeFailure, JsonEncodeCause, JsonEncodeFailure, JsonInputFailure,
};
use super::io::IoCause;
use super::shared::{
    Count, DataStep, DurationFact, Expected, ShapeFailure, UnknownTag, ValueShape,
};
use crate::{label, record, variant};
use strum::{IntoStaticStr, VariantArray};

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum FramePart {
    Length,
    Body,
}
label!(FramePart);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameEncodeFailure {
    TooLarge(FrameEncodeFailureTooLarge),
    Json(JsonEncodeFailure),
}
variant!(FrameEncodeFailure { TooLarge(FrameEncodeFailureTooLarge): "too-large", Json(JsonEncodeFailure): "json" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameFailure {
    TooLarge(FrameFailureTooLarge),
    Partial(FrameFailurePartial),
    Json(JsonInputFailure),
    Io(IoCause),
}
variant!(FrameFailure { TooLarge(FrameFailureTooLarge): "too-large", Partial(FrameFailurePartial): "partial", Json(JsonInputFailure): "json", Io(IoCause): "io" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ProtocolOperation {
    Attach,
    Detach,
    Dispatch,
    Probe,
    Surface,
    Enquiry,
    Answer,
    Reading,
    Report,
    Session,
    Control,
    Ping,
    Pong,
}
label!(ProtocolOperation);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ProtocolData {
    Request,
    Answer,
    RootConfig,
    Parent,
}
label!(ProtocolData);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ProtocolWork {
    Dispatch,
    Probe,
}
label!(ProtocolWork);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolFailure {
    VersionMismatch(ProtocolFailureVersionMismatch),
    UnknownInstaller(String),
    RootMissingScratch,
    Decode(ProtocolFailureDecode),
    Unencodable(ProtocolFailureUnencodable),
    /// A shell's image cannot be written as the document its child reads.
    Marshal(JsonEncodeFailure),
    ConnectionLostDuringEnquiry,
    Busy(ProtocolWork),
}
variant!(ProtocolFailure { VersionMismatch(ProtocolFailureVersionMismatch): "version-mismatch", UnknownInstaller(String): "unknown-installer", RootMissingScratch: "root-missing-scratch", Decode(ProtocolFailureDecode): "decode", Unencodable(ProtocolFailureUnencodable): "unencodable", Marshal(JsonEncodeFailure): "marshal", ConnectionLostDuringEnquiry: "connection-lost-during-enquiry", Busy(ProtocolWork): "busy" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ProtocolPeer {
    Frontend,
    Engine,
}
label!(ProtocolPeer);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ProtocolPhase {
    AwaitAttach,
    Attached,
}
label!(ProtocolPhase);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum FrameKind {
    Attach,
    Detach,
    Dispatch,
    Probe,
    Event,
    Session,
    Control,
    Answer,
    Ping,
    Pong,
}
label!(FrameKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ConnectionOperation {
    Adopt,
    Read,
    Write,
    SetWriteDeadline,
}
label!(ConnectionOperation);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeSubject {
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
    WorkerCount,
    GrantDepth,
}
variant!(ProbeSubject { BindingCount: "binding-count", LeasedBindingCount: "leased-binding-count", LargestBindingBytes: "largest-binding-bytes", EnvVar(String): "env-var", Cwd: "cwd", Home: "home", BuiltinNames: "builtin-names", PathBytes(String): "path-bytes", Workers: "workers", SessionEnded: "session-ended", CompletionNames: "completion-names", Bindings: "bindings", PathEntries(String): "path-entries", Grants: "grants", WorkerCount: "worker-count", GrantDepth: "grant-depth" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeFailure {
    Busy,
    WorkerPanicked,
    Unencodable(FrameEncodeFailure),
}
variant!(ProbeFailure { Busy: "busy", WorkerPanicked: "worker-panicked", Unencodable(FrameEncodeFailure): "unencodable" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionClosed {
    Eof(ProtocolPeer),
    ReaderThreadEnded,
    TransportDropped,
    Io(ConnectionClosedIo),
    Frame(FrameFailure),
}
variant!(ConnectionClosed { Eof(ProtocolPeer): "eof", ReaderThreadEnded: "reader-thread-ended", TransportDropped: "transport-dropped", Io(ConnectionClosedIo): "io", Frame(FrameFailure): "frame" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionFault {
    UnexpectedFrame(ConnectionFaultUnexpectedFrame),
    ReadingForDispatch,
    ProbeAnswer(ConnectionFaultProbeAnswer),
    ProbeRejected(ConnectionFaultProbeRejected),
    Unencodable(ConnectionFaultUnencodable),
    WorkersSurvived(Count),
}
variant!(ConnectionFault { UnexpectedFrame(ConnectionFaultUnexpectedFrame): "unexpected-frame", ReadingForDispatch: "reading-for-dispatch", ProbeAnswer(ConnectionFaultProbeAnswer): "probe-answer", ProbeRejected(ConnectionFaultProbeRejected): "probe-rejected", Unencodable(ConnectionFaultUnencodable): "unencodable", WorkersSurvived(Count): "workers-survived" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionFailure {
    Closed(ConnectionClosed),
    Silent(DurationFact),
    Faulted(ConnectionFault),
}
variant!(ConnectionFailure { Closed(ConnectionClosed): "closed", Silent(DurationFact): "silent", Faulted(ConnectionFault): "faulted" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameEncodeFailureTooLarge {
    pub actual: Count,
    pub limit: Count,
}
record!(FrameEncodeFailureTooLarge {
    actual: "actual",
    limit: "limit"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameFailureTooLarge {
    pub actual: Count,
    pub limit: Count,
}
record!(FrameFailureTooLarge {
    actual: "actual",
    limit: "limit"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameFailurePartial {
    pub part: FramePart,
    pub got: Count,
    pub expected: Count,
}
record!(FrameFailurePartial {
    part: "part",
    got: "got",
    expected: "expected"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolFailureVersionMismatch {
    pub front: u32,
    pub engine: u32,
}
record!(ProtocolFailureVersionMismatch {
    front: "front",
    engine: "engine"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolFailureDecode {
    pub kind: ProtocolData,
    pub operation: String,
    pub cause: DecodeFailure,
}
record!(ProtocolFailureDecode {
    kind: "kind",
    operation: "operation",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolFailureUnencodable {
    pub operation: ProtocolOperation,
    pub cause: FrameEncodeFailure,
}
record!(ProtocolFailureUnencodable {
    operation: "operation",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionClosedIo {
    pub operation: ConnectionOperation,
    pub cause: IoCause,
}
record!(ConnectionClosedIo {
    operation: "operation",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionFaultUnexpectedFrame {
    pub receiver: ProtocolPeer,
    pub phase: ProtocolPhase,
    pub actual: FrameKind,
}
record!(ConnectionFaultUnexpectedFrame {
    receiver: "receiver",
    phase: "phase",
    actual: "actual"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionFaultProbeAnswer {
    pub probe: ProbeSubject,
    pub cause: DecodeFailure,
}
record!(ConnectionFaultProbeAnswer {
    probe: "probe",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionFaultProbeRejected {
    pub probe: ProbeSubject,
    pub cause: ProbeFailure,
}
record!(ConnectionFaultProbeRejected {
    probe: "probe",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionFaultUnencodable {
    pub operation: ProtocolOperation,
    pub cause: FrameEncodeFailure,
}
record!(ConnectionFaultUnencodable {
    operation: "operation",
    cause: "cause"
});

impl FramePart {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl FrameEncodeFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::TooLarge(value) => value.describe(),
            Self::Json(value) => value.describe(),
        }
    }
}

impl FrameFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::TooLarge(value) => value.describe(),
            Self::Partial(value) => value.describe(),
            Self::Json(value) => format!("invalid frame JSON: {}", value.describe()),
            Self::Io(value) => format!("could not read frame: {}", value.describe()),
        }
    }
}

impl ProtocolOperation {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ProtocolData {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ProtocolWork {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ProtocolFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::VersionMismatch(value) => value.describe(),
            Self::UnknownInstaller(value) => format!("unknown installer {value:?}"),
            Self::RootMissingScratch => "the root attach names no $EXARCH_SCRATCH; the host owns the scratch and names it in every root attach".into(),
            Self::Decode(value) => value.describe(),
            Self::Unencodable(value) => value.describe(),
            Self::Marshal(value) => match &value.cause {
                JsonEncodeCause::NonUnicodePath(path) => format!(
                    "cannot start a child: a child receives its parent shell as text, so every path in it must be valid Unicode, and {} is not; `cd` to a directory whose path is Unicode, then spawn again",
                    path.shown
                ),
            },
            Self::ConnectionLostDuringEnquiry => "connection lost during enquiry".to_owned(),
            Self::Busy(value) => format!("busy: {}", value.describe()),
        }
    }
}

impl ProtocolPeer {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ProtocolPhase {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl FrameKind {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ConnectionOperation {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ProbeSubject {
    pub fn describe(&self) -> String {
        match self {
            Self::BindingCount => "binding count".to_owned(),
            Self::LeasedBindingCount => "leased binding count".to_owned(),
            Self::LargestBindingBytes => "largest binding bytes".to_owned(),
            Self::EnvVar(value) => format!("environment-variable {value:?}"),
            Self::Cwd => "cwd".to_owned(),
            Self::Home => "home".to_owned(),
            Self::BuiltinNames => "builtin names".to_owned(),
            Self::PathBytes(value) => format!("path bytes: {value}"),
            Self::Workers => "workers".to_owned(),
            Self::SessionEnded => "session ended".to_owned(),
            Self::CompletionNames => "completion names".to_owned(),
            Self::Bindings => "bindings".to_owned(),
            Self::PathEntries(value) => format!("path entries: {value}"),
            Self::Grants => "grants".to_owned(),
            Self::WorkerCount => "worker count".to_owned(),
            Self::GrantDepth => "grant depth".to_owned(),
        }
    }
}

impl ProbeFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Busy => "the engine is busy".into(),
            Self::WorkerPanicked => "the probe worker panicked".into(),
            Self::Unencodable(value) => {
                format!("cannot encode the probe answer: {}", value.describe())
            }
        }
    }
}

impl ConnectionClosed {
    pub fn describe(&self) -> String {
        match self {
            Self::Eof(value) => format!("end of stream from the {}", value.describe()),
            Self::ReaderThreadEnded => "the reader thread ended".into(),
            Self::TransportDropped => "the transport was dropped".into(),
            Self::Io(value) => value.describe(),
            Self::Frame(value) => value.describe(),
        }
    }
}

impl ConnectionFault {
    pub fn describe(&self) -> String {
        match self {
            Self::UnexpectedFrame(value) => value.describe(),
            Self::ReadingForDispatch => {
                "received a probe reading while awaiting a dispatch report".into()
            }
            Self::ProbeAnswer(value) => value.describe(),
            Self::ProbeRejected(value) => value.describe(),
            Self::Unencodable(value) => value.describe(),
            Self::WorkersSurvived(value) => format!(
                "{} remained after session shutdown",
                crate::text::plural(value.to_u64(), "worker thread")
            ),
        }
    }
}

impl ConnectionFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Closed(value) => format!("connection closed: {}", value.describe()),
            Self::Silent(value) => format!(
                "no response from the engine for {}.{:09} seconds",
                value.seconds, value.nanos
            ),
            Self::Faulted(value) => value.describe(),
        }
    }
}

impl FrameEncodeFailureTooLarge {
    pub fn describe(&self) -> String {
        format!(
            "encoded frame has {}, exceeding limit {}",
            crate::text::plural(self.actual.to_u64(), "byte"),
            self.limit
        )
    }
}

impl FrameFailureTooLarge {
    pub fn describe(&self) -> String {
        format!(
            "frame has {}, exceeding limit {}",
            crate::text::plural(self.actual.to_u64(), "byte"),
            self.limit
        )
    }
}

impl FrameFailurePartial {
    pub fn describe(&self) -> String {
        format!(
            "partial frame {}: got {}, expected {}",
            self.part.describe(),
            crate::text::plural(self.got.to_u64(), "byte"),
            self.expected
        )
    }
}

impl ProtocolFailureVersionMismatch {
    pub fn describe(&self) -> String {
        format!(
            "protocol version mismatch: frontend {}, engine {}",
            self.front, self.engine
        )
    }
}

impl ProtocolFailureDecode {
    pub fn describe(&self) -> String {
        match self.kind {
            ProtocolData::Answer => format!(
                "{}: the host answered out of shape; {}",
                self.operation,
                self.cause.describe()
            ),
            ProtocolData::Parent => format!(
                "{}: cannot read the image of the parent shell that ral itself wrote for this child: {}. A refusal here means the two engines differ in version, or ral has a bug; it is not caused by your program",
                self.operation,
                self.cause.describe()
            ),
            ProtocolData::Request | ProtocolData::RootConfig => format!(
                "{}: malformed {}: {}",
                self.operation,
                self.kind.describe(),
                self.cause.describe()
            ),
        }
    }
}

impl ProtocolFailureUnencodable {
    pub fn describe(&self) -> String {
        format!(
            "cannot encode {}: {}",
            self.operation.describe(),
            self.cause.describe()
        )
    }
}

impl ConnectionClosedIo {
    pub fn describe(&self) -> String {
        format!(
            "could not {}: {}",
            match self.operation {
                ConnectionOperation::Adopt => "adopt the connection",
                ConnectionOperation::Read => "read from the connection",
                ConnectionOperation::Write => "write to the connection",
                ConnectionOperation::SetWriteDeadline => "set the connection's write deadline",
            },
            self.cause.describe()
        )
    }
}

impl ConnectionFaultUnexpectedFrame {
    pub fn describe(&self) -> String {
        format!(
            "the {} received an unexpected {} frame {}",
            self.receiver.describe(),
            self.actual.describe(),
            match self.phase {
                ProtocolPhase::AwaitAttach => "while awaiting attachment",
                ProtocolPhase::Attached => "after attachment",
            }
        )
    }
}

impl ConnectionFaultProbeAnswer {
    pub fn describe(&self) -> String {
        format!(
            "malformed answer to the {} probe: {}",
            self.probe.describe(),
            self.cause.describe()
        )
    }
}

impl ConnectionFaultProbeRejected {
    pub fn describe(&self) -> String {
        format!(
            "the {} probe was rejected: {}",
            self.probe.describe(),
            self.cause.describe()
        )
    }
}

impl ConnectionFaultUnencodable {
    pub fn describe(&self) -> String {
        format!(
            "cannot encode {}: {}",
            self.operation.describe(),
            self.cause.describe()
        )
    }
}

impl ProtocolFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Decode(value) => value.advice(),
            _ => Vec::new(),
        }
    }
}

impl ProtocolFailureDecode {
    pub fn advice(&self) -> Vec<String> {
        if self.kind != ProtocolData::Request || self.operation != "exarch-agents" {
            return Vec::new();
        }
        let advice = match (self.cause.path.as_slice(), self.cause.cause.as_ref()) {
            (
                [DataStep::Payload(start), DataStep::Field(grant)],
                DecodeCause::UnknownTag(UnknownTag { tag, .. }),
            ) if start == "start"
                && grant == "grant"
                && matches!(tag.as_str(), "dangerous" | "minimal") =>
            {
                "`inherit is how you decline to narrow at all; a child cannot widen its parent's grant"
            }
            (
                [DataStep::Payload(start), DataStep::Field(grant)],
                DecodeCause::MissingPayload(tag),
            ) if start == "start" && grant == "grant" && tag == "restrict" => {
                "`restrict must carry a capability record of the same shape grant [...] takes: [exec: …, fs: …, net: …, gpu: …, editor: …, shell: …], every key optional"
            }
            (
                [
                    DataStep::Payload(start),
                    DataStep::Field(grant),
                    DataStep::Payload(restrict),
                ],
                DecodeCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Map),
                    ..
                }),
            ) if start == "start" && grant == "grant" && restrict == "restrict" => {
                "`restrict must carry a capability record of the same shape grant [...] takes: [exec: …, fs: …, net: …, gpu: …, editor: …, shell: …], every key optional"
            }
            _ => return Vec::new(),
        };
        vec![advice.into()]
    }
}

#[cfg(test)]
mod payload_presentation_tests {
    use super::*;
    use crate::data::datum::Datum;
    use crate::error::Error;
    use crate::fact::JsonEncodeCauseNonUnicodePath;
    use crate::fact::{Found, Reason};
    use std::fmt::Write as _;

    fn check(value: &ProtocolFailure, description: &str, advice: &[&str]) {
        let back = ProtocolFailure::decode(&value.clone().encode()).unwrap();
        assert_eq!(&back, value);
        for value in [value, &back] {
            assert_eq!(value.describe(), description);
            let reason = Reason::Protocol(Box::new(value.clone()));
            assert_eq!(reason.advice(), advice);
            let error = Error::new(reason);
            assert_eq!(error.hints(), []);
            let mut expected = description.to_owned();
            for hint in advice {
                let _ = write!(expected, "\nhint: {hint}");
            }
            assert_eq!(error.plain(), expected);
        }
    }
    fn decode(
        kind: ProtocolData,
        operation: &str,
        path: &[DataStep],
        cause: &DecodeCause,
    ) -> ProtocolFailureDecode {
        ProtocolFailureDecode {
            kind,
            operation: operation.into(),
            cause: DecodeFailure {
                path: path.to_vec(),
                cause: Box::new(cause.clone()),
            },
        }
    }

    #[test]
    fn parent_projects_as_a_bare_label() {
        let parent = crate::data::datum::tag("parent", None);
        assert_eq!(ProtocolData::Parent.encode(), parent);
        assert_eq!(ProtocolData::decode(&parent), Ok(ProtocolData::Parent));
    }

    #[test]
    fn parent_decode_is_presented_as_malformed_parent_at_its_path() {
        let value = ProtocolFailure::Decode(decode(
            ProtocolData::Parent,
            "child",
            &[
                DataStep::Field("rows".into()),
                DataStep::Index(Count::from_u64(0)),
            ],
            &DecodeCause::OpenClosure("x".into()),
        ));
        check(
            &value,
            "child: cannot read the image of the parent shell that ral itself wrote for this child: field \"rows\": element 0: a closure names `x`, which its row does not bind. A refusal here means the two engines differ in version, or ral has a bug; it is not caused by your program",
            &[],
        );
    }

    #[test]
    fn marshal_refusal_names_the_path_and_the_remedy() {
        let value = ProtocolFailure::Marshal(
            JsonEncodeFailure::new(JsonEncodeCause::NonUnicodePath(
                JsonEncodeCauseNonUnicodePath {
                    shown: "\"/invalid-\\xff\"".into(),
                },
            ))
            .field("cwd")
            .field("context"),
        );
        check(
            &value,
            "cannot start a child: a child receives its parent shell as text, so every path in it must be valid Unicode, and \"/invalid-\\xff\" is not; `cd` to a directory whose path is Unicode, then spawn again",
            &[],
        );
    }

    #[test]
    fn spawn_grant_advice_requires_request_operation_path_and_exact_cause() {
        let path = vec![
            DataStep::Payload("start".into()),
            DataStep::Field("grant".into()),
        ];
        let restrict_path = vec![
            DataStep::Payload("start".into()),
            DataStep::Field("grant".into()),
            DataStep::Payload("restrict".into()),
        ];
        let inherit =
            "`inherit is how you decline to narrow at all; a child cannot widen its parent's grant";
        let restrict = "`restrict must carry a capability record of the same shape grant [...] takes: [exec: …, fs: …, net: …, gpu: …, editor: …, shell: …], every key optional";
        let cases = [
            (
                path.clone(),
                DecodeCause::UnknownTag(UnknownTag {
                    tag: "dangerous".into(),
                    offered: vec!["inherit".into(), "restrict".into()],
                }),
                inherit,
            ),
            (
                path.clone(),
                DecodeCause::UnknownTag(UnknownTag {
                    tag: "minimal".into(),
                    offered: vec![],
                }),
                inherit,
            ),
            (
                path.clone(),
                DecodeCause::MissingPayload("restrict".into()),
                restrict,
            ),
            (
                restrict_path.clone(),
                DecodeCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Map),
                    found: Found::Unit,
                }),
                restrict,
            ),
        ];
        for (positive_path, cause, hint) in &cases {
            let value = decode(ProtocolData::Request, "exarch-agents", positive_path, cause);
            check(
                &ProtocolFailure::Decode(value.clone()),
                &value.describe(),
                &[*hint],
            );
            for kind in [ProtocolData::Answer, ProtocolData::RootConfig] {
                let value = decode(kind, "exarch-agents", positive_path, cause);
                check(
                    &ProtocolFailure::Decode(value.clone()),
                    &value.describe(),
                    &[],
                );
            }
            for operation in ["exarch-agents-other", "exarch-schedules", "other"] {
                let value = decode(ProtocolData::Request, operation, positive_path, cause);
                check(
                    &ProtocolFailure::Decode(value.clone()),
                    &value.describe(),
                    &[],
                );
            }
            for negative_path in [
                vec![],
                vec![
                    DataStep::Payload("start".into()),
                    DataStep::Field("model".into()),
                ],
                vec![
                    DataStep::Payload("other".into()),
                    DataStep::Field("grant".into()),
                ],
                {
                    let mut p = positive_path.clone();
                    p.push(DataStep::Field("exec".into()));
                    p
                },
            ] {
                let value = decode(
                    ProtocolData::Request,
                    "exarch-agents",
                    &negative_path,
                    cause,
                );
                check(
                    &ProtocolFailure::Decode(value.clone()),
                    &value.describe(),
                    &[],
                );
            }
        }
        for (path, cause) in [
            (
                path.clone(),
                DecodeCause::UnknownTag(UnknownTag {
                    tag: "other".into(),
                    offered: vec!["inherit".into()],
                }),
            ),
            (path.clone(), DecodeCause::MissingPayload("other".into())),
            (
                path,
                DecodeCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Map),
                    found: Found::Unit,
                }),
            ),
            (
                restrict_path,
                DecodeCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::List),
                    found: Found::Unit,
                }),
            ),
        ] {
            let value = decode(ProtocolData::Request, "exarch-agents", &path, &cause);
            check(
                &ProtocolFailure::Decode(value.clone()),
                &value.describe(),
                &[],
            );
        }
    }
}
