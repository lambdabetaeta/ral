//! The normal-form path types the grant pipeline trusts: [`LexicalPath`] on
//! the access side, [`FrozenPath`] on the grant side, and [`RealPath`], the
//! position a walk ends at and exec authority judges.
//!
//! A [`FrozenPath`] is a spelling and nothing else: folded, shown, ordered.
//! What it names is the walk's to say, once, when the grant is made
//! (`GrantPath { written, real }`); no containment or resolution is asked of
//! the spelling.  There is no `From<&str>` sugar: the constructors, [`from_surface`]
//! and [`freeze`], keep the fold in one place.
//!
//! [`from_surface`]: FrozenPath::from_surface
//! [`freeze`]: FrozenPath::freeze

use super::lex::{is_absolute, is_working_directory, utf8};
use super::site::Silent;
use super::walk::{EntryName, RealPath};
use super::{GuestPath, PathRules};
use crate::path::shown;
use crate::site::site;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A working directory as ral holds one: a path a relative one is read
/// against.
///
/// Absolute under the host's rules and, on Windows, on a drive, a share, or
/// the verbatim spelling of either ([`is_working_directory`]), so whatever
/// [`anchor`](super::lex::anchor) reads against it, a relative, a root-relative
/// or a drive-relative path, lands on that root and comes out absolute.
///
/// Checked at its constructor, [`new`](Self::new), which a copy read off the wire
/// passes through again; there is no other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PathBuf", into = "PathBuf")]
pub struct CwdPath(PathBuf);

impl CwdPath {
    /// `path` as a working directory.
    ///
    /// # Errors
    /// [`NotACwd`] when `path` is relative, or, on Windows, on a malformed or
    /// mixed head, past which no name joins without assembling a share or a
    /// drive the path never named (`\\srv` and `sh` would make `\\srv\sh`).
    pub fn new(path: PathBuf) -> Result<Self, NotACwd> {
        if is_working_directory(&path) {
            Ok(Self(path))
        } else {
            Err(NotACwd(path))
        }
    }

    /// A borrow, for a join or a child's `current_dir`.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume into the owned `PathBuf`.
    pub fn into_inner(self) -> PathBuf {
        self.0
    }
}

impl TryFrom<PathBuf> for CwdPath {
    type Error = NotACwd;

    fn try_from(path: PathBuf) -> Result<Self, NotACwd> {
        Self::new(path)
    }
}

impl AsRef<Path> for CwdPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl From<CwdPath> for PathBuf {
    fn from(cwd: CwdPath) -> Self {
        cwd.0
    }
}

/// A path refused as a working directory by [`CwdPath::new`].  Shows as the
/// sentence naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotACwd(PathBuf);

impl std::fmt::Display for NotACwd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let on = match PathRules::HOST {
            PathRules::Posix => "",
            PathRules::Windows => " on a drive or a share",
        };
        write!(
            f,
            "{} cannot be a working directory: it is not an absolute path{on}",
            shown(&self.0)
        )
    }
}

impl std::error::Error for NotACwd {}

/// A lexically-resolved path: absolute, sigils expanded, anchored against
/// the logical cwd, its `.`, `..` and any trailing separator kept as
/// written.  No disk is consulted to make one.
///
/// What reaches the disk takes it unfolded, for the physical resolvers to
/// read as the kernel does; a logical consumer (`cd`, `absolute-path`, a
/// display base) asks for [`folded`](Self::folded).  The walk refuses a head
/// `std::path` cannot parse ([`is_unparsed_head`](super::lex::is_unparsed_head)).
///
/// Reified so that a walk can only follow resolution — there is no way to
/// walk a path that has not first been anchored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalPath(PathBuf);

impl LexicalPath {
    /// Wrap the output of [`super::lex::anchor`], asserting exactly what
    /// that kernel guarantees — no further normalisation happens here.  The
    /// public entries are [`super::Resolver::resolve`] and
    /// [`super::Resolver::anchor`].
    ///
    /// Asserted rather than checked, because the kernel is total on what it
    /// is given: an absolute input it keeps, and every other it reads against
    /// a [`CwdPath`], whose root each reading keeps, so the join is absolute,
    /// hence not empty.  A check here would have no refusal to give a caller.
    pub(super) fn from_lexed(path: PathBuf) -> Self {
        debug_assert!(
            is_absolute(&path),
            "LexicalPath must be absolute, got {}",
            shown(&path)
        );
        debug_assert!(
            !path.as_os_str().is_empty(),
            "LexicalPath must be non-empty",
        );
        Self(path)
    }

