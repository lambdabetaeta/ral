use super::{LeafMode, RealPath, Trace, walk_lenient};
use crate::path::LexicalPath;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// `ELOOP`'s wording; `io::ErrorKind::FilesystemLoop` is not yet stable.
pub(super) const LOOP_MESSAGE: &str = "too many levels of symbolic links";

/// A walk's refusal of a link, or of a name still splicing past the budget.
#[derive(Debug)]
struct Looped {
    trace: Option<Box<Trace>>,
}

impl fmt::Display for Looped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(LOOP_MESSAGE)
    }
}

impl std::error::Error for Looped {}

pub(super) fn loop_error() -> io::Error {
    io::Error::other(Looped { trace: None })
}

pub(super) fn loop_at(trace: Trace) -> io::Error {
    io::Error::other(Looped {
        trace: Some(Box::new(trace)),
    })
}

/// The actual link where the splice budget ran out, never its unresolved tail.
pub(crate) fn loop_trace(e: &io::Error) -> Option<&Trace> {
    e.get_ref()?.downcast_ref::<Looped>()?.trace.as_deref()
}

/// What a failed walk of `path` leaves to judge before its error is told: the
/// link it looped at, else the name walked leniently.
///
/// # Errors
/// The lenient walk's, where it ends no steps: nothing walked, nothing to judge.
pub(crate) fn failure_trace(path: &LexicalPath, error: &io::Error) -> io::Result<Trace> {
    loop_trace(error)
        .cloned()
        .map_or_else(|| walk_lenient(path, LeafMode::Follow), Ok)
}

/// Whether `e` is the walk's `ELOOP`: a link met where none is allowed, or a
/// splice budget spent.
pub(crate) fn is_loop(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<Looped>)
}

/// A walk's refusal where the link at the spelling's last name leads nowhere:
/// the `NotFound` met past it, and the target the link named.  Shows as that
/// `NotFound`.
#[derive(Debug)]
struct Dangling {
    target: PathBuf,
    error: io::Error,
}

impl fmt::Display for Dangling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for Dangling {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub(super) fn dangling_error(source: io::Error, target: PathBuf) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        Dangling {
            target,
            error: source,
        },
    )
}

/// The target a link at the spelling's last name named, as the link spelled
/// it, where the walk could not reach it: read off its `NotFound`, as
/// [`is_loop`] reads `ELOOP`.  `None` for any other error.
pub(crate) fn dangling(e: &io::Error) -> Option<&Path> {
    e.get_ref()?
        .downcast_ref::<Dangling>()
        .map(|d| d.target.as_path())
}

/// A directory sync's refusal: the position of the held directory whose sync,
/// or the probe deciding it, failed, as the walk's trace records it, and the
/// OS's error.  Shows as that error.
#[derive(Debug)]
struct Unsynced {
    at: RealPath,
    error: io::Error,
}

impl fmt::Display for Unsynced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for Unsynced {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub(super) fn unsynced_error(source: io::Error, at: RealPath) -> io::Error {
    io::Error::new(source.kind(), Unsynced { at, error: source })
}

/// The held directory a sync failed at and the OS's error, taken out of `e`;
/// any other error handed back unchanged.
///
/// # Errors
/// `e` itself, where it is no directory sync's refusal.
pub(crate) fn unsynced(e: io::Error) -> Result<(RealPath, io::Error), io::Error> {
    e.downcast::<Unsynced>().map(|u| (u.at, u.error))
}

/// A staged write's refusal where the sibling's create was refused: the
/// directory's refusal, not the leaf's.  Shows as the words that say so.
#[derive(Debug)]
struct DirectoryRefused;

impl fmt::Display for DirectoryRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("its directory is not writable")
    }
}

impl std::error::Error for DirectoryRefused {}

pub(super) fn directory_error() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, DirectoryRefused)
}

/// Whether `e` is a staged write's [`DirectoryRefused`]: read off its
/// `PermissionDenied`, as [`is_loop`] reads `ELOOP`.  `false` for any other
/// error, including the leaf's own refusal.
#[cfg(all(test, unix))]
pub(crate) fn directory_refused(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<DirectoryRefused>)
}

/// The error [`Located::stage`](super::held::Located::stage) returns where the
/// sibling's create is refused, built as it builds it, for the tests that
/// render it where no directory can be made unwritable (root writes every
/// directory).
#[cfg(test)]
pub(crate) fn directory_refusal_for_test() -> io::Error {
    directory_error()
}

