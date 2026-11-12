//! The serialized context, with validation outside serde's error channel.

use super::Context;
use crate::authority::policy::json::RawGrantStack;
use crate::data::json::{
    JsonDecode, JsonEncode, JsonFormat, JsonRef, decode_structural, serialize_structural,
};
use crate::environ::EnvVars;
use crate::fact::{
    DecodeCause, DecodeFailure, JsonEncodeCause, JsonEncodeCauseNonUnicodePath, JsonEncodeFailure,
};
use crate::path::CwdPath;
use crate::seed::WireHandlerFrame;
use crate::seed::json::RawWireHandlerFrame;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawContext<H> {
    env_overrides: imbl::HashMap<String, String>,
    cwd: String,
    grants: RawGrantStack,
    handlers: H,
    args: Vec<String>,
}

impl<H> RawContext<H> {
    /// The handler adapter owns its descendants; this envelope adds only its field.
    pub(crate) fn validate_with<G>(
        self,
        handlers: impl FnOnce(H) -> Result<G, DecodeFailure>,
    ) -> Result<Context<G>, DecodeFailure> {
        Ok(Context {
            env_overrides: EnvVars::from_json_map(self.env_overrides)
                .map_err(|error| error.field("env_overrides"))?,
            cwd: CwdPath::new(self.cwd.into()).map_err(|error| {
                DecodeFailure::new(DecodeCause::Path((&error).into())).field("cwd")
            })?,
            grants: self
                .grants
                .validate()
                .map_err(|error| error.field("grants"))?,
            handlers: handlers(self.handlers).map_err(|error| error.field("handlers"))?,
            args: self.args,
        })
    }

    pub(crate) fn from_value_with<G>(
        value: &Context<G>,
        handlers: impl FnOnce(&G) -> Result<H, JsonEncodeFailure>,
    ) -> Result<Self, JsonEncodeFailure> {
        let cwd = crate::path::utf8(value.cwd.as_path()).map_err(|_| {
            JsonEncodeFailure::new(JsonEncodeCause::NonUnicodePath(
                JsonEncodeCauseNonUnicodePath {
                    shown: crate::text::quote_native(value.cwd.as_path().as_os_str()),
                },
            ))
            .field("cwd")
        })?;
        Ok(Self {
            env_overrides: value
                .env_overrides
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            cwd: cwd.into(),
            grants: RawGrantStack::from(&value.grants),
            handlers: handlers(&value.handlers).map_err(|error| error.field("handlers"))?,
            args: value.args.clone(),
        })
    }
}

impl JsonDecode for Context<Vec<WireHandlerFrame>> {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawContext<Vec<RawWireHandlerFrame>>>(value)?.validate_with(|frames| {
            frames
                .iter()
                .enumerate()
                .map(|(index, frame)| frame.validate().map_err(|error| error.index(index.into())))
                .collect()
        })
    }
}

impl JsonEncode for Context<Vec<WireHandlerFrame>> {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        let raw = RawContext::from_value_with(self, |frames| {
            frames
                .iter()
                .enumerate()
                .map(|(index, frame)| {
                    RawWireHandlerFrame::from_value(frame)
                        .map_err(|error| error.index(index.into()))
                })
                .collect::<Result<Vec<_>, _>>()
        })?;
        Ok(serialize_structural(&raw, format))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::policy::GrantStack;
    use crate::data::json::{from_slice, to_vec, to_vec_pretty};
    use crate::fact::{Count, DataStep, JsonInputFailure, ReservedVar};
    use serde_json::json;

    type WireContext = Context<Vec<WireHandlerFrame>>;