    /// `path` as a spelling, where it is absolute: what a held path is when
    /// it must be walked again, such as a `PATH` entry or the running binary.
    pub(crate) fn absolute(path: &Path) -> Option<Self> {
        is_absolute(path).then(|| Self::from_lexed(path.to_path_buf()))
    }

    /// The lexical fold, a fixed point of [`fold_dots`](super::lex::fold_dots):
    /// no `.` or `..` past the root, no trailing separator, and on Windows one
    /// separator throughout, `\`, but in a head whose kind or meaning turns on
    /// its spelling, which is kept as written.  A malformed or mixed head
    /// ([`HeadKind::Malformed`](super::lex::HeadKind::Malformed)) is all head,
    /// and comes back as written: a fold across it could assemble a share or
    /// a drive it never named.
    #[must_use]
    pub fn folded(self) -> Self {
        Self::from_lexed(super::lex::fold_dots(&self.0))
    }

    /// A borrow, for the disk operation the check authorised.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume into the owned `PathBuf` a caller opens or stats.
    pub fn into_inner(self) -> PathBuf {
        self.0
    }

    /// The spelling of the entry `name` of the directory this spells: the one
    /// name more, and no resolution.
    #[must_use]
    pub(crate) fn join(&self, name: &EntryName) -> Self {
        Self::from_lexed(self.0.join(name.as_os_str()))
    }

    /// This path, [folded](Self::folded), as a working directory, for `cd`,
    /// `within [dir: …]`, or a script's own directory.
    ///
    /// # Errors
    /// [`CwdPath::new`]'s: a lexical path may be on a malformed or mixed
    /// head, which no working directory is.
    pub fn into_cwd(self) -> Result<CwdPath, NotACwd> {
        CwdPath::new(self.folded().0)
    }

    /// For audit fields and denial messages.
    pub(crate) fn shown(&self) -> String {
        shown(&self.0)
    }

    /// True iff this path names the host's discard device — `/dev/null` on
    /// Unix, `\\.\NUL` on Windows ([`super::device::is_discard_device`] holds both
    /// tables).
    ///
    /// The one question two checks ask about such a write: the in-process
    /// guard (`Shell::locate`), which calls it no *access*, and the
    /// observation fan-out (`Shell::observe_stamped`), which calls
    /// it no *mutation* — nothing changed in the world, so nothing is
    /// reported.
    pub(crate) fn is_discard(&self) -> bool {
        super::device::is_discard_device(
            self.folded_name().as_os_str().as_encoded_bytes(),
            PathRules::HOST,
        )
    }

    /// The refusal owed this path if, on Windows, its last component is a DOS
    /// reserved device name ([`super::device::reserved_device_refusal`]); `None`
    /// everywhere else.  The fs operations ask it of every name they are about to
    /// act on, as they ask [`is_discard`](Self::is_discard).
    pub(crate) fn reserved_device_refusal(&self) -> Option<String> {
        super::device::reserved_device_refusal(
            self.folded_name().as_os_str().as_encoded_bytes(),
            PathRules::HOST,
        )
    }

    /// The folded name as the device tables match it: a device is a name.
    fn folded_name(&self) -> PathBuf {
        super::lex::fold_dots(&self.0)
    }
}

/// A frozen grant prefix: the author's spelling, in the normal form
/// [`LexicalPath::folded`] gives.
///
/// That form is absolute, `.`/`..`-collapsed, and on Windows spelled with
/// `\` wherever its head allows (`C:/a/b` is frozen as `C:\a\b`).  The
/// derived `Ord` is by spelling, so a `BTreeSet` dedups by the string the
/// author wrote, and the wire form is that string.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FrozenPath {
    surface: String,
}

impl FrozenPath {
    /// Mint a prefix from a surface form: fold it.  Idempotent on a normal
    /// form, such as the OS-sandbox renderer emits.
    ///
    /// The root is one spelling, `/`, on every host: it is the universal
    /// prefix, and Windows has no `/` to walk (a drive's root is one drive's).
    pub fn from_surface(surface: &str) -> Self {
        let folded = super::lex::fold_dots_under(surface, PathRules::HOST);
        let surface = if super::lex::is_bare_root(&folded, PathRules::HOST) {
            "/".into()
        } else {
            folded
        };
        Self { surface }
    }

