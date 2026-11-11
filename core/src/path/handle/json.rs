//! Lossless path representations; validation is lexical and never reopens a hold.

use super::exclusion::{Excluded, Hold};
use super::names::{EntryName, RootName};
use super::trace::{Kind, ObjectId, RealPath, Step, Trace};
use crate::data::json::{
    JsonDecode, JsonEncode, JsonFormat, JsonRef, decode_structural, serialize_structural,
};
use crate::fact::{
    DecodeCause, DecodeFailure, JsonEncodeFailure, OsStringPlatform, OsStringPlatformFailure,
    PathDecodeFailure, TraceDecodeFailure, TraceDecodeFailurePositionMismatch,
};
use crate::path::shown;
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum RawOsString {
    Unix(Vec<u8>),
    Windows(Vec<u16>),
}

impl From<&OsStr> for RawOsString {
    fn from(value: &OsStr) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self::Unix(value.as_bytes().to_vec())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self::Windows(value.encode_wide().collect())
        }
    }
}

impl RawOsString {
    pub(crate) fn validate(self) -> Result<OsString, DecodeFailure> {
        match self {
            Self::Unix(bytes) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    Ok(OsString::from_vec(bytes))
                }
                #[cfg(windows)]
                {
                    let _ = bytes;
                    Err(platform(OsStringPlatform::Windows, OsStringPlatform::Unix))
                }
            }
            Self::Windows(wide) => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt;
                    Ok(OsString::from_wide(&wide))
                }
                #[cfg(unix)]
                {
                    let _ = wide;
                    Err(platform(OsStringPlatform::Unix, OsStringPlatform::Windows))
                }
            }
        }
    }

    pub(crate) fn entry(self) -> Result<EntryName, DecodeFailure> {
        let raw = self.validate()?;
        EntryName::parse(&raw).map_err(|error| {
            path_failure(
                &raw,
                crate::fact::LexicalPathFailure::EntryName((&error).into()),
            )
        })
    }

    pub(crate) fn root(self) -> Result<RootName, DecodeFailure> {
        let raw = self.validate()?;
        RootName::parse(&raw).map_err(|cause| path_failure(&raw, cause))
    }

    pub(crate) fn real(self) -> Result<RealPath, DecodeFailure> {
        let raw = self.validate()?;
        RealPath::parse(&raw).map_err(|cause| path_failure(&raw, cause))
    }
}

fn platform(expected: OsStringPlatform, found: OsStringPlatform) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::OsStringPlatform(OsStringPlatformFailure {
        expected,
        found,
    }))
}

