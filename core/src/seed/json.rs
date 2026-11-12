//! The seed's serialization graph, with semantic validation outside serde.

use super::table::{
    ScopeTable, SerialAtom, SerialBinding, SerialEnvSnapshot, SerialThunk, SerialValue,
};
use super::{EngineSeed, Sealed, WireHandlerFrame};
use crate::data::Bytes;
use crate::data::json::{
    JsonDecode, JsonEncode, JsonFormat, JsonRef, RawFOValue, decode_structural, on_json_stack,
    serialize_structural,
};
use crate::fact::{DecodeCause, DecodeFailure, JsonEncodeFailure};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::{FrameKind, RawContext, Terms};
use crate::source::SourceDb;
use crate::ty::{Closed, Fixings, Scheme};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawEngineSeed {
    scope_table: RawScopeTable,
    env: SerialEnvSnapshot,
    context: RawContext<Vec<RawWireHandlerFrame>>,
    sources: SourceDb,
    terms: Terms,
    fixings: Fixings,
    grant: RawSpawnGrant,
}

type RawScopeTable = Vec<Vec<(String, RawSerialBinding)>>;
type RawSerialValue = RawFOValue<RawSerialAtom>;

#[derive(Debug, Serialize, Deserialize)]
enum RawSerialAtom {
    Thunk(SerialThunk),
    Builtin(String),
    Handle(String),
}

/// A scheme is read structurally and closed by [`close`], so no serde
/// validator runs inside the audited graph.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawSerialBinding {
    value: RawSerialValue,
    scheme: Option<Arc<Scheme>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawWireHandlerFrame {
    entries: Vec<(String, RawSerialValue)>,
    catch_all: Option<RawSerialValue>,
    kind: FrameKind,
}

/// Authored grants remain unresolved until child hydration.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum RawSpawnGrant {
    Inherit,
    Base(String),
    Restrict(RawFOValue),
}

impl RawSpawnGrant {
    pub(crate) fn validate(&self) -> Result<SpawnGrant, DecodeFailure> {
        match self {
            Self::Inherit => Ok(SpawnGrant::Inherit),
            Self::Base(name) => Ok(SpawnGrant::Base(name.clone())),
            Self::Restrict(value) => value
                .validate()
                .map(SpawnGrant::Restrict)
                .map_err(|error| error.payload("Restrict")),
        }
    }

    pub(crate) fn from_value(value: &SpawnGrant) -> Self {
        match value {
            SpawnGrant::Inherit => Self::Inherit,
            SpawnGrant::Base(name) => Self::Base(name.clone()),
            SpawnGrant::Restrict(value) => Self::Restrict(RawFOValue::from_value(value.clone())),
        }
    }
}

fn validate_value(value: &RawSerialValue) -> Result<SerialValue, DecodeFailure> {
    on_json_stack(|| {
        value.validate_with(|atom| match atom {
            RawSerialAtom::Thunk(thunk) => Ok(SerialAtom::Thunk(thunk.clone())),
            RawSerialAtom::Builtin(name) => Ok(SerialAtom::Builtin(name.clone())),
            RawSerialAtom::Handle(title) => Ok(SerialAtom::Handle(title.clone())),
        })
    })
}

fn encode_value(value: &SerialValue) -> Result<RawSerialValue, JsonEncodeFailure> {
    on_json_stack(|| {
        RawFOValue::from_value_with(value.clone(), |atom| match atom {
            SerialAtom::Thunk(thunk) => Ok(RawSerialAtom::Thunk(thunk)),
            SerialAtom::Builtin(name) => Ok(RawSerialAtom::Builtin(name)),
            SerialAtom::Handle(title) => Ok(RawSerialAtom::Handle(title)),
        })
    })
}

fn close(scheme: &Arc<Scheme>) -> Result<Closed, DecodeFailure> {
    Closed::new(Arc::clone(scheme))
        .map_err(|open| DecodeFailure::new(DecodeCause::OpenScheme(open)))
}

fn raw_scheme(scheme: &Closed) -> Arc<Scheme> {
    Arc::clone(scheme.scheme())
}

impl RawSerialBinding {
    pub(crate) fn validate(&self) -> Result<SerialBinding, DecodeFailure> {
        // The scheme can fail, so it goes first: a value is dropped only by
        // its owner's iterative guard.
        let scheme = self
            .scheme
            .as_ref()
            .map(close)
            .transpose()
            .map_err(|error| error.field("scheme"))?;
        Ok(SerialBinding {
            value: validate_value(&self.value).map_err(|error| error.field("value"))?,
            scheme,
        })
    }

    pub(crate) fn from_value(value: &SerialBinding) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            value: encode_value(&value.value).map_err(|error| error.field("value"))?,
            scheme: value.scheme.as_ref().map(raw_scheme),
        })
    }
}