    /// [`from_surface`](Self::from_surface) for a path held natively, as a
    /// grant is serialized as text.
    ///
    /// # Errors
    /// The refusal to hold `path` as a grant, saying why: it is not valid
    /// Unicode.
    pub fn freeze(path: &Path) -> Result<Self, String> {
        utf8(path).map(Self::from_surface).map_err(|_| {
            format!(
                "ral cannot hold '{}' as a grant: it is not valid Unicode; grant a path that is",
                shown(path),
            )
        })
    }

    /// The filesystem root, as the prefix that covers every path: the
    /// implicit ceiling a policy attenuates down from.
    #[must_use]
    pub fn root() -> Self {
        Self::from_surface("/")
    }

    /// Mint a prefix naming a path inside the Linux guest, whichever host
    /// mints it.
    ///
    /// The in-process guard that matches it runs *inside* the machine, so the
    /// fold that must agree with the access side is the one under Linux's
    /// rules ([`fold_dots_under`](super::lex::fold_dots_under)), not this
    /// process's — hence a [`GuestPath`] here where
    /// [`from_surface`](Self::from_surface), for prefixes matched on
    /// *this* computer, takes an `AsRef<Path>`.
    #[must_use]
    pub fn from_guest(path: &GuestPath) -> Self {
        debug_assert!(
            path.is_absolute(),
            "a guest prefix must be absolute in the guest's namespace, got {path}"
        );
        Self {
            surface: super::lex::fold_dots_under(path.as_str(), PathRules::Posix),
        }
    }

    /// The surface form as a `Path`, for [`lexical`](Self::lexical).
    pub(super) fn surface_path(&self) -> &Path {
        site!(Silent::SurfaceLift, Path::new(&self.surface))
    }

    /// This spelling as the walk takes one: absolute and folded already, as
    /// every frozen spelling is.  Never the root, which no host walks as one
    /// spelling (Windows has no `/`).
    pub(crate) fn lexical(&self) -> LexicalPath {
        LexicalPath::from_lexed(PathBuf::from(&self.surface))
    }

    /// A position held as a spelling, shown: what a launch's own support
    /// files are, which no author wrote.
    pub(crate) fn of_real(real: &RealPath) -> Self {
        Self {
            surface: shown(real.as_path()),
        }
    }

    /// The surface form, for the OS sandbox renderer and overlap keys.
    pub fn as_str(&self) -> &str {
        &self.surface
    }

    /// The surface form as ral shows it, for grant spellings and refusals.
    pub(crate) fn shown(&self) -> String {
        shown(&self.surface)
    }

    /// Whether this is the universal prefix, [`root`](Self::root).
    pub fn is_root(&self) -> bool {
        super::lex::is_bare_root(&self.surface, PathRules::HOST)
    }

    /// Consume into the owned surface `String`, for the wire and render
    /// forms that flatten the prefix back to bytes.
    pub fn into_string(self) -> String {
        self.surface
    }

    /// True iff this prefix is absolute; `authority::grant::freeze` rejects a
    /// frozen entry that is not.
    pub fn is_absolute(&self) -> bool {
        is_absolute(self.surface_path())
    }
}

impl AsRef<str> for FrozenPath {
    fn as_ref(&self) -> &str {
        &self.surface
    }
}

impl PartialEq<str> for FrozenPath {
    fn eq(&self, other: &str) -> bool {
        self.surface == other
    }
}

impl PartialEq<&str> for FrozenPath {
    fn eq(&self, other: &&str) -> bool {
        self.surface == *other
    }
}

impl PartialEq<String> for FrozenPath {
    fn eq(&self, other: &String) -> bool {
        &self.surface == other
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::{FrozenPath, GuestPath};
    #[cfg(unix)]
    use crate::authority::policy::GrantPath;
    #[cfg(unix)]
    use crate::path::{Depth, Excluded, Subject, Walked as _, utf8};

    /// An exclusion's walk keeps a name that is not Unicode byte-exact, so it
    /// holds that name and not the U+FFFD sibling, nor what the link reached
    /// when it was walked before.
    #[cfg(unix)]
    #[test]
    fn a_live_exclusion_holds_a_target_that_is_not_unicode() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::test_helper::real(tmp.path());
        let link = dir.as_path().join("dlink");
        std::os::unix::fs::symlink(dir.as_path().join("tgt"), &link).unwrap();
        let prefix = FrozenPath::from_surface(utf8(&link).unwrap());
        let before = GrantPath::<Excluded>::walk(prefix.clone()).unwrap();

        let target = dir.as_path().join(OsStr::from_bytes(b"tgt\xFF"));
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let walked = GrantPath::<Excluded>::walk(prefix).unwrap().real;
        let holds = |excluded: &Excluded, path: &std::path::Path| {
            let trace = crate::test_helper::trace(path);
            excluded.matches(Subject::Access(&trace), Depth::Within)
        };

        assert_eq!(walked.followed().real().as_path(), target);
        assert!(holds(&walked, &target.join("child")).is_some());
        let lossy = dir.as_path().join("tgt\u{FFFD}");
        assert!(holds(&walked, &lossy.join("child")).is_none());
        assert!(holds(&before.real, &target.join("child")).is_none());
    }

