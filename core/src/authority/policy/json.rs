//! Raw policy mirrors validate paths without resolving or reholding them.

use super::exec::{ExecAllow, ExecDeny, Veto};
use super::fs::{FsAllow, FsDeny, FsLayer};
use super::lattice::{EditorVerb, Extent, Gpu, GrantLayer, GrantPath, GrantStack, Net, ShellVerb};
use super::table::Table;
use crate::data::json::{
    JsonDecode, JsonEncode, JsonFormat, JsonRef, decode_structural, serialize_structural,
};
use crate::fact::{DecodeFailure, JsonEncodeFailure};
use crate::path::{Excluded, FrozenPath};
use crate::path::{RawExcluded, RawOsString};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct RawGrantStack(Vec<RawGrantLayer>);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGrantLayer {
    exec: Option<RawTable<RawExecAllow, RawExecDeny>>,
    fs: Option<RawFsLayer>,
    net: Option<Net>,
    gpu: Option<Gpu>,
    editor: Option<BTreeSet<EditorVerb>>,
    shell: Option<BTreeSet<ShellVerb>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawTable<A, D> {
    allows: Vec<A>,
    denies: Vec<D>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawFsLayer {
    read: RawTable<RawFsAllow, RawFsDeny>,
    write: RawTable<RawFsAllow, RawFsDeny>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawGrantPath<R> {
    written: FrozenPath,
    real: R,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
enum RawExtent {
    Everywhere,
    Under {
        position: RawOsString,
        passed: Vec<RawOsString>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
enum RawExecAllow {
    Any,
    Dir(RawGrantPath<RawExtent>),
    File {
        written: FrozenPath,
        real: RawOsString,
        found_as: Option<String>,
    },
    Tool(String),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
enum RawExecDeny {
    Name(Veto),
    Dir(RawGrantPath<RawExcluded>),
    File(RawGrantPath<RawExcluded>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
enum RawFsAllow {
    Prefix(RawGrantPath<RawExtent>),
    RunPlace(RawOsString),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawFsDeny(RawGrantPath<RawExcluded>);

impl RawExtent {
    fn validate(self) -> Result<Extent, DecodeFailure> {
        match self {
            Self::Everywhere => Ok(Extent::Everywhere),
            Self::Under { position, passed } => Ok(Extent::Under {
                position: position
                    .real()
                    .map_err(|e| e.field("position").payload("Under"))?,
                passed: passed
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| {
                        p.real()
                            .map_err(|e| e.index(i.into()).field("passed").payload("Under"))
                    })
                    .collect::<Result<_, _>>()?,
            }),
        }
    }
}
impl From<&Extent> for RawExtent {
    fn from(value: &Extent) -> Self {
        match value {
            Extent::Everywhere => Self::Everywhere,
            Extent::Under { position, passed } => Self::Under {
                position: position.as_path().as_os_str().into(),
                passed: passed
                    .iter()
                    .map(|p| p.as_path().as_os_str().into())
                    .collect(),
            },
        }
    }
}
impl<R> RawGrantPath<R> {
    fn validate<T>(
        self,
        validate: impl FnOnce(R) -> Result<T, DecodeFailure>,
    ) -> Result<GrantPath<T>, DecodeFailure> {
        Ok(GrantPath {
            written: self.written,
            real: validate(self.real).map_err(|e| e.field("real"))?,
        })
    }
}
fn extent(value: &GrantPath<Extent>) -> RawGrantPath<RawExtent> {
    RawGrantPath {
        written: value.written.clone(),
        real: (&value.real).into(),
    }
}
fn excluded(value: &GrantPath<Excluded>) -> RawGrantPath<RawExcluded> {
    RawGrantPath {
        written: value.written.clone(),
        real: (&value.real).into(),
    }
}
impl RawExecAllow {
    fn validate(self) -> Result<ExecAllow, DecodeFailure> {
        Ok(match self {
            Self::Any => ExecAllow::Any,
            Self::Tool(name) => ExecAllow::Tool(name),
            Self::Dir(path) => ExecAllow::Dir(
                path.validate(RawExtent::validate)
                    .map_err(|e| e.payload("Dir"))?,
            ),
            Self::File {
                written,
                real,
                found_as,
            } => ExecAllow::File {
                written,
                real: real.real().map_err(|e| e.field("real").payload("File"))?,
                found_as,
            },
        })
    }
}
impl From<&ExecAllow> for RawExecAllow {
    fn from(value: &ExecAllow) -> Self {
        match value {
            ExecAllow::Any => Self::Any,
            ExecAllow::Tool(name) => Self::Tool(name.clone()),
            ExecAllow::Dir(path) => Self::Dir(extent(path)),
            ExecAllow::File {
                written,
                real,
                found_as,
            } => Self::File {
                written: written.clone(),
                real: real.as_path().as_os_str().into(),
                found_as: found_as.clone(),
            },
        }
    }
}
impl RawExecDeny {
    fn validate(self) -> Result<ExecDeny, DecodeFailure> {
        Ok(match self {
            Self::Name(veto) => ExecDeny::Name(veto),
            Self::Dir(path) => ExecDeny::Dir(
                path.validate(RawExcluded::validate)
                    .map_err(|e| e.payload("Dir"))?,
            ),
            Self::File(path) => ExecDeny::File(
                path.validate(RawExcluded::validate)
                    .map_err(|e| e.payload("File"))?,
            ),
        })
    }
}
impl From<&ExecDeny> for RawExecDeny {
    fn from(value: &ExecDeny) -> Self {
        match value {
            ExecDeny::Name(veto) => Self::Name(veto.clone()),
            ExecDeny::Dir(path) => Self::Dir(excluded(path)),
            ExecDeny::File(path) => Self::File(excluded(path)),
        }
    }
}
impl RawFsAllow {
    fn validate(self) -> Result<FsAllow, DecodeFailure> {
        Ok(match self {
            Self::Prefix(path) => FsAllow::Prefix(
                path.validate(RawExtent::validate)
                    .map_err(|e| e.payload("Prefix"))?,
            ),
            Self::RunPlace(path) => {
                FsAllow::RunPlace(path.real().map_err(|e| e.payload("RunPlace"))?)
            }
        })
    }
}
impl From<&FsAllow> for RawFsAllow {
    fn from(value: &FsAllow) -> Self {
        match value {
            FsAllow::Prefix(path) => Self::Prefix(extent(path)),
            FsAllow::RunPlace(path) => Self::RunPlace(path.as_path().as_os_str().into()),
        }
    }
}
impl RawFsDeny {
    fn validate(self) -> Result<FsDeny, DecodeFailure> {
        self.0.validate(RawExcluded::validate).map(FsDeny)
    }
}
impl From<&FsDeny> for RawFsDeny {
    fn from(value: &FsDeny) -> Self {
        Self(excluded(&value.0))
    }
}
impl<A, D> RawTable<A, D> {
    fn validate<B: Ord, E: Ord>(
        self,
        allow: impl Fn(A) -> Result<B, DecodeFailure>,
        deny: impl Fn(D) -> Result<E, DecodeFailure>,
    ) -> Result<Table<B, E>, DecodeFailure> {
        let allows = self
            .allows
            .into_iter()
            .enumerate()
            .map(|(i, a)| allow(a).map_err(|e| e.index(i.into()).field("allows")))
            .collect::<Result<Vec<_>, _>>()?;
        let denies = self
            .denies
            .into_iter()
            .enumerate()
            .map(|(i, d)| deny(d).map_err(|e| e.index(i.into()).field("denies")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Table::new(allows, denies))
    }
}
fn table<'a, A: Ord + 'a, D: Ord + 'a, B, E>(
    value: &'a Table<A, D>,
    allow: impl Fn(&'a A) -> B,
    deny: impl Fn(&'a D) -> E,
) -> RawTable<B, E> {
    RawTable {
        allows: value.allows().map(allow).collect(),
        denies: value.denies().map(deny).collect(),
    }
}
impl RawFsLayer {
    fn validate(self) -> Result<FsLayer, DecodeFailure> {
        Ok(FsLayer {
            read: self
                .read
                .validate(RawFsAllow::validate, RawFsDeny::validate)
                .map_err(|e| e.field("read"))?,
            write: self
                .write
                .validate(RawFsAllow::validate, RawFsDeny::validate)
                .map_err(|e| e.field("write"))?,
        })
    }
}
impl From<&FsLayer> for RawFsLayer {
    fn from(value: &FsLayer) -> Self {
        Self {
            read: table(&value.read, RawFsAllow::from, RawFsDeny::from),
            write: table(&value.write, RawFsAllow::from, RawFsDeny::from),
        }
    }
}
impl RawGrantLayer {
    fn validate(self) -> Result<GrantLayer, DecodeFailure> {
        Ok(GrantLayer {
            exec: self
                .exec
                .map(|t| t.validate(RawExecAllow::validate, RawExecDeny::validate))
                .transpose()
                .map_err(|e| e.field("exec"))?,
            fs: self
                .fs
                .map(RawFsLayer::validate)
                .transpose()
                .map_err(|e| e.field("fs"))?,
            net: self.net,
            gpu: self.gpu,
            editor: self.editor,
            shell: self.shell,
        })
    }
}
impl From<&GrantLayer> for RawGrantLayer {
    fn from(value: &GrantLayer) -> Self {
        Self {
            exec: value
                .exec
                .as_ref()
                .map(|t| table(t, RawExecAllow::from, RawExecDeny::from)),
            fs: value.fs.as_ref().map(Into::into),
            net: value.net,
            gpu: value.gpu,
            editor: value.editor.clone(),
            shell: value.shell.clone(),
        }
    }
}
impl RawGrantStack {
    pub(crate) fn validate(self) -> Result<GrantStack, DecodeFailure> {
        self.0
            .into_iter()
            .enumerate()
            .map(|(i, l)| l.validate().map_err(|e| e.index(i.into())))
            .collect::<Result<Vec<_>, _>>()
            .map(GrantStack)
    }
}
impl From<&GrantStack> for RawGrantStack {
    fn from(value: &GrantStack) -> Self {
        Self(value.layers().map(Into::into).collect())
    }
}
impl JsonDecode for GrantStack {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawGrantStack>(value)?.validate()
    }
}
impl JsonEncode for GrantStack {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        Ok(serialize_structural(&RawGrantStack::from(self), format))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::json;

    #[test]
    fn raw_grant_stack_preserves_the_existing_serialized_shape() {
        let stack = GrantStack::root();
        assert_eq!(
            json::to_vec(&stack).unwrap(),
            serde_json::to_vec(&stack).unwrap()
        );
        assert_eq!(
            json::from_slice::<GrantStack>(&json::to_vec(&stack).unwrap()).unwrap(),
            stack
        );
    }

    #[test]
    fn raw_grant_stack_keeps_deny_unknown_fields() {
        let error = json::from_slice::<GrantStack>(br#"[{"unknown":true}]"#).unwrap_err();
        assert!(matches!(error, crate::fact::JsonInputFailure::Decode(_)));
    }
    #[test]
    fn raw_grant_stack_validates_nested_paths_with_the_logical_field_path() {
        let raw = RawGrantStack(vec![RawGrantLayer {
            exec: Some(RawTable {
                allows: vec![RawExecAllow::File {
                    written: FrozenPath::from_surface("relative"),
                    real: std::ffi::OsStr::new("relative").into(),
                    found_as: None,
                }],
                denies: vec![],
            }),
            fs: None,
            net: None,
            gpu: None,
            editor: None,
            shell: None,
        }]);
        let failure = raw.validate().unwrap_err();
        assert_eq!(
            failure.path,
            vec![
                crate::fact::DataStep::Index(0usize.into()),
                crate::fact::DataStep::Field("exec".into()),
                crate::fact::DataStep::Field("allows".into()),
                crate::fact::DataStep::Index(0usize.into()),
                crate::fact::DataStep::Payload("File".into()),
                crate::fact::DataStep::Field("real".into()),
            ]
        );
        assert!(matches!(*failure.cause, crate::fact::DecodeCause::Path(_)));
    }

    #[test]
    fn raw_grant_stack_accepts_positional_layers_and_ignores_unknown_table_fields() {
        let positional = br"[[null,null,null,null,null,null]]";
        assert_eq!(
            json::from_slice::<GrantStack>(positional).unwrap(),
            GrantStack::root()
        );
        let table = br#"[{"exec":{"allows":[],"denies":[],"ignored":true}}]"#;
        assert!(json::from_slice::<GrantStack>(table).is_ok());
        let duplicate = br#"[{"exec":null,"exec":null}]"#;
        assert!(matches!(
            json::from_slice::<GrantStack>(duplicate),
            Err(crate::fact::JsonInputFailure::Decode(
                crate::fact::DecodeFailure {
                    cause,
                    ..
                }
            )) if matches!(cause.as_ref(), crate::fact::DecodeCause::DuplicateField(_))
        ));
    }
}