fn path_failure(raw: &OsStr, cause: crate::fact::LexicalPathFailure) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::Path(PathDecodeFailure {
        shown: crate::text::quote_native(raw),
        cause,
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RawStep {
    pub(crate) name: RawOsString,
    pub(crate) id: ObjectId,
    pub(crate) kind: Kind,
}

impl From<&Step> for RawStep {
    fn from(value: &Step) -> Self {
        Self {
            name: value.name.as_os_str().into(),
            id: value.id,
            kind: value.kind,
        }
    }
}

impl RawStep {
    pub(crate) fn validate(self) -> Result<Step, DecodeFailure> {
        Ok(Step {
            name: self.name.entry().map_err(|error| error.field("name"))?,
            id: self.id,
            kind: self.kind,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RawTrace {
    pub(crate) root: RawOsString,
    pub(crate) root_id: ObjectId,
    pub(crate) steps: Vec<RawStep>,
    pub(crate) tail: Vec<RawOsString>,
    pub(crate) real: RawOsString,
    pub(crate) passed: Vec<RawOsString>,
}

impl From<&Trace> for RawTrace {
    fn from(value: &Trace) -> Self {
        Self {
            root: value.root.0.as_os_str().into(),
            root_id: value.root_id,
            steps: value.steps.iter().map(Into::into).collect(),
            tail: value
                .tail
                .iter()
                .map(|name| name.as_os_str().into())
                .collect(),
            real: value.real.as_path().as_os_str().into(),
            passed: value
                .passed
                .iter()
                .map(|path| path.as_path().as_os_str().into())
                .collect(),
        }
    }
}

impl RawTrace {
    pub(crate) fn validate(self) -> Result<Trace, DecodeFailure> {
        let root = self.root.root().map_err(|error| error.field("root"))?;
        let steps = self
            .steps
            .into_iter()
            .enumerate()
            .map(|(index, step)| {
                step.validate()
                    .map_err(|error| error.index(index.into()).field("steps"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let tail = self
            .tail
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                name.entry()
                    .map_err(|error| error.index(index.into()).field("tail"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let real = self.real.real().map_err(|error| error.field("real"))?;
        let passed = self
            .passed
            .into_iter()
            .enumerate()
            .map(|(index, path)| {
                path.real()
                    .map_err(|error| error.index(index.into()).field("passed"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !steps.split_last().is_none_or(|(last, above)| {
            above.iter().all(|step| step.kind == Kind::Dir)
                && (tail.is_empty() || last.kind == Kind::Dir)
        }) {
            return Err(DecodeFailure::new(DecodeCause::Trace(
                TraceDecodeFailure::NonDirectorySpine,
            )));
        }
        let trace = Trace::new(root, self.root_id, steps, tail, passed);
        if trace.real() != &real {
            return Err(DecodeFailure::new(DecodeCause::Trace(
                TraceDecodeFailure::PositionMismatch(TraceDecodeFailurePositionMismatch {
                    assembled: shown(trace.real().as_path()),
                    claimed: shown(real.as_path()),
                }),
            )));
        }
        Ok(trace)
    }
}

pub(crate) type RawHold = RawTrace;

impl From<&Hold> for RawTrace {
    fn from(value: &Hold) -> Self {
        value.trace().into()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RawExcluded {
    pub(crate) named: RawHold,
    pub(crate) target: Option<RawHold>,
}

impl From<&Excluded> for RawExcluded {
    fn from(value: &Excluded) -> Self {
        Self {
            named: (&value.named).into(),
            target: value.target.as_ref().map(Into::into),
        }
    }
}

impl RawExcluded {
    pub(crate) fn validate(self) -> Result<Excluded, DecodeFailure> {
        let named = Hold::unheld(
            self.named
                .validate()
                .map_err(|error| error.field("named"))?,
        );
        let target = self
            .target
            .map(|raw| {
                raw.validate()
                    .map(Hold::unheld)
                    .map_err(|error| error.field("target"))
            })
            .transpose()?;
        Ok(Excluded::new(named, target))
    }
}

macro_rules! path_json {
    ($type:ty, $raw:ty, $decode:expr, $encode:expr) => {
        impl JsonDecode for $type {
            fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
                let raw: $raw = decode_structural(value)?;
                ($decode)(raw)
            }
        }
        impl JsonEncode for $type {
            fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
                let raw: $raw = ($encode)(self);
                Ok(serialize_structural(&raw, format))
            }
        }
    };
}
path_json!(
    EntryName,
    RawOsString,
    RawOsString::entry,
    |value: &EntryName| RawOsString::from(value.as_os_str())
);
path_json!(
    RootName,
    RawOsString,
    RawOsString::root,
    |value: &RootName| RawOsString::from(value.0.as_os_str())
);
path_json!(
    RealPath,
    RawOsString,
    RawOsString::real,
    |value: &RealPath| RawOsString::from(value.as_path().as_os_str())
);
path_json!(Step, RawStep, RawStep::validate, RawStep::from);
path_json!(Trace, RawTrace, RawTrace::validate, RawTrace::from);
path_json!(
    Hold,
    RawHold,
    |raw: RawHold| raw.validate().map(Hold::unheld),
    RawTrace::from
);
path_json!(
    Excluded,
    RawExcluded,
    RawExcluded::validate,
    RawExcluded::from
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::json::{from_slice, to_vec};
    use crate::fact::{DataStep, EntryNameFailure, JsonInputFailure, LexicalPathFailure};

    fn id() -> ObjectId {
        #[cfg(unix)]
        {
            ObjectId { dev: 1, ino: 2 }
        }
        #[cfg(windows)]
        {
            ObjectId {
                volume: 1,
                id: u128::MAX,
            }
        }
    }

    fn trace() -> Trace {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        Trace::new(
            RootName::parse(root.as_ref()).unwrap(),
            id(),
            vec![Step {
                name: EntryName::parse("leaf".as_ref()).unwrap(),
                id: id(),
                kind: Kind::File,
            }],
            Vec::new(),
            Vec::new(),
        )
    }

    fn round_trip(trace: &Trace) {
        let bytes = to_vec(trace).unwrap();
        assert_eq!(bytes, serde_json::to_vec(trace).unwrap());
        assert_eq!(from_slice::<Trace>(&bytes).unwrap(), *trace);
        let hold = from_slice::<Hold>(&bytes).unwrap();
        assert_eq!(hold.trace(), trace);
        #[cfg(unix)]
        assert!(
            hold.handle_for_test().is_none(),
            "decoding must not acquire authority"
        );
    }

    #[test]
    fn trace_and_hold_json_preserve_the_existing_representation_without_reopening() {
        round_trip(&trace());
    }

    #[cfg(unix)]
    #[test]
    fn trace_json_preserves_non_unicode_os_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let original = trace();
        let raw = OsStr::from_bytes(b"leaf\xff");
        let trace = Trace::new(
            original.root,
            id(),
            vec![Step {
                name: EntryName::parse(raw).unwrap(),
                id: id(),
                kind: Kind::File,
            }],
            Vec::new(),
            Vec::new(),
        );
        round_trip(&trace);
    }

    #[cfg(windows)]
    #[test]
    fn trace_json_preserves_unpaired_utf16_and_full_width_identity() {
        use std::os::windows::ffi::OsStringExt;
        let original = trace();
        let raw = OsString::from_wide(&[0xD800, 0x0061]);
        let trace = Trace::new(
            original.root,
            id(),
            vec![Step {
                name: EntryName::parse(&raw).unwrap(),
                id: id(),
                kind: Kind::File,
            }],
            Vec::new(),
            Vec::new(),
        );
        round_trip(&trace);
    }

    #[test]
    fn foreign_os_string_platform_is_a_typed_decode_cause() {
        #[cfg(unix)]
        let raw = RawOsString::Windows(vec![0xD800]);
        #[cfg(windows)]
        let raw = RawOsString::Unix(vec![0xFF]);
        assert!(matches!(
            *raw.validate().unwrap_err().cause,
            DecodeCause::OsStringPlatform(_)
        ));
    }

    #[test]
    fn entry_name_validation_retains_rule_and_logical_field_path() {
        for (text, expected) in [
            ("", EntryNameFailure::Empty),
            (".", EntryNameFailure::Dots),
            ("..", EntryNameFailure::Dots),
            ("a/b", EntryNameFailure::Separator),
        ] {
            let mut raw = RawTrace::from(&trace());
            raw.steps[0].name = OsStr::new(text).into();
            let bytes = serialize_structural(&raw, JsonFormat::Compact);
            let JsonInputFailure::Decode(error) = from_slice::<Trace>(&bytes).unwrap_err() else {
                panic!("syntax")
            };
            assert_eq!(
                error.path,
                vec![
                    DataStep::Field("steps".into()),
                    DataStep::Index(0_usize.into()),
                    DataStep::Field("name".into())
                ]
            );
            assert!(
                matches!(*error.cause, DecodeCause::Path(PathDecodeFailure { cause: LexicalPathFailure::EntryName(cause), .. }) if cause == expected)
            );
        }
        #[cfg(windows)]
        assert!(matches!(
            *RawOsString::from(OsStr::new("a:b"))
                .entry()
                .unwrap_err()
                .cause,
            DecodeCause::Path(PathDecodeFailure {
                cause: LexicalPathFailure::EntryName(EntryNameFailure::Stream),
                ..
            })
        ));
    }

    #[test]
    fn root_and_position_checks_are_pure_lexical_causes() {
        let original = trace();
        let root = original.root.0.as_os_str();
        assert!(matches!(
            *RawOsString::from(OsStr::new("relative"))
                .root()
                .unwrap_err()
                .cause,
            DecodeCause::Path(PathDecodeFailure {
                cause: LexicalPathFailure::Relative,
                ..
            })
        ));
        assert!(matches!(
            *RawOsString::from(original.real().as_path().as_os_str())
                .root()
                .unwrap_err()
                .cause,
            DecodeCause::Path(PathDecodeFailure {
                cause: LexicalPathFailure::MoreThanRoot,
                ..
            })
        ));
        let mut parent = root.to_os_string();
        parent.push("../leaf");
        assert!(matches!(
            *RawOsString::from(parent.as_os_str())
                .real()
                .unwrap_err()
                .cause,
            DecodeCause::Path(PathDecodeFailure {
                cause: LexicalPathFailure::ParentInPosition,
                ..
            })
        ));
        let mut doubled = root.to_os_string();
        doubled.push("leaf//child");
        assert!(matches!(
            *RawOsString::from(doubled.as_os_str())
                .real()
                .unwrap_err()
                .cause,
            DecodeCause::Path(PathDecodeFailure {
                cause: LexicalPathFailure::NonCanonicalPosition,
                ..
            })
        ));
        #[cfg(windows)]
        assert!(matches!(
            *RawOsString::from(OsStr::new(r"\\server"))
                .root()
                .unwrap_err()
                .cause,
            DecodeCause::Path(PathDecodeFailure {
                cause: LexicalPathFailure::MalformedRoot,
                ..
            })
        ));
    }

    #[test]
    fn trace_shape_precedes_claimed_position_equality() {
        let mut raw = RawTrace::from(&trace());
        raw.tail.push(OsStr::new("child").into());
        assert!(matches!(
            *raw.validate().unwrap_err().cause,
            DecodeCause::Trace(TraceDecodeFailure::NonDirectorySpine)
        ));
        let mut raw = RawTrace::from(&trace());
        raw.real = raw.root.clone();
        assert!(matches!(
            *raw.validate().unwrap_err().cause,
            DecodeCause::Trace(TraceDecodeFailure::PositionMismatch(_))
        ));
    }

    #[test]
    fn raw_trace_json_keeps_struct_duplicate_unknown_and_sequence_policies() {
        let original = trace();
        let raw = RawTrace::from(&original);
        let root = serde_json::to_string(&raw.root).unwrap();
        let bytes = String::from_utf8(to_vec(&original).unwrap()).unwrap();
        let duplicate = format!("{{\"root\":{root},{}", &bytes[1..]);
        let JsonInputFailure::Decode(error) =
            from_slice::<Trace>(duplicate.as_bytes()).unwrap_err()
        else {
            panic!("syntax")
        };
        assert!(matches!(*error.cause, DecodeCause::DuplicateField(field) if field == "root"));
        assert_eq!(error.path, [] as [DataStep; 0]);
        let unknown = format!("{{\"ignored\":null,{}", &bytes[1..]);
        assert_eq!(from_slice::<Trace>(unknown.as_bytes()).unwrap(), original);
        let positional = serde_json::to_vec(&(
            raw.root,
            raw.root_id,
            raw.steps,
            raw.tail,
            raw.real,
            raw.passed,
        ))
        .unwrap();
        assert_eq!(from_slice::<Trace>(&positional).unwrap(), original);
    }

    #[test]
    fn excluded_json_validates_both_unheld_traces() {
        let trace = trace();
        let raw = RawExcluded {
            named: (&trace).into(),
            target: Some((&trace).into()),
        };
        let bytes = serialize_structural(&raw, JsonFormat::Compact);
        let excluded = from_slice::<Excluded>(&bytes).unwrap();
        assert_eq!(excluded.holds().count(), 2);
        assert_eq!(to_vec(&excluded).unwrap(), bytes);
        #[cfg(unix)]
        assert!(
            excluded
                .holds()
                .all(|hold| hold.handle_for_test().is_none())
        );
    }
}
