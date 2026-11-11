//! Nested record ingress keeps portable causes in their canonical Datum form.

use super::Severed;
use crate::data::json::{
    JsonDecode, JsonEncode, JsonFormat, JsonRef, RawDatum, decode_structural, serialize_structural,
};
use crate::fact::{ConnectionClosed, ConnectionFault, DecodeFailure, JsonEncodeFailure};
use crate::protocol::json::{RawDuration, RawWireError};
use serde::{Deserialize, Serialize};

/// Unvalidated serialization data for records containing a severance.
#[derive(Debug, Serialize, Deserialize)]
pub struct RawSevered(RawSeveredKind);

#[derive(Debug, Serialize, Deserialize)]
enum RawSeveredKind {
    Refused(RawWireError),
    Closed(RawDatum<ConnectionClosed>),
    Silent(RawDuration),
    Faulted(RawDatum<ConnectionFault>),
}

impl RawSevered {
    /// Validate causes outside serde's error channel.
    ///
    /// # Errors
    /// A malformed Datum cause or overflowing duration.
    pub fn validate(self) -> Result<Severed, DecodeFailure> {
        match self.0 {
            RawSeveredKind::Refused(value) => value
                .validate()
                .map(Severed::Refused)
                .map_err(|e| e.payload("Refused")),
            RawSeveredKind::Closed(value) => value
                .validate()
                .map(Severed::Closed)
                .map_err(|e| e.payload("Closed")),
            RawSeveredKind::Silent(value) => value
                .validate()
                .map(Severed::Silent)
                .map_err(|e| e.payload("Silent")),
            RawSeveredKind::Faulted(value) => value
                .validate()
                .map(Severed::Faulted)
                .map_err(|e| e.payload("Faulted")),
        }
    }

    /// Validate borrowed causes outside serde's error channel.
    ///
    /// # Errors
    /// A malformed Datum cause or overflowing duration.
    pub fn validate_ref(&self) -> Result<Severed, DecodeFailure> {
        match &self.0 {
            RawSeveredKind::Refused(value) => value
                .validate_ref()
                .map(Severed::Refused)
                .map_err(|e| e.payload("Refused")),
            RawSeveredKind::Closed(value) => value
                .validate()
                .map(Severed::Closed)
                .map_err(|e| e.payload("Closed")),
            RawSeveredKind::Silent(value) => value
                .validate_ref()
                .map(Severed::Silent)
                .map_err(|e| e.payload("Silent")),
            RawSeveredKind::Faulted(value) => value
                .validate()
                .map(Severed::Faulted)
                .map_err(|e| e.payload("Faulted")),
        }
    }

    #[must_use]
    pub fn from_value(value: &Severed) -> Self {
        Self(match value {
            Severed::Refused(value) => RawSeveredKind::Refused(RawWireError::from_value(value)),
            Severed::Closed(value) => RawSeveredKind::Closed(RawDatum::from_datum(value.clone())),
            Severed::Silent(value) => RawSeveredKind::Silent((*value).into()),
            Severed::Faulted(value) => RawSeveredKind::Faulted(RawDatum::from_datum(value.clone())),
        })
    }
}

impl JsonDecode for Severed {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawSevered>(value)?.validate()
    }
}

impl JsonEncode for Severed {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&RawSevered::from_value(self), format))
    }
}

#[cfg(test)]
mod tests;
