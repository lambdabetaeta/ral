//! Typed record representations, separate from the log's byte envelope.

use super::{
    AgentId, BlockId, Card, Change, ChatMessage, ChildFailure, ChildStatus, Control, Display,
    EditAuthority, Eviction, FOValue, Forensic, HelperFailure, ItemKind, JobId, Linked, Location,
    Locus, NonEmpty, PathBuf, Pointer, Post, PostId, ProviderError, Range, Record, Role, Schedule,
    Selected, Seq, Settlement, Severed, Span, SpanRow, Spent, Started, Sum, ToolResult, Transcript,
    Tuning, TurnKind, TurnRow, Verdict, Via, Voice, WireError,
};
use crate::card::{Diff, Hunk, Row};
use crate::provider::{CancelSite, CutShort, Limit, Refusal, Usage};
use genai::chat::{
    Binary, ChatRole, ContentPart, CustomPart, MessageOptions, ToolCall, ToolResponse,
};
use ral_core::carrier::json::RawSevered;
use ral_core::data::json::{
    JsonDecode, JsonEncode, JsonFormat, JsonRef, RawDatum, RawFOValue, decode_structural,
    drop_value, on_json_stack, serialize_structural,
};
use ral_core::fact::{
    Count, CountRule, CredentialFailure, DecodeCause, DecodeFailure, DecodeLength, Done,
    JsonEncodeCause, JsonEncodeCauseNonUnicodePath, JsonEncodeFailure, JsonShape, JsonShapeFailure,
    ProviderRefusalCause, Reason, RecordingFailure, RetryableProviderCause, SealSpellingFailure,
    ServiceFailure, TimestampParseFailure, UnknownField, UnknownTag, ViaFailure,
    ViaFailureDuplicateTag, WriteOutcome,
};
use serde::Serialize;

// Selected raw fields never invoke semantic serde validators.
trait Mirror: Sized {
    type Value;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure>;
    fn check(&self) -> Result<(), DecodeFailure>;
    fn hydrate(&self) -> Self::Value;
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure>;
}

pub(super) fn shape(value: JsonRef<'_, '_>, expected: &[JsonShape]) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::JsonShape(JsonShapeFailure {
        expected: expected.to_vec(),
        found: value.shape(),
    }))
}

fn length(expected: CountRule, got: usize) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::Length(DecodeLength {
        expected,
        got: got.into(),
    }))
}

pub(super) fn missing(field: &str) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::MissingField(field.into()))
}

pub(super) fn duplicate(field: &str) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::DuplicateField(field.into()))
}

fn unknown_tag(tag: &str, offered: &[&str]) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
        tag: tag.into(),
        offered: offered.iter().map(|s| (*s).into()).collect(),
    }))
}

