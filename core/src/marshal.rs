//! One codec from a parent's image to either kind of child.

use crate::data::datum::Datum;
use crate::data::json::{JsonDocument, JsonFormat, decode_structural, serialize_structural};
use crate::data::{Bytes, FOValue};
use crate::fact::{
    DecodeCause, DecodeFailure, ProtocolData, ProtocolFailure, ProtocolFailureDecode, Reason,
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
pub(crate) fn write(image: Image, grant: &SpawnGrant) -> Result<Marshalled, Error> {
    let wire = write::wire(image, grant)
        .map_err(|cause| Error::new(Reason::Protocol(Box::new(ProtocolFailure::Marshal(cause)))))?;
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
        // The raw tree and the document drop recursively inside.
        stacker::grow(read::STACK, || {
            let document = JsonDocument::parse(&parent.0)
                .map_err(|cause| DecodeFailure::new(DecodeCause::JsonSyntax(cause)))?;
            read::nesting(document.root())?;
            let wire = decode_structural::<wire::Wire<wire::RawData>>(document.root())?;
            read::image(runtime, wire)
        })
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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
