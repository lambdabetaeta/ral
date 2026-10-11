use base64::Engine as _;
use base64::display::Base64Display;
use base64::engine::general_purpose::STANDARD;
use serde::de::{Error, Visitor};
use serde::{Deserialize, Deserializer, Serializer};
use std::fmt;

pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    if serializer.is_human_readable() {
        serializer.collect_str(&Base64Display::new(bytes, &STANDARD))
    } else {
        serializer.serialize_bytes(bytes)
    }
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    if deserializer.is_human_readable() {
        deserializer.deserialize_str(ByteVisitor)
    } else {
        Vec::deserialize(deserializer)
    }
}

struct ByteVisitor;

impl Visitor<'_> for ByteVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a base64 byte string")
    }

    fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
        STANDARD
            .decode(value)
            .map_err(|error| E::custom(format!("invalid base64 byte string: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use crate::first_order::FOValue;

    #[test]
    fn json_bytes_are_lossless_and_compact() {
        for bytes in [vec![], (0..=255).cycle().take(4096).collect()] {
            let value: FOValue = FOValue::Bytes { value: bytes };
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<FOValue>(&json).unwrap(), value);
            assert!(json.starts_with(r#"{"bytes":{"value":""#));
            assert!(json.len() < 5500);
        }
    }

    #[test]
    fn json_byte_arrays_are_refused() {
        let error =
            serde_json::from_str::<FOValue>(r#"{"bytes":{"value":[0,128,255]}}"#).unwrap_err();
        assert!(error.to_string().contains("expected a base64 byte string"));
    }

    #[test]
    fn malformed_base64_is_refused() {
        let error = serde_json::from_str::<FOValue>(r#"{"bytes":{"value":"??"}}"#).unwrap_err();
        assert!(error.to_string().contains("invalid base64 byte string"));
    }

    #[test]
    fn binary_encoding_uses_native_bytes() {
        let bytes: Vec<u8> = (0..=255).collect();
        let encoded = postcard::to_allocvec(&FOValue::<crate::first_order::NoExt>::Bytes {
            value: bytes.clone(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            [vec![5], postcard::to_allocvec(&bytes).unwrap()].concat()
        );
        assert_eq!(
            postcard::from_bytes::<FOValue>(&encoded)
                .unwrap()
                .as_bytes(),
            Some(bytes.as_slice())
        );
    }
}
