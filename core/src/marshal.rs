//! One codec from a parent's image to either kind of child.

use crate::data::datum::Datum;
use crate::data::json::{
    JsonDecode, JsonDocument, JsonEncode, JsonFormat, JsonRef, decode_structural,
    serialize_structural,
};
use crate::data::{Bytes, FOValue};
use crate::fact::{
    DecodeCause, DecodeFailure, FrameEncodeFailure, JsonEncodeFailure, ProtocolData,
    ProtocolFailure, ProtocolFailureDecode, ProtocolFailureUnencodable, ProtocolOperation, Reason,
};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::{Error, Image, Runtime};
use serde::{Deserialize, Serialize};

mod read;
mod wire;
mod write;

/// A shell's image and a spawn's grant as one JSON document. The host relays
/// it unread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marshalled(Bytes);

/// # Errors
/// The cwd is not Unicode.
pub(crate) fn write(image: &Image, grant: &SpawnGrant) -> Result<Marshalled, Error> {
    let wire = write::wire(image, grant).map_err(|cause| {
        Error::new(Reason::Protocol(Box::new(ProtocolFailure::Unencodable(
            ProtocolFailureUnencodable {
                operation: ProtocolOperation::Marshal,
                cause: FrameEncodeFailure::Json(cause),
            },
        ))))
    })?;
    Ok(Marshalled(
        serialize_structural(&wire, JsonFormat::Compact).into(),
    ))
}

/// Read against the receiving runtime. The grant stays unfrozen; the grant
/// stack is held again.
///
/// # Errors
/// A decode failure at the refusing path, wrapped as a malformed parent.
pub(crate) fn read(runtime: &Runtime, parent: &Marshalled) -> Result<(Image, SpawnGrant), Error> {
    let decode = || {
        let document = JsonDocument::parse(&parent.0)
            .map_err(|cause| DecodeFailure::new(DecodeCause::JsonSyntax(cause)))?;
        let wire = decode_structural::<wire::Wire<wire::RawData>>(document.root())?;
        read::image(runtime, wire)
    };
    decode().map_err(|cause| {
        Error::new(Reason::Protocol(Box::new(ProtocolFailure::Decode(
            ProtocolFailureDecode {
                kind: ProtocolData::Parent,
                operation: "child".into(),
                cause,
            },
        ))))
    })
}

impl Datum for Marshalled {
    fn encode(self) -> FOValue {
        self.0.encode()
    }

    fn decode(value: &FOValue) -> Result<Self, DecodeFailure> {
        Bytes::decode(value).map(Self)
    }
}

impl Marshalled {
    pub(crate) fn from_base64(value: &str) -> Result<Self, DecodeFailure> {
        Bytes::from_base64(value).map(Self)
    }

    pub(crate) fn to_base64(&self) -> String {
        self.0.to_base64()
    }
}

impl JsonDecode for Marshalled {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        Self::from_base64(&decode_structural::<String>(value)?)
    }
}

impl JsonEncode for Marshalled {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&self.to_base64(), format))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
