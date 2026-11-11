//! Portable I/O failure facts.

use super::decode::{DecodeFailure, JsonEncodeFailure};
use super::shared::{Count, EntryNameFailure, JsonSyntaxFailure};
use crate::path::XdgKind;
use crate::{label, record, variant};
use strum::{IntoStaticStr, VariantArray};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoFailure {
    pub operation: IoOperation,
    pub subject: IoSubject,
    pub cause: IoCause,
}
record!(IoFailure {
    operation: "operation",
    subject: "subject",
    cause: "cause"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum IoOperation {
    Open,
    Read,
    Write,
    Append,
    Stage,
    Sync,
    List,
    Walk,
    Stat,
    ReadLink,
    Duplicate,
    CreateFile,
    CreateDirectory,
    Chdir,
    StartThread,
}
label!(IoOperation);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoSubject {
    Path(String),
    Stream(StandardStream),
    Terminal,
    Pipe,
    PipelineAnchor,
    Thread(String),
    HostFile(String),
    Unspecified,
}
variant!(IoSubject { Path(String): "path", Stream(StandardStream): "stream", Terminal: "terminal", Pipe: "pipe", PipelineAnchor: "pipeline-anchor", Thread(String): "thread", HostFile(String): "host-file", Unspecified: "unspecified" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum StandardStream {
    Stdin,
    Stdout,
    Stderr,
}
label!(StandardStream);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsFailure {
    pub kind: IoKind,
    pub raw: Option<i32>,
    pub detail: Option<String>,
}
record!(OsFailure {
    kind: "kind",
    raw: "raw",
    detail: "detail"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum IoKind {
    NotFound,
    PermissionDenied,
    ConnectionRefused,
    ConnectionReset,
    HostUnreachable,
    NetworkUnreachable,
    ConnectionAborted,
    NotConnected,
    AddrInUse,
    AddrNotAvailable,
    NetworkDown,
    BrokenPipe,
    AlreadyExists,
    WouldBlock,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    ReadOnlyFilesystem,
    StaleNetworkFileHandle,
    InvalidInput,
    InvalidData,
    TimedOut,
    WriteZero,
    StorageFull,
    NotSeekable,
    QuotaExceeded,
    FileTooLarge,
    ResourceBusy,
    ExecutableFileBusy,
    Deadlock,
    CrossesDevices,
    TooManyLinks,
    InvalidFilename,
    ArgumentListTooLong,
    Interrupted,
    Unsupported,
    UnexpectedEof,
    OutOfMemory,
    Other,
    /// Not publicly matchable on the selected stable toolchain, or a future kind.
    Unclassified,
}
label!(IoKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum OwnAct {
    Open,
    Make,
    Read,
    Write,
    Sync,
    Rename,
    Remove,
    List,
}
label!(OwnAct);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum KernelReply {
    MacosAttributes,
    WindowsInformation,
}
label!(KernelReply);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum WindowsPathOperation {
    StoredName,
    Rename,
    Remove,
}
label!(WindowsPathOperation);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum WindowsInformationClass {
    FileNormalizedNameInfo,
    FileRenameInformationEx,
    FileDispositionInformationEx,
}
label!(WindowsInformationClass);

/// Context is outermost first; rendering folds it right-to-left around the leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoCause {
    pub context: Vec<IoContext>,
    pub leaf: Box<IoLeaf>,
}
record!(IoCause {
    context: "context",
    leaf: "leaf"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoContext {
    Path(PathIoContext),
    Owned(OwnIoContext),
}
variant!(IoContext { Path(PathIoContext): "path", Owned(OwnIoContext): "owned" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathIoContext {
    StoredNameRead(PathIoContextStoredNameRead),
    UnsafeScopedInheritance,
    Dangling(PathIoContextDangling),
    Unsynced(PathIoContextUnsynced),
}
variant!(PathIoContext { StoredNameRead(PathIoContextStoredNameRead): "stored-name-read", UnsafeScopedInheritance: "unsafe-scoped-inheritance", Dangling(PathIoContextDangling): "dangling", Unsynced(PathIoContextUnsynced): "unsynced" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnIoContext {
    pub act: OwnAct,
    pub at: String,
    pub to: Option<String>,
}
record!(OwnIoContext {
    act: "act",
    at: "at",
    to: "to"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoLeaf {
    Os(OsFailure),
    Path(PathIoLeaf),
    Owned(OwnLeaf),
    WriteAbortedUnknownPrefix(OsFailure),
    Torn,
    JsonEncoding(JsonEncodeFailure),
}
variant!(IoLeaf { Os(OsFailure): "os", Path(PathIoLeaf): "path", Owned(OwnLeaf): "owned", WriteAbortedUnknownPrefix(OsFailure): "write-aborted-unknown-prefix", Torn: "torn", JsonEncoding(JsonEncodeFailure): "json-encoding" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnLeaf {
    NoBase(XdgKind),
    NoCwd(String),
    Linked(String),
    Dangling(OwnLeafDangling),
    Outside(OwnLeafOutside),
    Json(OwnLeafJson),
}
variant!(OwnLeaf { NoBase(XdgKind): "no-base", NoCwd(String): "no-cwd", Linked(String): "linked", Dangling(OwnLeafDangling): "dangling", Outside(OwnLeafOutside): "outside", Json(OwnLeafJson): "json" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnJsonCause {
    Syntax(JsonSyntaxFailure),
    Decode(DecodeFailure),
    Encode(JsonEncodeFailure),
}
variant!(OwnJsonCause { Syntax(JsonSyntaxFailure): "syntax", Decode(DecodeFailure): "decode", Encode(JsonEncodeFailure): "encode" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathIoLeaf {
    DirectoryNotWritable,
    NotHeld,
    LinkHasNoData,
    RootHasNoEntry,
    UpdateNeedsHeldFile,
    SpecialFileNotReplaceable,
    NotDirectory(String),
    NotLink(String),
    RelativeWalk,
    MalformedRoot,
    MoreThanRoot(String),
    ParentInPosition(String),
    NonCanonicalPosition(String),
    EmptyLinkTarget,
    VettingRefused,
    AbsoluteRequired(String),
    EntryName(PathIoLeafEntryName),
    NoFileEntry,
    DirectoryChanged,
    EntryMoved(String),
    StoredNameUnheld(PathIoLeafStoredNameUnheld),
    StoredNameAmbiguous(PathIoLeafStoredNameAmbiguous),
    Untraversable(String),
    PrivateAncestorOwnerOrMode,
    PrivateDirectoryOwnerOrMode,
    ExtendedAcl,
    NoProtectedBoundary(PathIoLeafNoProtectedBoundary),
    ScopedDirectoryReparse,
    OwnerOnlyDaclMismatch,
    UnsafeAncestorDacl,
    KernelReplyShort(KernelReply),
    VolumeAttributesUnsupported(PathIoLeafVolumeAttributesUnsupported),
    MissingPathTerminator,
    UnsupportedReparseTag(u32),
    RequiredInformationClass(PathIoLeafRequiredInformationClass),
    Loop(PathIoLeafLoop),
}
variant!(PathIoLeaf { DirectoryNotWritable: "directory-not-writable", NotHeld: "not-held", LinkHasNoData: "link-has-no-data", RootHasNoEntry: "root-has-no-entry", UpdateNeedsHeldFile: "update-needs-held-file", SpecialFileNotReplaceable: "special-file-not-replaceable", NotDirectory(String): "not-directory", NotLink(String): "not-link", RelativeWalk: "relative-walk", MalformedRoot: "malformed-root", MoreThanRoot(String): "more-than-root", ParentInPosition(String): "parent-in-position", NonCanonicalPosition(String): "non-canonical-position", EmptyLinkTarget: "empty-link-target", VettingRefused: "vetting-refused", AbsoluteRequired(String): "absolute-required", EntryName(PathIoLeafEntryName): "entry-name", NoFileEntry: "no-file-entry", DirectoryChanged: "directory-changed", EntryMoved(String): "entry-moved", StoredNameUnheld(PathIoLeafStoredNameUnheld): "stored-name-unheld", StoredNameAmbiguous(PathIoLeafStoredNameAmbiguous): "stored-name-ambiguous", Untraversable(String): "untraversable", PrivateAncestorOwnerOrMode: "private-ancestor-owner-or-mode", PrivateDirectoryOwnerOrMode: "private-directory-owner-or-mode", ExtendedAcl: "extended-acl", NoProtectedBoundary(PathIoLeafNoProtectedBoundary): "no-protected-boundary", ScopedDirectoryReparse: "scoped-directory-reparse", OwnerOnlyDaclMismatch: "owner-only-dacl-mismatch", UnsafeAncestorDacl: "unsafe-ancestor-dacl", KernelReplyShort(KernelReply): "kernel-reply-short", VolumeAttributesUnsupported(PathIoLeafVolumeAttributesUnsupported): "volume-attributes-unsupported", MissingPathTerminator: "missing-path-terminator", UnsupportedReparseTag(u32): "unsupported-reparse-tag", RequiredInformationClass(PathIoLeafRequiredInformationClass): "required-information-class", Loop(PathIoLeafLoop): "loop" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIoContextStoredNameRead {
    pub name: String,
}
record!(PathIoContextStoredNameRead { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIoContextDangling {
    pub target: String,
}
record!(PathIoContextDangling { target: "target" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIoContextUnsynced {
    pub at: String,
}
record!(PathIoContextUnsynced { at: "at" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnLeafDangling {
    pub at: String,
    pub to: String,
}
record!(OwnLeafDangling { at: "at", to: "to" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnLeafOutside {
    pub at: String,
    pub root: String,
}
record!(OwnLeafOutside {
    at: "at",
    root: "root"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnLeafJson {
    pub act: OwnAct,
    pub at: String,
    pub cause: OwnJsonCause,
}
record!(OwnLeafJson {
    act: "act",
    at: "at",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIoLeafEntryName {
    pub name: String,
    pub cause: EntryNameFailure,
}
record!(PathIoLeafEntryName {
    name: "name",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIoLeafStoredNameUnheld {
    pub requested: String,
    pub answered: String,
}
record!(PathIoLeafStoredNameUnheld {
    requested: "requested",
    answered: "answered"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIoLeafStoredNameAmbiguous {
    pub name: String,
    pub spellings: Count,
}
record!(PathIoLeafStoredNameAmbiguous {
    name: "name",
    spellings: "spellings"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIoLeafNoProtectedBoundary {
    pub limit: Count,
}
record!(PathIoLeafNoProtectedBoundary { limit: "limit" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIoLeafVolumeAttributesUnsupported {
    pub required: u32,
    pub returned: u32,
}
record!(PathIoLeafVolumeAttributesUnsupported {
    required: "required",
    returned: "returned"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIoLeafRequiredInformationClass {
    pub operation: WindowsPathOperation,
    pub class: WindowsInformationClass,
    pub cause: OsFailure,
}
record!(PathIoLeafRequiredInformationClass {
    operation: "operation",
    class: "class",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIoLeafLoop {
    pub at: Option<String>,
}
record!(PathIoLeafLoop { at: "at" });

impl IoFailure {
    pub fn describe(&self) -> String {
        format!(
            "could not {} {}: {}",
            self.operation.describe(),
            self.subject.describe(),
            self.cause.describe()
        )
    }
}

impl IoOperation {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl IoSubject {
    pub fn describe(&self) -> String {
        match self {
            Self::Path(path) | Self::HostFile(path) => format!("{path:?}"),
            Self::Stream(stream) => stream.describe(),
            Self::Terminal => "the terminal".into(),
            Self::Pipe => "a pipe".into(),
            Self::PipelineAnchor => "the pipeline anchor".into(),
            Self::Thread(name) => format!("thread {name:?}"),
            Self::Unspecified => "I/O".into(),
        }
    }
}

impl StandardStream {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl OsFailure {
    pub fn describe(&self) -> String {
        match self.kind {
            IoKind::NotFound => "no such file or directory".into(),
            IoKind::PermissionDenied => "permission denied".into(),
            _ => self.detail.clone().unwrap_or_else(|| self.kind.describe()),
        }
    }
}

impl IoKind {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl OwnAct {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl KernelReply {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl WindowsPathOperation {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl WindowsInformationClass {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl IoCause {
    pub fn describe(&self) -> String {
        self.context
            .iter()
            .rev()
            .fold(self.leaf.describe(), |inner, context| match context {
                IoContext::Path(context) => match context {
                    PathIoContext::StoredNameRead(name) => {
                        format!("could not read stored name {:?}: {inner}", name.name)
                    }
                    PathIoContext::UnsafeScopedInheritance => {
                        format!("unsafe scoped inheritance: {inner}")
                    }
                    PathIoContext::Dangling(link) => {
                        format!("dangling link to {:?}: {inner}", link.target)
                    }
                    PathIoContext::Unsynced(file) => {
                        format!("change at {:?} was not synced: {inner}", file.at)
                    }
                },
                IoContext::Owned(context) => match &context.to {
                    Some(to) => format!(
                        "could not {} owned file {:?} to {to:?}: {inner}",
                        context.act.describe(),
                        context.at
                    ),
                    None => format!(
                        "could not {} owned file {:?}: {inner}",
                        context.act.describe(),
                        context.at
                    ),
                },
            })
    }
}

impl IoContext {
    pub fn describe(&self) -> String {
        match self {
            Self::Path(value) => format!("path: {}", value.describe()),
            Self::Owned(value) => format!("owned: {}", value.describe()),
        }
    }
}

impl PathIoContext {
    pub fn describe(&self) -> String {
        match self {
            Self::StoredNameRead(value) => format!("stored name read: {}", value.describe()),
            Self::UnsafeScopedInheritance => "unsafe scoped inheritance".to_owned(),
            Self::Dangling(value) => format!("dangling: {}", value.describe()),
            Self::Unsynced(value) => format!("unsynced: {}", value.describe()),
        }
    }
}

impl OwnIoContext {
    pub fn describe(&self) -> String {
        format!(
            "act: {}, at: {}, to: {}",
            self.act.describe(),
            self.at,
            self.to.as_deref().unwrap_or("none")
        )
    }
}

impl IoLeaf {
    pub fn describe(&self) -> String {
        match self {
            Self::Os(cause) => cause.describe(),
            Self::Path(cause) => cause.describe(),
            Self::Owned(cause) => cause.describe(),
            Self::WriteAbortedUnknownPrefix(cause) => format!(
                "write aborted; the committed prefix is unknown: {}",
                cause.describe()
            ),
            Self::Torn => "state was left torn by a failed mutation".into(),
            Self::JsonEncoding(cause) => cause.describe(),
        }
    }
}

impl OwnLeaf {
    pub fn describe(&self) -> String {
        match self {
            Self::NoBase(value) => format!("no base: {}", <&str>::from(*value).to_owned()),
            Self::NoCwd(value) => format!("no cwd: {value}"),
            Self::Linked(value) => format!("linked: {value}"),
            Self::Dangling(value) => format!("dangling: {}", value.describe()),
            Self::Outside(value) => format!("outside: {}", value.describe()),
            Self::Json(value) => format!("json: {}", value.describe()),
        }
    }
}

impl OwnJsonCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Syntax(value) => format!("syntax: {}", value.describe()),
            Self::Decode(value) => format!("decode: {}", value.describe()),
            Self::Encode(value) => format!("encode: {}", value.describe()),
        }
    }
}

impl PathIoLeaf {
    pub fn describe(&self) -> String {
        match self {
            Self::DirectoryNotWritable => "its directory is not writable".to_owned(),
            Self::NotHeld => "not held".to_owned(),
            Self::LinkHasNoData => "link has no data".to_owned(),
            Self::RootHasNoEntry => "root has no entry".to_owned(),
            Self::UpdateNeedsHeldFile => "update needs held file".to_owned(),
            Self::SpecialFileNotReplaceable => "special file not replaceable".to_owned(),
            Self::NotDirectory(value) => format!("not directory: {value}"),
            Self::NotLink(value) => format!("not link: {value}"),
            Self::RelativeWalk => "relative walk".to_owned(),
            Self::MalformedRoot => "malformed root".to_owned(),
            Self::MoreThanRoot(value) => format!("more than root: {value}"),
            Self::ParentInPosition(value) => format!("parent in position: {value}"),
            Self::NonCanonicalPosition(value) => {
                format!("non canonical position: {value}")
            }
            Self::EmptyLinkTarget => "empty link target".to_owned(),
            Self::VettingRefused => "vetting refused".to_owned(),
            Self::AbsoluteRequired(value) => format!("absolute required: {value}"),
            Self::EntryName(value) => format!("entry name: {}", value.describe()),
            Self::NoFileEntry => "no file entry".to_owned(),
            Self::DirectoryChanged => "directory changed".to_owned(),
            Self::EntryMoved(value) => format!("entry moved: {value}"),
            Self::StoredNameUnheld(value) => format!("stored name unheld: {}", value.describe()),
            Self::StoredNameAmbiguous(value) => {
                format!("stored name ambiguous: {}", value.describe())
            }
            Self::Untraversable(value) => format!("untraversable: {value}"),
            Self::PrivateAncestorOwnerOrMode => "private ancestor owner or mode".to_owned(),
            Self::PrivateDirectoryOwnerOrMode => "private directory owner or mode".to_owned(),
            Self::ExtendedAcl => "extended acl".to_owned(),
            Self::NoProtectedBoundary(value) => {
                format!("no protected boundary: {}", value.describe())
            }
            Self::ScopedDirectoryReparse => "scoped directory reparse".to_owned(),
            Self::OwnerOnlyDaclMismatch => "owner only dacl mismatch".to_owned(),
            Self::UnsafeAncestorDacl => "unsafe ancestor dacl".to_owned(),
            Self::KernelReplyShort(value) => format!("kernel reply short: {}", value.describe()),
            Self::VolumeAttributesUnsupported(value) => {
                format!("volume attributes unsupported: {}", value.describe())
            }
            Self::MissingPathTerminator => "missing path terminator".to_owned(),
            Self::UnsupportedReparseTag(value) => {
                format!("unsupported reparse tag: {value}")
            }
            Self::RequiredInformationClass(value) => {
                format!("required information class: {}", value.describe())
            }
            Self::Loop(value) => format!("loop: {}", value.describe()),
        }
    }
}

impl PathIoContextStoredNameRead {
    pub fn describe(&self) -> String {
        format!("name: {}", self.name)
    }
}

impl PathIoContextDangling {
    pub fn describe(&self) -> String {
        format!("target: {}", self.target)
    }
}

impl PathIoContextUnsynced {
    pub fn describe(&self) -> String {
        format!("at: {}", self.at)
    }
}

impl OwnLeafDangling {
    pub fn describe(&self) -> String {
        format!("at: {}, to: {}", self.at, self.to)
    }
}

impl OwnLeafOutside {
    pub fn describe(&self) -> String {
        format!("at: {}, root: {}", self.at, self.root)
    }
}

impl OwnLeafJson {
    pub fn describe(&self) -> String {
        format!(
            "act: {}, at: {}, cause: {}",
            self.act.describe(),
            self.at,
            self.cause.describe()
        )
    }
}

impl PathIoLeafEntryName {
    pub fn describe(&self) -> String {
        format!("name: {}, cause: {}", self.name, self.cause.describe())
    }
}

impl PathIoLeafStoredNameUnheld {
    pub fn describe(&self) -> String {
        format!("requested: {}, answered: {}", self.requested, self.answered)
    }
}

impl PathIoLeafStoredNameAmbiguous {
    pub fn describe(&self) -> String {
        format!("name: {}, spellings: {}", self.name, self.spellings)
    }
}

impl PathIoLeafNoProtectedBoundary {
    pub fn describe(&self) -> String {
        format!("limit: {}", self.limit)
    }
}

impl PathIoLeafVolumeAttributesUnsupported {
    pub fn describe(&self) -> String {
        format!("required: {}, returned: {}", self.required, self.returned)
    }
}

impl PathIoLeafRequiredInformationClass {
    pub fn describe(&self) -> String {
        format!(
            "operation: {}, class: {}, cause: {}",
            self.operation.describe(),
            self.class.describe(),
            self.cause.describe()
        )
    }
}

impl PathIoLeafLoop {
    pub fn describe(&self) -> String {
        format!("at: {}", self.at.as_deref().unwrap_or("none"))
    }
}

label!(XdgKind);

impl IoCause {
    pub fn new(leaf: IoLeaf) -> Self {
        Self {
            context: Vec::new(),
            leaf: Box::new(leaf),
        }
    }

    pub fn at(mut self, context: IoContext) -> Self {
        self.context.insert(0, context);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::Datum;

    #[test]
    fn boxed_io_leaf_preserves_datum_context_order_and_json_equality() {
        let leaf = IoLeaf::Path(PathIoLeaf::NotHeld);
        let inner = IoContext::Path(PathIoContext::UnsafeScopedInheritance);
        let outer = IoContext::Owned(OwnIoContext {
            act: OwnAct::Read,
            at: "/owned".into(),
            to: None,
        });
        let cause = IoCause::new(leaf.clone())
            .at(inner.clone())
            .at(outer.clone());
        assert_eq!(cause.context, [outer, inner]);
        assert_eq!(*cause.leaf, leaf);
        let expected = crate::data::FOValue::Map {
            entries: vec![
                ("context".into(), cause.context.clone().encode()),
                ("leaf".into(), leaf.encode()),
            ],
        };
        assert_eq!(cause.clone().encode(), expected);
        assert_eq!(IoCause::decode(&expected), Ok(cause.clone()));
        let bytes = crate::data::json::to_datum_vec(&cause);
        assert_eq!(
            crate::data::json::from_datum_slice::<IoCause>(&bytes),
            Ok(cause.clone())
        );
        assert_eq!(
            IoCause::decode(&expected).unwrap().describe(),
            cause.describe()
        );
    }
}