impl RawWireHandlerFrame {
    pub(crate) fn validate(&self) -> Result<WireHandlerFrame, DecodeFailure> {
        // The destination owns every completed value before the next can fail.
        let mut frame = WireHandlerFrame {
            entries: Vec::with_capacity(self.entries.len()),
            catch_all: None,
            kind: self.kind,
        };
        for (index, (name, value)) in self.entries.iter().enumerate() {
            let value = validate_value(value).map_err(|error| {
                error
                    .index(1usize.into())
                    .index(index.into())
                    .field("entries")
            })?;
            frame.entries.push((name.clone(), value));
        }
        frame.catch_all = self
            .catch_all
            .as_ref()
            .map(validate_value)
            .transpose()
            .map_err(|error| error.field("catch_all"))?;
        Ok(frame)
    }

    pub(crate) fn from_value(value: &WireHandlerFrame) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            entries: value
                .entries
                .iter()
                .enumerate()
                .map(|(index, (name, value))| {
                    Ok((
                        name.clone(),
                        encode_value(value).map_err(|error| {
                            error
                                .index(1usize.into())
                                .index(index.into())
                                .field("entries")
                        })?,
                    ))
                })
                .collect::<Result<_, JsonEncodeFailure>>()?,
            catch_all: value
                .catch_all
                .as_ref()
                .map(encode_value)
                .transpose()
                .map_err(|error| error.field("catch_all"))?,
            kind: value.kind,
        })
    }
}

fn validate_table(value: &RawScopeTable) -> Result<ScopeTable, DecodeFailure> {
    value
        .iter()
        .enumerate()
        .map(|(row, entries)| {
            entries
                .iter()
                .enumerate()
                .map(|(entry, (name, binding))| {
                    Ok((
                        name.clone(),
                        binding.validate().map_err(|error| {
                            error
                                .index(1usize.into())
                                .index(entry.into())
                                .index(row.into())
                        })?,
                    ))
                })
                .collect()
        })
        .collect()
}

fn encode_table(value: &ScopeTable) -> Result<RawScopeTable, JsonEncodeFailure> {
    value
        .iter()
        .enumerate()
        .map(|(row, entries)| {
            entries
                .iter()
                .enumerate()
                .map(|(entry, (name, binding))| {
                    Ok((
                        name.clone(),
                        RawSerialBinding::from_value(binding).map_err(|error| {
                            error
                                .index(1usize.into())
                                .index(entry.into())
                                .index(row.into())
                        })?,
                    ))
                })
                .collect()
        })
        .collect()
}

impl RawEngineSeed {
    fn validate(self) -> Result<EngineSeed, DecodeFailure> {
        Ok(EngineSeed {
            scope_table: validate_table(&self.scope_table)
                .map_err(|error| error.field("scope_table"))?,
            env: self.env,
            context: self
                .context
                .validate_with(|frames| {
                    frames
                        .iter()
                        .enumerate()
                        .map(|(index, frame)| {
                            frame.validate().map_err(|error| error.index(index.into()))
                        })
                        .collect()
                })
                .map_err(|error| error.field("context"))?,
            sources: self.sources,
            terms: self.terms,
            fixings: self.fixings,
            grant: self
                .grant
                .validate()
                .map_err(|error| error.field("grant"))?,
        })
    }

    fn from_value(value: &EngineSeed) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            scope_table: encode_table(&value.scope_table)
                .map_err(|error| error.field("scope_table"))?,
            env: value.env.clone(),
            context: RawContext::from_value_with(&value.context, |frames| {
                frames
                    .iter()
                    .enumerate()
                    .map(|(index, frame)| {
                        RawWireHandlerFrame::from_value(frame)
                            .map_err(|error| error.index(index.into()))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .map_err(|error| error.field("context"))?,
            sources: value.sources.clone(),
            terms: value.terms.clone(),
            fixings: value.fixings.snapshot(),
            grant: RawSpawnGrant::from_value(&value.grant),
        })
    }
}

impl JsonDecode for EngineSeed {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawEngineSeed>(value)?.validate()
    }
}

impl JsonEncode for EngineSeed {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(
            &RawEngineSeed::from_value(self)?,
            format,
        ))
    }
}

macro_rules! codec {
    ($semantic:ty => $raw:ty) => {
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

codec!(SerialBinding => RawSerialBinding);
codec!(WireHandlerFrame => RawWireHandlerFrame);

impl JsonDecode for SerialValue {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        validate_value(&decode_structural::<RawSerialValue>(value)?)
    }
}

impl JsonEncode for SerialValue {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&encode_value(self)?, format))
    }
}

impl JsonDecode for ScopeTable {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        validate_table(&decode_structural::<RawScopeTable>(value)?)
    }
}

impl JsonEncode for ScopeTable {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&encode_table(self)?, format))
    }
}

impl Sealed {
    pub(crate) fn from_base64(value: &str) -> Result<Self, DecodeFailure> {
        Bytes::from_base64(value).map(Self)
    }

    pub(crate) fn to_base64(&self) -> String {
        self.0.to_base64()
    }
}

impl JsonDecode for Sealed {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        Self::from_base64(&decode_structural::<String>(value)?)
    }
}

impl JsonEncode for Sealed {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&self.to_base64(), format))
    }
}

#[cfg(test)]
mod tests;
