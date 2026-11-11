//! Structural OAuth DTOs; token validation runs after decoding.

use super::{RawStore, RefreshResponse};
use ral_core::data::json::{
    JsonDecode, JsonEncode, JsonFormat, JsonRef, decode_structural, serialize_structural,
};
use ral_core::fact::{DecodeFailure, JsonEncodeFailure};

impl JsonDecode for RawStore {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural(value)
    }
}

impl JsonEncode for RawStore {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(self, format))
    }
}

impl JsonDecode for RefreshResponse {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::data::json::from_slice;
    use ral_core::fact::{DataStep, DecodeCause, JsonInputFailure};

    #[test]
    fn refresh_response_preserves_defaults_unknowns_and_exact_u64() {
        let raw: RefreshResponse = from_slice(br#"{"access_token":"a","expires_in":18446744073709551615,"refresh_token":null,"unknown":1,"unknown":2}"#).unwrap();
        assert_eq!(raw.expires_in, Some(u64::MAX));
        assert!(raw.refresh_token.is_none() && raw.id_token.is_none());
        let raw: RefreshResponse = from_slice(br#"["a",null,null,42]"#).unwrap();
        assert_eq!(raw.expires_in, Some(42));
    }

    #[test]
    fn refresh_response_missing_duplicate_and_wrong_head_are_typed_without_secrets() {
        for (input, expected) in [
            (
                br"{}".as_slice(),
                DecodeCause::MissingField("access_token".into()),
            ),
            (
                br#"{"access_token":"secret-one","access_token":"secret-two"}"#.as_slice(),
                DecodeCause::DuplicateField("access_token".into()),
            ),
        ] {
            let JsonInputFailure::Decode(failure) =
                from_slice::<RefreshResponse>(input).err().unwrap()
            else {
                panic!("structural failure")
            };
            assert_eq!(failure.cause.as_ref(), &expected);
            assert_eq!(failure.path, Vec::<DataStep>::new());
        }
        let JsonInputFailure::Decode(failure) =
            from_slice::<RefreshResponse>(br#"{"access_token":{"secret":"do-not-retain"}}"#)
                .err()
                .unwrap()
        else {
            panic!("structural failure")
        };
        assert_eq!(failure.path, [DataStep::Field("access_token".into())]);
        assert!(matches!(*failure.cause, DecodeCause::JsonShape(_)));
        assert!(!failure.describe().contains("do-not-retain"));
        assert!(matches!(
            from_slice::<RefreshResponse>(b"{"),
            Err(JsonInputFailure::Syntax(_))
        ));
    }

    #[test]
    fn store_decode_retains_nested_structural_path_and_duplicate_rules() {
        let JsonInputFailure::Decode(failure) = from_slice::<RawStore>(br#"{"accounts":{"account":{"service":"xai-grok","issued":"id","access_token":false,"refresh_token":"r","expires_at":0}}}"#).err().unwrap() else { panic!("structural failure") };
        assert_eq!(
            failure.path,
            ["accounts", "account", "access_token"].map(|key| DataStep::Field(key.into()))
        );
        assert!(matches!(*failure.cause, DecodeCause::JsonShape(_)));
        let JsonInputFailure::Decode(failure) =
            from_slice::<RawStore>(br#"{"accounts":{},"accounts":{}}"#)
                .err()
                .unwrap()
        else {
            panic!("structural failure")
        };
        assert_eq!(
            *failure.cause,
            DecodeCause::DuplicateField("accounts".into())
        );
        let raw: RawStore = from_slice(br"[null,{}]").unwrap();
        assert!(raw.host.is_none() && raw.accounts.is_empty());
    }
}
