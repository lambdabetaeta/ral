//! Wire representations retain Datum leaves until semantic validation.

use super::{
    Attach, Control, DispatchId, Ending, EngineSpec, EnquiryId, Event, Frame, Origin, Program, Ran,
    Report, Root, Run, SessionEvent, WireError,
};
use crate::authority::policy::json::RawGrantStack;
use crate::data::FOValue;
use crate::data::json::{
    JsonDecode, JsonEncode, JsonFormat, JsonRef, RawDatum, RawFOValue, decode_structural,
    serialize_structural,
};
use crate::environ::EnvVars;
use crate::fact::{
    DecodeCause, DecodeFailure, DecodeIntegerRange, DecodeIntegerTarget, DurationOverflow,
    ErrorFact, JsonEncodeCause, JsonEncodeCauseNonUnicodePath, JsonEncodeFailure, ProbeFailure,
    Reason,
};
use crate::io::{Captured, RunIo, RunStdin};
use crate::path::RawEnginePath;
use crate::process::{CancelCause, RequestedTerminalAccess};
use crate::protocol::probe::RawProbe;
use crate::shell::runtime::CapturePolicy;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU8;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum RawFrame {
    Attach(RawAttach),
    Detach,
    Dispatch(DispatchId, Box<RawRun>),
    Probe(DispatchId, RawProbe),
    Event(DispatchId, RawEvent),
    Session(RawSessionEvent),
    Control(Control),
    Answer(EnquiryId, Result<RawFOValue, RawWireError>),
    Ping(u64),
    Pong(u64),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawAttach {
    engine: RawEngineSpec,
    origin: RawOrigin,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawEngineSpec {
    proto_version: u32,
    installer: String,
    terminal: crate::io::terminal::TerminalState,
    home: RawEnginePath,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum RawOrigin {
    Root(RawRoot),
    Child(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawRoot {
    cwd: RawEnginePath,
    grants: RawGrantStack,
    env: imbl::HashMap<String, String>,
    config: RawFOValue,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawRun {
    program: RawProgram,
    origin: crate::source::SourceOrigin,
    wall: Option<RawDuration>,
    cleanup_grace: RawDuration,
    worker_cap: Option<usize>,
    worker_title: crate::shell::runtime::TitlePolicy,
    io: RunIo,
    terminal: RequestedTerminalAccess,
    stdin: RunStdin,
    trail: Option<CapturePolicy>,
    last: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum RawProgram {
    Source(String),
    Check(String),
    Hook {
        name: crate::shell::runtime::HookName,
        args: Vec<RawFOValue>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum RawEvent {
    Surface(RawFOValue),
    Enquiry(EnquiryId, RawFOValue),
    Reading(Result<RawFOValue, RawDatum<ProbeFailure>>),
    Report(RawReport),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum RawSessionEvent {
    Attached,
    Refused(RawWireError),
    Ended,
    DeferredSurface(Vec<RawFOValue>),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawWireError {
    reason: RawDatum<Reason>,
    context: Option<String>,
    hints: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum RawReport {
    Static {
        rendered: String,
        status: u8,
        trail: Vec<RawFOValue>,
    },
    Checked {
        trail: Vec<RawFOValue>,
    },
    Ran(RawRan),
    Faulted {
        rendered: String,
        status: u8,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawRan {
    ending: RawEnding,
    rendered: String,
    single_command: bool,
    captured: Option<Captured>,
    trail: Vec<RawFOValue>,
}

type RawEnding = crate::fact::Ending<RawFOValue, RawDatum<ErrorFact>, CancelCause>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDuration {
    secs: u64,
    nanos: u32,
}

impl RawDuration {
    pub(crate) fn validate(self) -> Result<Duration, DecodeFailure> {
        self.validate_ref()
    }

    pub(crate) fn validate_ref(&self) -> Result<Duration, DecodeFailure> {
        Duration::from_secs(self.secs)
            .checked_add(Duration::from_nanos(u64::from(self.nanos)))
            .ok_or_else(|| {
                DecodeFailure::new(DecodeCause::DurationOverflow(DurationOverflow {
                    seconds: self.secs.into(),
                    nanos: self.nanos,
                }))
            })
    }
}
impl From<Duration> for RawDuration {
    fn from(value: Duration) -> Self {
        Self {
            secs: value.as_secs(),
            nanos: value.subsec_nanos(),
        }
    }
}

pub(crate) fn path_text(path: &Path) -> Result<String, JsonEncodeFailure> {
    crate::path::utf8(path).map(str::to_owned).map_err(|_| {
        JsonEncodeFailure::new(JsonEncodeCause::NonUnicodePath(
            JsonEncodeCauseNonUnicodePath {
                shown: crate::text::quote_native(path.as_os_str()),
            },
        ))
    })
}

fn status(value: u8) -> Result<NonZeroU8, DecodeFailure> {
    NonZeroU8::new(value).ok_or_else(|| {
        DecodeFailure::new(DecodeCause::IntegerRange(DecodeIntegerRange {
            value: i64::from(value),
            target: DecodeIntegerTarget::FailureStatus,
        }))
    })
}

fn values(raw: &[RawFOValue]) -> Result<Vec<FOValue>, DecodeFailure> {
    let mut values = Vec::with_capacity(raw.len());
    for (i, raw) in raw.iter().enumerate() {
        match raw.validate() {
            Ok(value) => values.push(value),
            Err(error) => {
                for value in values {
                    crate::data::json::drop_value(value);
                }
                return Err(error.index(i.into()));
            }
        }
    }
    Ok(values)
}

fn raw_values(values: &[FOValue]) -> Vec<RawFOValue> {
    values
        .iter()
        .map(|v| RawFOValue::from_value(v.clone()))
        .collect()
}

impl RawWireError {
    pub(crate) fn validate_ref(&self) -> Result<WireError, DecodeFailure> {
        Ok(WireError {
            reason: self.reason.validate().map_err(|e| e.field("reason"))?,
            context: self.context.clone(),
            hints: self.hints.clone(),
        })
    }
    pub(crate) fn validate(self) -> Result<WireError, DecodeFailure> {
        Ok(WireError {
            reason: self.reason.validate().map_err(|e| e.field("reason"))?,
            context: self.context,
            hints: self.hints,
        })
    }
    pub(crate) fn from_value(value: &WireError) -> Self {
        Self {
            reason: RawDatum::from_datum(value.reason.clone()),
            context: value.context.clone(),
            hints: value.hints.clone(),
        }
    }
}
impl RawEngineSpec {
    fn validate(self) -> EngineSpec {
        EngineSpec {
            proto_version: self.proto_version,
            installer: self.installer,
            terminal: self.terminal,
            home: self.home.validate(),
        }
    }
    fn from_value(value: &EngineSpec) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            proto_version: value.proto_version,
            installer: value.installer.clone(),
            terminal: value.terminal,
            home: RawEnginePath::from_path(&value.home).map_err(|e| e.field("home"))?,
        })
    }
}
impl RawRoot {
    fn validate(self) -> Result<Root, DecodeFailure> {
        Ok(Root {
            cwd: self.cwd.validate(),
            grants: self.grants.validate().map_err(|e| e.field("grants"))?,
            env: EnvVars::from_json_map(self.env).map_err(|e| e.field("env"))?,
            config: self.config.validate().map_err(|e| e.field("config"))?,
        })
    }
    fn from_value(value: &Root) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            cwd: RawEnginePath::from_path(&value.cwd).map_err(|e| e.field("cwd"))?,
            grants: (&value.grants).into(),
            env: value
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            config: RawFOValue::from_value(value.config.clone()),
        })
    }
}
impl RawOrigin {
    fn validate(self) -> Result<Origin, DecodeFailure> {
        match self {
            Self::Root(root) => root
                .validate()
                .map(Origin::Root)
                .map_err(|e| e.payload("Root")),
            Self::Child(sealed) => crate::seed::Sealed::from_base64(&sealed)
                .map(Origin::Child)
                .map_err(|e| e.payload("Child")),
        }
    }
    fn from_value(value: &Origin) -> Result<Self, JsonEncodeFailure> {
        match value {
            Origin::Root(root) => RawRoot::from_value(root)
                .map(Self::Root)
                .map_err(|e| e.payload("Root")),
            Origin::Child(sealed) => Ok(Self::Child(sealed.to_base64())),
        }
    }
}
impl RawAttach {
    fn validate(self) -> Result<Attach, DecodeFailure> {
        Ok(Attach {
            engine: self.engine.validate(),
            origin: self.origin.validate().map_err(|e| e.field("origin"))?,
        })
    }
    fn from_value(value: &Attach) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            engine: RawEngineSpec::from_value(&value.engine).map_err(|e| e.field("engine"))?,
            origin: RawOrigin::from_value(&value.origin).map_err(|e| e.field("origin"))?,
        })
    }
}
impl RawProgram {
    fn validate(self) -> Result<Program, DecodeFailure> {
        match self {
            Self::Source(source) => Ok(Program::Source(source)),
            Self::Check(source) => Ok(Program::Check(source)),
            Self::Hook { name, args } => Ok(Program::Hook {
                name,
                args: values(&args).map_err(|e| e.field("args").payload("Hook"))?,
            }),
        }
    }
    fn from_value(value: &Program) -> Self {
        match value {
            Program::Source(source) => Self::Source(source.clone()),
            Program::Check(source) => Self::Check(source.clone()),
            Program::Hook { name, args } => Self::Hook {
                name: name.clone(),
                args: raw_values(args),
            },
        }
    }
}
impl RawRun {
    fn validate(self) -> Result<Run, DecodeFailure> {
        let program = self.program.validate().map_err(|e| e.field("program"))?;
        let timing = (|| {
            let wall = self
                .wall
                .map(RawDuration::validate)
                .transpose()
                .map_err(|e| e.field("wall"))?;
            let cleanup_grace = self
                .cleanup_grace
                .validate()
                .map_err(|e| e.field("cleanup_grace"))?;
            Ok((wall, cleanup_grace))
        })();
        let (wall, cleanup_grace) = match timing {
            Ok(timing) => timing,
            Err(error) => {
                if let Program::Hook { args, .. } = program {
                    for value in args {
                        crate::data::json::drop_value(value);
                    }
                }
                return Err(error);
            }
        };
        Ok(Run {
            program,
            origin: self.origin,
            wall,
            cleanup_grace,
            worker_cap: self.worker_cap,
            worker_title: self.worker_title,
            io: self.io,
            terminal: self.terminal,
            stdin: self.stdin,
            trail: self.trail,
            last: self.last,
        })
    }
    fn from_value(value: &Run) -> Self {
        Self {
            program: RawProgram::from_value(&value.program),
            origin: value.origin.clone(),
            wall: value.wall.map(Into::into),
            cleanup_grace: value.cleanup_grace.into(),
            worker_cap: value.worker_cap,
            worker_title: value.worker_title,
            io: value.io,
            terminal: value.terminal,
            stdin: value.stdin,
            trail: value.trail,
            last: value.last,
        }
    }
}

impl RawEvent {
    fn validate(self) -> Result<Event, DecodeFailure> {
        match self {
            Self::Surface(value) => value
                .validate()
                .map(Event::Surface)
                .map_err(|e| e.payload("Surface")),
            Self::Enquiry(id, value) => value
                .validate()
                .map(|v| Event::Enquiry(id, v))
                .map_err(|e| e.index(1usize.into()).payload("Enquiry")),
            Self::Reading(reading) => match reading {
                Ok(value) => value
                    .validate()
                    .map(|v| Event::Reading(Ok(v)))
                    .map_err(|e| e.payload("Ok").payload("Reading")),
                Err(error) => error
                    .validate()
                    .map(|e| Event::Reading(Err(e)))
                    .map_err(|e| e.payload("Err").payload("Reading")),
            },
            Self::Report(report) => report
                .validate()
                .map(Event::Report)
                .map_err(|e| e.payload("Report")),
        }
    }
    fn from_value(value: &Event) -> Self {
        match value {
            Event::Surface(value) => Self::Surface(RawFOValue::from_value(value.clone())),
            Event::Enquiry(id, value) => Self::Enquiry(*id, RawFOValue::from_value(value.clone())),
            Event::Reading(reading) => Self::Reading(match reading {
                Ok(value) => Ok(RawFOValue::from_value(value.clone())),
                Err(error) => Err(RawDatum::from_datum(error.clone())),
            }),
            Event::Report(report) => Self::Report(RawReport::from_value(report)),
        }
    }
}
impl RawSessionEvent {
    fn validate(self) -> Result<SessionEvent, DecodeFailure> {
        match self {
            Self::Attached => Ok(SessionEvent::Attached),
            Self::Refused(error) => error
                .validate()
                .map(SessionEvent::Refused)
                .map_err(|e| e.payload("Refused")),
            Self::Ended => Ok(SessionEvent::Ended),
            Self::DeferredSurface(raw) => values(&raw)
                .map(SessionEvent::DeferredSurface)
                .map_err(|e| e.payload("DeferredSurface")),
        }
    }
    fn from_value(value: &SessionEvent) -> Self {
        match value {
            SessionEvent::Attached => Self::Attached,
            SessionEvent::Refused(error) => Self::Refused(RawWireError::from_value(error)),
            SessionEvent::Ended => Self::Ended,
            SessionEvent::DeferredSurface(values) => Self::DeferredSurface(raw_values(values)),
        }
    }
}

fn ending(raw: RawEnding) -> Result<Ending, DecodeFailure> {
    use crate::fact::Ending as E;
    match raw {
        E::Ok(value) => value.validate().map(E::Ok).map_err(|e| e.payload("ok")),
        E::Err(error) => error.validate().map(E::Err).map_err(|e| e.payload("err")),
        E::Exited(code) => Ok(E::Exited(code)),
        E::Stopped(cause) => Ok(E::Stopped(cause)),
        E::Panic(text) => Ok(E::Panic(text)),
    }
}
fn raw_ending(value: &Ending) -> RawEnding {
    use crate::fact::Ending as E;
    match value {
        E::Ok(value) => E::Ok(RawFOValue::from_value(value.clone())),
        E::Err(record) => E::Err(RawDatum::from_datum(record.clone())),
        E::Exited(code) => E::Exited(*code),
        E::Stopped(cause) => E::Stopped(*cause),
        E::Panic(text) => E::Panic(text.clone()),
    }
}
impl RawRan {
    fn validate(self) -> Result<Ran, DecodeFailure> {
        let ending = ending(self.ending).map_err(|e| e.field("ending"))?;
        let trail = match values(&self.trail) {
            Ok(trail) => trail,
            Err(error) => {
                if let crate::fact::Ending::Ok(value) = ending {
                    crate::data::json::drop_value(value);
                }
                return Err(error.field("trail"));
            }
        };
        Ok(Ran {
            ending,
            rendered: self.rendered,
            single_command: self.single_command,
            captured: self.captured,
            trail,
        })
    }
    fn from_value(value: &Ran) -> Self {
        Self {
            ending: raw_ending(&value.ending),
            rendered: value.rendered.clone(),
            single_command: value.single_command,
            captured: value.captured.clone(),
            trail: raw_values(&value.trail),
        }
    }
}
impl RawReport {
    fn validate(self) -> Result<Report, DecodeFailure> {
        match self {
            Self::Static {
                rendered,
                status: value,
                trail,
            } => Ok(Report::Static {
                rendered,
                status: status(value).map_err(|e| e.field("status").payload("Static"))?,
                trail: values(&trail).map_err(|e| e.field("trail").payload("Static"))?,
            }),
            Self::Checked { trail } => Ok(Report::Checked {
                trail: values(&trail).map_err(|e| e.field("trail").payload("Checked"))?,
            }),
            Self::Ran(ran) => ran
                .validate()
                .map(Report::Ran)
                .map_err(|e| e.payload("Ran")),
            Self::Faulted {
                rendered,
                status: value,
            } => Ok(Report::Faulted {
                rendered,
                status: status(value).map_err(|e| e.field("status").payload("Faulted"))?,
            }),
        }
    }
    fn from_value(value: &Report) -> Self {
        match value {
            Report::Static {
                rendered,
                status,
                trail,
            } => Self::Static {
                rendered: rendered.clone(),
                status: status.get(),
                trail: raw_values(trail),
            },
            Report::Checked { trail } => Self::Checked {
                trail: raw_values(trail),
            },
            Report::Ran(ran) => Self::Ran(RawRan::from_value(ran)),
            Report::Faulted { rendered, status } => Self::Faulted {
                rendered: rendered.clone(),
                status: status.get(),
            },
        }
    }
}
impl RawFrame {
    fn validate(self) -> Result<Frame, DecodeFailure> {
        match self {
            Self::Attach(attach) => attach
                .validate()
                .map(Frame::Attach)
                .map_err(|e| e.payload("Attach")),
            Self::Detach => Ok(Frame::Detach),
            Self::Dispatch(id, run) => run
                .validate()
                .map(|run| Frame::Dispatch(id, Box::new(run)))
                .map_err(|e| e.index(1usize.into()).payload("Dispatch")),
            Self::Probe(id, probe) => Ok(Frame::Probe(id, probe.validate())),
            Self::Event(id, event) => event
                .validate()
                .map(|event| Frame::Event(id, event))
                .map_err(|e| e.index(1usize.into()).payload("Event")),
            Self::Session(event) => event
                .validate()
                .map(Frame::Session)
                .map_err(|e| e.payload("Session")),
            Self::Control(control) => Ok(Frame::Control(control)),
            Self::Answer(id, answer) => match answer {
                Ok(value) => value
                    .validate()
                    .map(|value| Frame::Answer(id, Ok(value)))
                    .map_err(|e| e.payload("Ok").index(1usize.into()).payload("Answer")),
                Err(error) => error
                    .validate()
                    .map(|error| Frame::Answer(id, Err(error)))
                    .map_err(|e| e.payload("Err").index(1usize.into()).payload("Answer")),
            },
            Self::Ping(id) => Ok(Frame::Ping(id)),
            Self::Pong(id) => Ok(Frame::Pong(id)),
        }
    }
    fn from_value(value: &Frame) -> Result<Self, JsonEncodeFailure> {
        match value {
            Frame::Attach(attach) => RawAttach::from_value(attach)
                .map(Self::Attach)
                .map_err(|e| e.payload("Attach")),
            Frame::Detach => Ok(Self::Detach),
            Frame::Dispatch(id, run) => Ok(Self::Dispatch(*id, Box::new(RawRun::from_value(run)))),
            Frame::Probe(id, probe) => RawProbe::from_value(probe)
                .map(|probe| Self::Probe(*id, probe))
                .map_err(|e| e.index(1usize.into()).payload("Probe")),
            Frame::Event(id, event) => Ok(Self::Event(*id, RawEvent::from_value(event))),
            Frame::Session(event) => Ok(Self::Session(RawSessionEvent::from_value(event))),
            Frame::Control(control) => Ok(Self::Control(*control)),
            Frame::Answer(id, answer) => Ok(Self::Answer(
                *id,
                match answer {
                    Ok(value) => Ok(RawFOValue::from_value(value.clone())),
                    Err(error) => Err(RawWireError::from_value(error)),
                },
            )),
            Frame::Ping(id) => Ok(Self::Ping(*id)),
            Frame::Pong(id) => Ok(Self::Pong(*id)),
        }
    }
}

macro_rules! codec {
    ($semantic:ty, $raw:ty) => {
        impl JsonDecode for $semantic {
            fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
                decode_structural::<$raw>(value)?.validate()
            }
        }
        impl JsonEncode for $semantic {
            fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
                Ok(serialize_structural(&<$raw>::from_value(self)?, format))
            }
        }
    };
}
codec!(Frame, RawFrame);
codec!(Attach, RawAttach);
impl JsonDecode for EngineSpec {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        Ok(decode_structural::<RawEngineSpec>(value)?.validate())
    }
}
impl JsonEncode for EngineSpec {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(
            &RawEngineSpec::from_value(self)?,
            format,
        ))
    }
}
codec!(Origin, RawOrigin);
codec!(Root, RawRoot);
impl JsonDecode for Run {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawRun>(value)?.validate()
    }
}
impl JsonEncode for Run {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&RawRun::from_value(self), format))
    }
}
impl JsonDecode for Program {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawProgram>(value)?.validate()
    }
}
impl JsonEncode for Program {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&RawProgram::from_value(self), format))
    }
}
impl JsonDecode for Event {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawEvent>(value)?.validate()
    }
}
impl JsonEncode for Event {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&RawEvent::from_value(self), format))
    }
}
impl JsonDecode for SessionEvent {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawSessionEvent>(value)?.validate()
    }
}
impl JsonEncode for SessionEvent {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(
            &RawSessionEvent::from_value(self),
            format,
        ))
    }
}
impl JsonDecode for Report {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawReport>(value)?.validate()
    }
}
impl JsonEncode for Report {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&RawReport::from_value(self), format))
    }
}
impl JsonDecode for Ran {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawRan>(value)?.validate()
    }
}
impl JsonEncode for Ran {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&RawRan::from_value(self), format))
    }
}

impl JsonDecode for WireError {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawWireError>(value)?.validate()
    }
}
impl JsonEncode for WireError {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(
            &RawWireError::from_value(self),
            format,
        ))
    }
}
impl JsonDecode for Ending {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        ending(decode_structural(value)?)
    }
}
impl JsonEncode for Ending {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&raw_ending(self), format))
    }
}

impl JsonDecode for Control {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural(value)
    }
}
impl JsonEncode for Control {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(self, format))
    }
}

#[cfg(test)]
mod tests;