enum Fields<'d, 'a> {
    Object(Vec<(&'d str, JsonRef<'d, 'a>)>),
    Sequence(Vec<JsonRef<'d, 'a>>),
}

impl<'d, 'a> Fields<'d, 'a> {
    fn product(value: JsonRef<'d, 'a>) -> Result<Self, DecodeFailure> {
        if let Some(members) = value.members() {
            Ok(Self::Object(members.collect()))
        } else if let Some(items) = value.items() {
            Ok(Self::Sequence(items.collect()))
        } else {
            Err(shape(value, &[JsonShape::Array, JsonShape::Object]))
        }
    }

    fn object(value: JsonRef<'d, 'a>) -> Result<Self, DecodeFailure> {
        value
            .members()
            .map(|m| Self::Object(m.collect()))
            .ok_or_else(|| shape(value, &[JsonShape::Object]))
    }
}

macro_rules! absent {
    (optional, $name:expr, $index:expr, $count:expr, $sequence:expr) => {
        if $sequence {
            Err(length(CountRule::Exactly(Count::from($count)), $index))
        } else {
            Ok(None)
        }
    };
    (required, $name:expr, $index:expr, $count:expr, $sequence:expr) => {
        Err(if $sequence {
            length(CountRule::Exactly(Count::from($count)), $index)
        } else {
            missing($name)
        })
    };
}

// Product decoding keeps input order, duplicates and positional struct forms.
macro_rules! read_fields {
    ($input:expr, $strict:expr, $($ctor:ident)::+, { $($name:ident: $ty:ty => $presence:ident),* $(,)? }) => {{
        $(let mut $name: Option<$ty> = None;)*
        let names = [$(stringify!($name)),*];
        let sequence = matches!(&$input, Fields::Sequence(_));
        match $input {
            Fields::Object(members) => for (key, value) in members {
                match key {
                    $(stringify!($name) => {
                        if $name.is_some() { return Err(duplicate(key)); }
                        $name = Some(<$ty>::read(value).map_err(|e| e.field(key))?);
                    },)*
                    _ if $strict => return Err(DecodeFailure::new(DecodeCause::UnknownField(UnknownField {
                        field: key.into(), offered: names.iter().map(|s| (*s).into()).collect(),
                    }))),
                    _ => {},
                }
            },
            Fields::Sequence(items) => {
                let mut items = items.into_iter().enumerate();
                $($name = match items.next() {
                    Some((index, value)) => Some(<$ty>::read(value).map_err(|e| e.index(index.into()))?),
                    None => None,
                };)*
                if let Some((index, _)) = items.next() {
                    return Err(length(CountRule::Exactly(names.len().into()), index + 1 + items.count()));
                }
            }
        }
        let mut index = 0usize;
        $(let $name = match $name {
            Some(value) => value,
            None => absent!($presence, stringify!($name), index, names.len(), sequence)?,
        };
        index += 1;)*
        let _ = index;
        Ok($($ctor)::+ { $($name),* })
    }};
}

fn external<'d, 'a>(
    value: JsonRef<'d, 'a>,
    offered: &[&str],
) -> Result<(&'d str, Option<JsonRef<'d, 'a>>), DecodeFailure> {
    let (tag, payload) = if let Some(tag) = value.as_str() {
        (tag, None)
    } else if let Some(mut members) = value.members() {
        if members.len() != 1 {
            return Err(length(CountRule::Exactly(1u64.into()), members.len()));
        }
        let (tag, payload) = members.next().expect("one member");
        (tag, Some(payload))
    } else {
        return Err(shape(value, &[JsonShape::String, JsonShape::Object]));
    };
    if !offered.contains(&tag) {
        return Err(unknown_tag(tag, offered));
    }
    Ok((tag, payload))
}

fn required_payload<'d, 'a>(
    payload: Option<JsonRef<'d, 'a>>,
    tag: &str,
) -> Result<JsonRef<'d, 'a>, DecodeFailure> {
    payload.ok_or_else(|| DecodeFailure::new(DecodeCause::MissingPayload(tag.into())))
}

fn unit(payload: Option<JsonRef<'_, '_>>, tag: &str) -> Result<(), DecodeFailure> {
    payload.map_or(Ok(()), |v| {
        decode_structural::<()>(v).map_err(|e| e.payload(tag))
    })
}

macro_rules! structural {
    ($($ty:ty),* $(,)?) => {$(
        impl Mirror for $ty {
            type Value = Self;
            fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> { decode_structural(v) }
            fn check(&self) -> Result<(), DecodeFailure> { Ok(()) }
            fn hydrate(&self) -> Self { self.clone() }
            fn from_value(v: &Self) -> Result<Self, JsonEncodeFailure> { Ok(v.clone()) }
        }
    )*};
}

structural!(
    String,
    bool,
    u8,
    u32,
    u64,
    usize,
    AgentId,
    JobId,
    PostId,
    Seq,
    BlockId,
    Range<u64>,
    Voice,
    Role,
    TurnKind,
    ItemKind,
    EditAuthority,
    ToolResult,
    Spent,
    Post,
    Schedule,
    Verdict,
    Tuning,
    Usage,
    CancelSite,
    Limit,
    WriteOutcome,
    crate::card::Span,
    crate::card::Field,
    crate::card::Readout,
    crate::card::Seg,
    ChatRole,
    MessageOptions,
    Binary,
    ToolResponse,
    genai::ModelIden
);

impl<T: Mirror> Mirror for Vec<T> {
    type Value = Vec<T::Value>;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        v.items()
            .ok_or_else(|| shape(v, &[JsonShape::Array]))?
            .enumerate()
            .map(|(i, v)| on_json_stack(|| T::read(v)).map_err(|e| e.index(i.into())))
            .collect()
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        for (i, value) in self.iter().enumerate() {
            value.check().map_err(|e| e.index(i.into()))?;
        }
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        self.iter().map(T::hydrate).collect()
    }
    fn from_value(v: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        v.iter()
            .enumerate()
            .map(|(i, v)| T::from_value(v).map_err(|e| e.index(i.into())))
            .collect()
    }
}

impl<T: Mirror> Mirror for Option<T> {
    type Value = Option<T::Value>;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        if v.shape() == JsonShape::Null {
            Ok(None)
        } else {
            T::read(v).map(Some)
        }
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.as_ref().map_or(Ok(()), T::check)
    }
    fn hydrate(&self) -> Self::Value {
        self.as_ref().map(T::hydrate)
    }
    fn from_value(v: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        v.as_ref().map(T::from_value).transpose()
    }
}

impl<T: Mirror> Mirror for Box<T> {
    type Value = Box<T::Value>;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        T::read(v).map(Self::new)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.as_ref().check()
    }
    fn hydrate(&self) -> Self::Value {
        Box::new(self.as_ref().hydrate())
    }
    fn from_value(v: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        T::from_value(v).map(Self::new)
    }
}

impl<T: ral_core::data::datum::Datum + Clone> Mirror for RawDatum<T> {
    type Value = T;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural(v)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.validate().map(drop)
    }
    fn hydrate(&self) -> T {
        self.validate().expect("Datum checked before hydration")
    }
    fn from_value(v: &T) -> Result<Self, JsonEncodeFailure> {
        Ok(Self::from_datum(v.clone()))
    }
}