    /// The universal prefix is one separator alone, read by the rules: `\`
    /// is a root only where it is a separator.
    #[test]
    fn a_bare_root_is_a_lone_separator_under_the_rules() {
        use crate::path::PathRules::{Posix, Windows};
        use crate::path::lex::is_bare_root;
        for (path, posix, windows) in [
            ("/", true, true),
            ("\\", false, true),
            ("", false, false),
            ("//", false, false),
            ("\\\\", false, false),
            ("/x", false, false),
            ("C:\\", false, false),
        ] {
            assert_eq!(is_bare_root(path, Posix), posix, "{path:?} under POSIX");
            assert_eq!(
                is_bare_root(path, Windows),
                windows,
                "{path:?} under Windows"
            );
        }
    }

    /// Under POSIX rules `\` is a relative file name: a home of `\` joined
    /// with `.` folds back to it, and must mint no universal authority.
    #[cfg(unix)]
    #[test]
    fn a_backslash_mints_no_universal_authority_under_posix_rules() {
        let prefix = FrozenPath::from_surface("\\/.");
        assert!(!prefix.is_root(), "{prefix:?}");
        assert_eq!(prefix.as_str(), "\\");
    }

    /// Asserting on the *bytes* is deliberate: a test that instead checked
    /// admission of `/work/letter.docx` would pass on Windows even with
    /// both sides mangled, since the access side folds with the same host
    /// kernel.  Only the spelling crosses to the guest.
    #[test]
    fn a_guest_prefix_is_spelled_the_guests_way_on_every_host() {
        let guest = |path| FrozenPath::from_guest(&GuestPath::from_static(path));
        assert_eq!(guest("/work").as_str(), "/work");
        assert_eq!(guest("/tmp").as_str(), "/tmp");
        assert_eq!(
            guest("/work/./letters/../letters").as_str(),
            "/work/letters",
            "the folding is still done, only in the right namespace"
        );
    }

    /// A name that is not Unicode orders apart from the U+FFFD spelling it
    /// decodes to, and is no command name.
    ///
    /// Linux alone can lay one: APFS refuses a name that is not UTF-8.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_byte_is_not_its_replacement_character() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::test_helper::real(tmp.path());
        let (raw, lossy) = (
            dir.as_path().join(OsStr::from_bytes(b"\xFFtool")),
            "\u{FFFD}tool",
        );
        std::fs::write(&raw, "").unwrap();
        std::fs::write(dir.as_path().join(lossy), "").unwrap();
        let (raw, lossy) = (
            crate::test_helper::real(raw),
            crate::test_helper::real(dir.as_path().join(lossy)),
        );
        assert_ne!(raw, lossy);
        assert_eq!(raw.name(), None);
    }

    /// An unpaired surrogate orders apart from the U+FFFD spelling it
    /// decodes to, and is no command name.
    #[cfg(windows)]
    #[test]
    fn an_unpaired_surrogate_is_not_its_replacement_character() {
        use std::os::windows::ffi::OsStringExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::test_helper::real(tmp.path());
        let wide: Vec<u16> = std::iter::once(0xD800)
            .chain("tool".encode_utf16())
            .collect();
        let (raw, lossy) = (
            dir.as_path().join(std::ffi::OsString::from_wide(&wide)),
            dir.as_path().join("\u{FFFD}tool"),
        );
        std::fs::write(&raw, "").unwrap();
        std::fs::write(&lossy, "").unwrap();
        let (raw, lossy) = (
            crate::test_helper::real(raw),
            crate::test_helper::real(lossy),
        );
        assert_ne!(raw, lossy);
        assert_eq!(raw.name(), None);
    }
}