/// The custom path payloads, including every wrapper's original source.
pub(crate) fn io_failure(error: &io::Error) -> Option<crate::fact::IoCause> {
    use crate::fact::{
        IoCause, IoContext, IoLeaf, PathIoContext, PathIoContextDangling, PathIoContextUnsynced,
        PathIoLeaf, PathIoLeafEntryName, PathIoLeafLoop,
    };
    let inner = error.get_ref()?;
    let leaf = if let Some(leaf) = inner.downcast_ref::<PathIoLeaf>() {
        leaf.clone()
    } else if let Some(looped) = inner.downcast_ref::<Looped>() {
        PathIoLeaf::Loop(PathIoLeafLoop {
            at: looped
                .trace
                .as_ref()
                .map(|trace| crate::path::shown(trace.real().as_path())),
        })
    } else if let Some(dangling) = inner.downcast_ref::<Dangling>() {
        return Some(crate::io::failure(&dangling.error).at(IoContext::Path(
            PathIoContext::Dangling(PathIoContextDangling {
                target: crate::path::shown(&dangling.target),
            }),
        )));
    } else if let Some(unsynced) = inner.downcast_ref::<Unsynced>() {
        return Some(crate::io::failure(&unsynced.error).at(IoContext::Path(
            PathIoContext::Unsynced(PathIoContextUnsynced {
                at: crate::path::shown(unsynced.at.as_path()),
            }),
        )));
    } else if inner.is::<DirectoryRefused>() {
        PathIoLeaf::DirectoryNotWritable
    } else {
        let name = inner.downcast_ref::<super::names::NotAnEntryName>()?;
        PathIoLeaf::EntryName(PathIoLeafEntryName {
            name: crate::path::shown(name.name()),
            cause: name.into(),
        })
    };
    Some(IoCause::new(IoLeaf::Path(leaf)))
}

impl fmt::Display for crate::fact::PathIoLeaf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}
impl std::error::Error for crate::fact::PathIoLeaf {}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use crate::fact::*;

    #[test]
    fn intrinsic_path_leaves_do_not_become_os_sentences() {
        let text = || "entry".to_owned();
        let leaves = [
            PathIoLeaf::DirectoryNotWritable,
            PathIoLeaf::NotHeld,
            PathIoLeaf::LinkHasNoData,
            PathIoLeaf::RootHasNoEntry,
            PathIoLeaf::UpdateNeedsHeldFile,
            PathIoLeaf::SpecialFileNotReplaceable,
            PathIoLeaf::NotDirectory(text()),
            PathIoLeaf::NotLink(text()),
            PathIoLeaf::RelativeWalk,
            PathIoLeaf::MalformedRoot,
            PathIoLeaf::MoreThanRoot(text()),
            PathIoLeaf::ParentInPosition(text()),
            PathIoLeaf::NonCanonicalPosition(text()),
            PathIoLeaf::EmptyLinkTarget,
            PathIoLeaf::VettingRefused,
            PathIoLeaf::AbsoluteRequired(text()),
            PathIoLeaf::EntryName(PathIoLeafEntryName {
                name: text(),
                cause: EntryNameFailure::Separator,
            }),
            PathIoLeaf::NoFileEntry,
            PathIoLeaf::DirectoryChanged,
            PathIoLeaf::EntryMoved(text()),
            PathIoLeaf::StoredNameUnheld(PathIoLeafStoredNameUnheld {
                requested: text(),
                answered: "other".into(),
            }),
            PathIoLeaf::StoredNameAmbiguous(PathIoLeafStoredNameAmbiguous {
                name: text(),
                spellings: 2_usize.into(),
            }),
            PathIoLeaf::Untraversable(text()),
            PathIoLeaf::PrivateAncestorOwnerOrMode,
            PathIoLeaf::PrivateDirectoryOwnerOrMode,
            PathIoLeaf::ExtendedAcl,
            PathIoLeaf::NoProtectedBoundary(PathIoLeafNoProtectedBoundary {
                limit: 256_usize.into(),
            }),
            PathIoLeaf::ScopedDirectoryReparse,
            PathIoLeaf::OwnerOnlyDaclMismatch,
            PathIoLeaf::UnsafeAncestorDacl,
            PathIoLeaf::KernelReplyShort(KernelReply::MacosAttributes),
            PathIoLeaf::KernelReplyShort(KernelReply::WindowsInformation),
            PathIoLeaf::VolumeAttributesUnsupported(PathIoLeafVolumeAttributesUnsupported {
                required: 7,
                returned: 3,
            }),
            PathIoLeaf::MissingPathTerminator,
            PathIoLeaf::UnsupportedReparseTag(0x1234),
            PathIoLeaf::RequiredInformationClass(PathIoLeafRequiredInformationClass {
                operation: WindowsPathOperation::Rename,
                class: WindowsInformationClass::FileRenameInformationEx,
                cause: (&io::Error::from_raw_os_error(87)).into(),
            }),
            PathIoLeaf::Loop(PathIoLeafLoop { at: None }),
        ];
        for leaf in leaves {
            assert_eq!(
                crate::io::failure(&io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    leaf.clone()
                )),
                IoCause::new(IoLeaf::Path(leaf))
            );
        }
    }

    #[test]
    fn source_wrappers_contribute_one_outermost_first_link_each() {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        let at = RealPath::unwalked_for_test(root);
        let source = io::Error::other(PathIoLeaf::NotHeld);
        let source = dangling_error(source, PathBuf::from("target"));
        let source = unsynced_error(source, at.clone());
        let cause = crate::io::failure(&source);
        assert_eq!(*cause.leaf, IoLeaf::Path(PathIoLeaf::NotHeld));
        assert_eq!(
            cause.context,
            vec![
                IoContext::Path(PathIoContext::Unsynced(PathIoContextUnsynced {
                    at: crate::path::shown(at.as_path())
                })),
                IoContext::Path(PathIoContext::Dangling(PathIoContextDangling {
                    target: "target".into()
                })),
            ]
        );
        assert_eq!(
            *crate::io::failure(&loop_error()).leaf,
            IoLeaf::Path(PathIoLeaf::Loop(PathIoLeafLoop { at: None }))
        );
    }
}