impl Mirror for RawFOValue {
    type Value = FOValue;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural(v)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.validate().map(drop_value)
    }
    fn hydrate(&self) -> FOValue {
        self.validate().expect("data checked before hydration")
    }
    fn from_value(v: &FOValue) -> Result<Self, JsonEncodeFailure> {
        Ok(Self::from_value(v.clone()))
    }
}

impl Mirror for RawSevered {
    type Value = Severed;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural(v)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.validate_ref().map(drop)
    }
    fn hydrate(&self) -> Severed {
        self.validate_ref()
            .expect("severance checked before hydration")
    }
    fn from_value(v: &Severed) -> Result<Self, JsonEncodeFailure> {
        Ok(Self::from_value(v))
    }
}

mod leaves;
use leaves::{RawNonEmpty, RawPath, RawSpan, RawSum, RawTimestamp, RawVia, nonempty};
mod context;
use context::{RawEviction, RawLinked, RawLocation, RawLocus, RawPointer, RawSpanRow, RawTurnRow};
mod provider;
use provider::{RawCutShort, RawProviderError, RawRefusal, RawSelected};
mod chat;
use chat::RawChatMessage;
mod card;
use card::{RawChange, RawDiff, RawHunk};
mod graph;
pub(crate) use graph::RawRecord;
use graph::{
    RawChildFailure, RawChildStatus, RawControl, RawDisplay, RawForensic, RawStarted, RawTranscript,
};
mod tagged;
use tagged::{RawCard, RawMark, RawMeasure, RawRow, RawSettlement};

/// An object-only commit body. The caller owns and hashes its original bytes.
#[derive(Debug, Serialize)]
pub(crate) struct RawBody {
    at_unix_ms: u64,
    records: Vec<RawRecord>,
}

impl RawBody {
    pub(crate) fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::object(value)?;
        read_fields!(fields, true, Self, { at_unix_ms: u64 => required, records: Vec<RawRecord> => required })
    }
    pub(crate) fn check(&self) -> Result<(), DecodeFailure> {
        nonempty(self.records.len()).map_err(|e| e.field("records"))?;
        self.records.check().map_err(|e| e.field("records"))
    }
    pub(crate) fn validate(&self) -> Result<NonEmpty<Record>, DecodeFailure> {
        self.check()?;
        Ok(NonEmpty::collect(self.records.hydrate()).expect("nonempty checked"))
    }
    pub(crate) fn from_records(
        at_unix_ms: u64,
        records: &NonEmpty<Record>,
    ) -> Result<Self, JsonEncodeFailure> {
        let records = records
            .iter()
            .enumerate()
            .map(|(i, v)| RawRecord::from_value(v).map_err(|e| e.index(i.into()).field("records")))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            at_unix_ms,
            records,
        })
    }
    pub(crate) fn encode(&self, format: JsonFormat) -> Vec<u8> {
        serialize_structural(self, format)
    }
}

impl RawRecord {
    pub(crate) fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        <Self as Mirror>::read(value)
    }
    pub(crate) fn check(&self) -> Result<(), DecodeFailure> {
        <Self as Mirror>::check(self)
    }
    pub(crate) fn from_value(value: &Record) -> Result<Self, JsonEncodeFailure> {
        <Self as Mirror>::from_value(value)
    }
}

macro_rules! endpoints {
    ($($value:ty => $raw:ty),* $(,)?) => {$(
        impl JsonDecode for $value {
            fn decode_json(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
                let raw = <$raw>::read(v)?;
                raw.check()?;
                Ok(raw.hydrate())
            }
        }
        impl JsonEncode for $value {
            fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
                Ok(serialize_structural(&<$raw as Mirror>::from_value(self)?, format))
            }
        }
    )*};
}
endpoints!(Record => RawRecord, Transcript => RawTranscript, Control => RawControl,
    Display => RawDisplay, Forensic => RawForensic, Started => RawStarted,
    ChildFailure => RawChildFailure, ChildStatus => RawChildStatus, Settlement => RawSettlement,
    Locus => RawLocus, Span => RawSpan, Eviction => RawEviction, Pointer => RawPointer,
    SpanRow => RawSpanRow, TurnRow => RawTurnRow, Location => RawLocation, Linked => RawLinked,
    Selected => RawSelected, ProviderError => RawProviderError, Refusal => RawRefusal,
    CutShort => RawCutShort, Card => RawCard, Change => RawChange,
    crate::card::Mark => RawMark, crate::card::Measure => RawMeasure,
    crate::card::Row => RawRow, crate::card::Hunk => RawHunk, crate::card::Diff => RawDiff, Post => Post, Schedule => Schedule, Sum => RawSum,
    NonEmpty<Record> => RawNonEmpty<RawRecord>);

#[cfg(test)]
mod tests;