    fn cwd() -> &'static str {
        if cfg!(windows) {
            r"C:\context"
        } else {
            "/context"
        }
    }

    fn context() -> WireContext {
        Context {
            env_overrides: EnvVars::from_pairs([("A", "value")]).unwrap(),
            cwd: CwdPath::new(cwd().into()).unwrap(),
            grants: GrantStack::root(),
            handlers: Vec::new(),
            args: vec!["first".into(), String::new()],
        }
    }

    fn envelope(handlers: &serde_json::Value) -> serde_json::Value {
        json!({
            "env_overrides": {},
            "cwd": cwd(),
            "grants": [],
            "handlers": handlers,
            "args": []
        })
    }

    fn decode(value: &serde_json::Value) -> Result<WireContext, JsonInputFailure> {
        from_slice(&serde_json::to_vec(value).unwrap())
    }

    fn failure(value: &serde_json::Value) -> DecodeFailure {
        let JsonInputFailure::Decode(error) = decode(value).unwrap_err() else {
            panic!("valid JSON syntax")
        };
        error
    }

    fn frame(value: &serde_json::Value) -> serde_json::Value {
        json!({"entries": [["handler", value]], "kind": "Within"})
    }

    #[test]
    fn raw_context_preserves_compact_pretty_and_field_spelling() {
        let context = context();
        let semantic = context.with_handlers(Vec::<()>::new());
        for (bytes, expected) in [
            (
                to_vec(&context).unwrap(),
                serde_json::to_vec(&semantic).unwrap(),
            ),
            (
                to_vec_pretty(&context).unwrap(),
                serde_json::to_vec_pretty(&semantic).unwrap(),
            ),
        ] {
            assert_eq!(bytes, expected);
            let decoded: WireContext = from_slice(&bytes).unwrap();
            assert_eq!(decoded.env_overrides, context.env_overrides);
            assert_eq!(decoded.cwd, context.cwd);
            assert_eq!(decoded.grants, context.grants);
            assert_eq!(decoded.args, context.args);
            assert!(decoded.handlers.is_empty());
        }
    }

    #[test]
    fn raw_context_keeps_positional_forms_without_adding_defaults() {
        let positional = json!([{}, cwd(), [], [], []]);
        assert!(decode(&positional).is_ok());
        let mut object = envelope(&json!([]));
        object.as_object_mut().unwrap().remove("args");
        let error = failure(&object);
        assert_eq!(error.path, [] as [DataStep; 0]);
        assert_eq!(*error.cause, DecodeCause::MissingField("args".into()));
        let mut positional = positional.as_array().unwrap().clone();
        positional.pop();
        assert!(matches!(
            *failure(&json!(positional)).cause,
            DecodeCause::Length(_)
        ));
    }

    #[test]
    fn raw_context_rejects_duplicate_known_fields_and_ignores_unknown_fields() {
        let bytes = format!(
            r#"{{"env_overrides":{{}},"cwd":{},"grants":[],"handlers":[],"args":[],"args":[]}}"#,
            serde_json::to_string(cwd()).unwrap()
        );
        let error = from_slice::<WireContext>(bytes.as_bytes()).unwrap_err();
        assert_eq!(
            error,
            JsonInputFailure::Decode(DecodeFailure::new(DecodeCause::DuplicateField(
                "args".into()
            )))
        );
        let bytes = bytes.replace(
            r#""args":[],"args":[]"#,
            r#""args":[],"ignored":{"Float":18446744073709551615},"ignored":null"#,
        );
        assert!(from_slice::<WireContext>(bytes.as_bytes()).is_ok());
    }

    #[test]
    fn raw_context_validates_environment_after_map_replacement() {
        let bytes = format!(
            r#"{{"env_overrides":{{"A":"first","A":"last"}},"cwd":{},"grants":[],"handlers":[],"args":[]}}"#,
            serde_json::to_string(cwd()).unwrap()
        );
        let decoded = from_slice::<WireContext>(bytes.as_bytes()).unwrap();
        assert_eq!(
            decoded.env_overrides.get("A").map(String::as_str),
            Some("last")
        );
        for (name, reserved) in [("PWD", ReservedVar::Pwd), ("OLDPWD", ReservedVar::Oldpwd)] {
            let mut value = envelope(&json!([]));
            value["env_overrides"] = json!({name: "value"});
            let error = failure(&value);
            assert_eq!(error.path, [DataStep::Field("env_overrides".into())]);
            assert!(
                matches!(*error.cause, DecodeCause::ReservedEnvironment(cause)
                if cause.name == name && cause.reserved == reserved)
            );
        }
        let mut value = envelope(&json!([]));
        value["env_overrides"] = json!({"pwd": "value"});
        assert_eq!(decode(&value).is_err(), cfg!(windows));
    }

    #[test]
    fn raw_context_refuses_relative_cwd_without_normalizing_it() {
        let mut value = envelope(&json!([]));
        value["cwd"] = json!("relative");
        let error = failure(&value);
        assert_eq!(error.path, [DataStep::Field("cwd".into())]);
        assert!(matches!(*error.cause, DecodeCause::Path(_)));
        value["cwd"] = json!(format!("{}/../unwalked", cwd()));
        let decoded = decode(&value).unwrap();
        assert_eq!(
            decoded.cwd.as_path(),
            std::path::Path::new(value["cwd"].as_str().unwrap())
        );
    }

    #[test]
    fn raw_context_prefixes_grant_path_validation() {
        let mut value = envelope(&json!([]));
        let relative = if cfg!(windows) {
            json!({"Windows": "relative".encode_utf16().collect::<Vec<_>>()})
        } else {
            json!({"Unix": b"relative".to_vec()})
        };
        value["grants"] = json!([{"exec": {"allows": [{"File": {
            "written": "authored", "real": relative
        }}], "denies": []}}]);
        let error = failure(&value);
        assert_eq!(
            error.path,
            [
                DataStep::Field("grants".into()),
                DataStep::Index(0usize.into()),
                DataStep::Field("exec".into()),
                DataStep::Field("allows".into()),
                DataStep::Index(0usize.into()),
                DataStep::Payload("File".into()),
                DataStep::Field("real".into()),
            ]
        );
        assert!(matches!(*error.cause, DecodeCause::Path(_)));
    }

    #[test]
    fn raw_context_handler_bytes_failure_keeps_tuple_and_payload_path() {
        let value = envelope(&json!([frame(&json!({"bytes": {"value": "!"}}))]));
        let error = failure(&value);
        assert_eq!(
            error.path,
            [
                DataStep::Field("handlers".into()),
                DataStep::Index(0usize.into()),
                DataStep::Field("entries".into()),
                DataStep::Index(0usize.into()),
                DataStep::Index(1usize.into()),
                DataStep::Payload("bytes".into()),
                DataStep::Field("value".into()),
            ]
        );
        assert!(matches!(*error.cause, DecodeCause::Base64(_)));
    }

    #[test]
    fn raw_context_handler_ir_checks_bits_but_does_not_resolve_scopes() {
        for bits in [(-0.0f64).to_bits(), f64::INFINITY.to_bits()] {
            let thunk = json!({"ext": {"Thunk": {
                "comp": {"span": null, "item": {"Return": {"Float": bits}}},
                "env": {"bindings": u32::MAX}
            }}});
            let value = envelope(&json!([frame(&thunk)]));
            if f64::from_bits(bits).is_finite() {
                let decoded = decode(&value).unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&to_vec(&decoded).unwrap())
                        .unwrap(),
                    value_with_null_catch_all(value)
                );
            } else {
                let error = failure(&value);
                assert_eq!(
                    error.path,
                    [
                        DataStep::Field("handlers".into()),
                        DataStep::Index(0usize.into()),
                        DataStep::Field("entries".into()),
                        DataStep::Index(0usize.into()),
                        DataStep::Index(1usize.into()),
                        DataStep::Payload("ext".into()),
                        DataStep::Payload("Thunk".into()),
                        DataStep::Field("comp".into()),
                        DataStep::Field("item".into()),
                        DataStep::Payload("Return".into()),
                        DataStep::Payload("Float".into()),
                    ]
                );
                assert!(
                    matches!(*error.cause, DecodeCause::FiniteBits(cause) if cause.bits == Count::from_u64(bits))
                );
            }
        }
    }

    fn value_with_null_catch_all(mut value: serde_json::Value) -> serde_json::Value {
        value["handlers"][0]["catch_all"] = serde_json::Value::Null;
        value
    }

    #[test]
    fn raw_context_handler_structural_failures_keep_their_containing_record() {
        let error = failure(&envelope(&json!([{"entries": []}])));
        assert_eq!(
            error.path,
            [
                DataStep::Field("handlers".into()),
                DataStep::Index(0usize.into())
            ]
        );
        assert_eq!(*error.cause, DecodeCause::MissingField("kind".into()));
        let bytes = format!(
            r#"{{"env_overrides":{{}},"cwd":{},"grants":[],"handlers":[{{"entries":[],"kind":"Within","kind":"Handler"}}],"args":[]}}"#,
            serde_json::to_string(cwd()).unwrap()
        );
        assert_eq!(
            from_slice::<WireContext>(bytes.as_bytes()).unwrap_err(),
            JsonInputFailure::Decode(
                DecodeFailure::new(DecodeCause::DuplicateField("kind".into()))
                    .index(0usize.into())
                    .field("handlers")
            )
        );
        assert!(decode(&envelope(&json!([[[], null, "Handler"]]))).is_ok());
    }

    #[test]
    fn raw_context_handler_catch_all_keeps_optional_and_typed_data_rules() {
        let mut value = envelope(&json!([{"entries": [], "kind": "Within"}]));
        assert!(decode(&value).is_ok());
        value["handlers"][0]["catch_all"] = json!({"float": {"value": f64::NAN.to_bits()}});
        let error = failure(&value);
        assert_eq!(
            error.path,
            [
                DataStep::Field("handlers".into()),
                DataStep::Index(0usize.into()),
                DataStep::Field("catch_all".into()),
                DataStep::Payload("float".into()),
                DataStep::Field("value".into()),
            ]
        );
        assert!(matches!(*error.cause, DecodeCause::FiniteBits(_)));
        value["handlers"][0]["catch_all"] = json!("unit");
        assert!(decode(&value).is_ok());
        value["handlers"][0]["catch_all"] = serde_json::Value::Null;
        assert!(decode(&value).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn raw_context_encoding_refuses_nonunicode_cwd_at_its_field() {
        use std::os::unix::ffi::OsStringExt;
        let mut context = context();
        context.cwd =
            CwdPath::new(std::ffi::OsString::from_vec(b"/invalid-\xff".to_vec()).into()).unwrap();
        let error = to_vec(&context).unwrap_err();
        assert_eq!(error.path, [DataStep::Field("cwd".into())]);
        assert!(matches!(error.cause, JsonEncodeCause::NonUnicodePath(_)));
    }

    #[cfg(windows)]
    #[test]
    fn raw_context_encoding_refuses_nonunicode_cwd_at_its_field() {
        use std::os::windows::ffi::OsStringExt;
        let mut path: Vec<u16> = r"C:\invalid-".encode_utf16().collect();
        path.push(0xd800);
        let mut context = context();
        context.cwd = CwdPath::new(std::ffi::OsString::from_wide(&path).into()).unwrap();
        let error = to_vec(&context).unwrap_err();
        assert_eq!(error.path, [DataStep::Field("cwd".into())]);
        assert!(matches!(error.cause, JsonEncodeCause::NonUnicodePath(_)));
    }
}
