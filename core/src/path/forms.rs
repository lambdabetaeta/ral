//! The normal-form path types the grant pipeline trusts: [`LexicalPath`] on
//! the access side, [`FrozenPath`] on the grant side, and [`RealPath`], the
//! symlink-followed form a prefix carries and exec authority judges.
//!
//! A prefix carries its symlink-followed `real` form on the value, so
//! the meet of two policies is a total pure function of them: the disk is
//! consulted once, at freeze.  Enforcement still re-resolves
//! against the live filesystem — the algebra speaks about the policy,
//! enforcement about the world.
//!
//! The grant types have private fields.  A prefix is a logical spelling, folded
//! and then resolved; an access is resolved as the kernel resolves it,
//! unfolded; the two meet on their real forms under
//! [`FrozenPath::contains`].  There is no `From<&str>`
//! sugar: a `From` impl cannot consult the disk oracle `real` needs,
//! so it would be a door for fabricating one.
//!
//! A prefix's two forms are not a normal form and a spelling of it: fs and
//! exec alike are judged over *objects*, on `real`; `surface` is the
//! author's spelling, kept for display and for the order of a set.  Hence
//! the containment door, [`contains`](FrozenPath::contains), and
//! [`real`](FrozenPath::real), the one door from a prefix to its real form,
//! and no other: `surface` leaves the type only as a `String`, for rendering.

use super::lex::{is_absolute, is_working_directory, utf8};
use super::walk::RealPath;
use super::{GuestPath, PathRules};
use crate::path::shown;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A working directory as ral holds one: a path a relative one is read
/// against.
///
/// Absolute under the host's rules and, on Windows, on a drive, a share, or
/// the verbatim spelling of either
/// ([`is_working_directory`](super::lex::is_working_directory)), so whatever
/// [`anchor`](super::lex::anchor) reads against it, a relative, a
/// root-relative or a drive-relative path, lands on that root and comes out
/// absolute.
///
/// Checked at its door, [`new`](Self::new), which a copy read off the wire
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
/// Reified so that canonicalisation can only follow resolution — there is
/// no way to `realpath` a path that has not first been anchored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalPath(PathBuf);

impl LexicalPath {
    /// Wrap the output of [`super::lex::anchor`], asserting exactly what
    /// that kernel guarantees — no further normalisation happens here.  The
    /// public doors are [`super::Resolver::resolve`] and
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
    /// The one question two doors ask about such a write: the in-process
    /// guard (`Shell::check_fs_read`), which calls it no *access*, and the
    /// observation fan-out (`Shell::observe_stamped`), which calls
    /// it no *mutation* — nothing changed in the world, so nothing is
    /// reported.
    pub(crate) fn is_discard(&self) -> bool {
        super::device::is_discard_device(&self.folded_name(), PathRules::HOST)
    }

    /// The refusal owed this path if, on Windows, its last component is a DOS
    /// reserved device name ([`super::device::reserved_device_refusal`]); `None`
    /// everywhere else.  The fs doors ask it of every name they are about to
    /// act on, as they ask [`is_discard`](Self::is_discard).
    pub(crate) fn reserved_device_refusal(&self) -> Option<String> {
        super::device::reserved_device_refusal(&self.folded_name(), PathRules::HOST)
    }

    /// The folded name as the device tables match it: a device is a name.
    #[allow(
        clippy::disallowed_methods,
        reason = "native: the host-spelled name the device tables match"
    )]
    fn folded_name(&self) -> String {
        super::lex::fold_dots(&self.0)
            .to_string_lossy()
            .into_owned()
    }

    /// Strict `realpath(3)`.  The input is already absolute, so nothing here
    /// can anchor against the *process* cwd.
    ///
    /// # Errors
    /// Returns `Err` if the path or any intermediate component does not
    /// exist, or on any other `realpath(3)` failure (a non-directory in the
    /// prefix, a permission or symlink-loop error).
    pub fn canonicalise_strict(&self) -> std::io::Result<PathBuf> {
        super::canon::canonicalise_strict(&self.0)
    }

    /// Lenient canonicalisation: the physical walk, its missing tail
    /// appended lexically.  Infallible.
    pub fn canonicalise_lenient(&self) -> PathBuf {
        super::canon::canonicalise_lenient(&self.0)
    }
}

/// A frozen grant prefix: `surface` as the author wrote it, in the normal
/// form [`LexicalPath::folded`] gives.
///
/// That form is absolute, `.`/`..`-collapsed, and on Windows spelled with
/// `\` wherever its head allows (`C:/a/b` is frozen as `C:\a\b`).
///
/// `real` is that path with symlinks followed, kept native, never decoded to
/// a `String`; `surface` is UTF-8, being authored text.
///
/// Field order is load-bearing: the derived `Ord` sorts by `surface`
/// first, so a `BTreeSet` dedups two spellings of one directory by the
/// string the author wrote.  Deduping on `real` instead would fold
/// two distinct-looking grants into one and change the rendered OS rule
/// list.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FrozenPath {
    surface: String,
    real: RealPath,
}

impl FrozenPath {
    /// Mint a prefix from a surface form: fold it, then follow its symlinks,
    /// so `cwd link/../x` names `<cwd>/x`, a prefix being a logical spelling.
    ///
    /// The type's one disk consultation and its one minting door, the freeze
    /// pass's in `guard::freeze`.  Idempotent on a normal form, such as the
    /// OS-sandbox renderer emits.
    pub fn from_surface(surface: &str) -> Self {
        let folded = super::lex::fold_dots_under(surface, PathRules::HOST);
        // The root is the one path `realpath` must not be asked about.  It
        // answers with a *drive* on Windows — the process's own — so a
        // ceiling frozen here covered one volume and denied every other, and
        // the universal prefix stopped being universal the moment a session
        // ran from a drive its data was not on.  Folded to zero components it
        // matches everything, which is what the internal ceiling
        // [`Self::root`] means (an authored `absolute /` is a POSIX grant and
        // dead on Windows).
        if super::lex::is_bare_root(&folded, PathRules::HOST) {
            return Self {
                surface: "/".into(),
                real: RealPath::from_folded("/"),
            };
        }
        #[allow(
            clippy::disallowed_methods,
            reason = "lexical Path::new over a folded surface: no I/O behind it"
        )]
        let real = RealPath::lenient(Path::new(&folded));
        Self {
            surface: folded,
            real,
        }
    }

    /// [`from_surface`](Self::from_surface) for a grant, an identity that is
    /// kept for good: `Err` when the surface or the real path it follows is not
    /// valid Unicode, since a grant is serialized as text.
    ///
    /// # Errors
    /// The refusal to hold `path` as a grant, saying why.
    pub fn freeze(path: &Path) -> Result<Self, String> {
        let refusal = |real: &Path| {
            format!(
                "ral cannot hold '{}' as a grant: it resolves to '{}', which is not valid \
                 Unicode; grant a path that is, or one that does not pass through it",
                shown(path),
                shown(real),
            )
        };
        let frozen = Self::from_surface(utf8(path).map_err(|_| refusal(path))?);
        utf8(frozen.real.as_path()).map_err(|_| refusal(frozen.real.as_path()))?;
        Ok(frozen)
    }

    /// The filesystem root, as the prefix that covers every path: the
    /// implicit ceiling a policy attenuates down from.
    ///
    /// Minted rather than frozen, because freezing would *narrow* it.
    /// `realpath("/")` is drive-relative on Windows: it answers `D:\` when
    /// the process cwd sits on `D:` — so a ceiling built through
    /// [`from_surface`](Self::from_surface) covers one drive and silently
    /// denies every other, and a session whose checkout and whose `%TEMP%`
    /// are on different drives (GitHub's Windows runners are exactly that)
    /// loses the whole of the second.  Left unfrozen, `/` folds to zero
    /// components under
    /// [`starts_with_identity`](super::identity::starts_with_identity) and is the
    /// universal prefix on either platform, which is what a ceiling means.
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
    /// *this* computer, takes an `AsRef<Path>`.  There is no `realpath(3)`
    /// on this host for another machine's path, so `real` is `surface`
    /// again.
    #[must_use]
    pub fn from_guest(path: &GuestPath) -> Self {
        debug_assert!(
            path.is_absolute(),
            "a guest prefix must be absolute in the guest's namespace, got {path}"
        );
        let folded = super::lex::fold_dots_under(path.as_str(), PathRules::Posix);
        Self {
            surface: folded.clone(),
            real: RealPath::from_folded(folded),
        }
    }

    /// The surface form as a `Path` — the author's spelling.
    ///
    /// Private, and staying so: authority is judged on the real form
    /// ([`real`](Self::real)), so the choice of form is
    /// made here — never by a caller holding a `&Path`.  The `xdg:` freeze
    /// guard made that choice for itself once, and read a symlink out of
    /// `HOME` as contained.
    #[allow(
        clippy::disallowed_methods,
        reason = "lexical Path::new over a surface already in normal form: no I/O behind it"
    )]
    pub(super) fn surface_path(&self) -> &Path {
        Path::new(&self.surface)
    }

    /// The surface form, for the OS sandbox renderer and overlap keys.
    pub fn as_str(&self) -> &str {
        &self.surface
    }

    /// The surface form as ral shows it, for grant spellings and refusals.
    pub(crate) fn shown(&self) -> String {
        shown(&self.surface)
    }

    /// The symlink-followed form: the one door from a prefix to its real
    /// form, for containment matching against an access path that has itself
    /// been resolved.
    pub(crate) fn real(&self) -> &RealPath {
        &self.real
    }

    /// `self` unless it is the root, which is never re-resolved:
    /// canonicalising anchors a driveless path to a cwd, which on Windows
    /// would narrow the universal root to one drive.
    fn off_root(&self) -> Option<&Self> {
        (!self.is_root()).then_some(self)
    }

    /// This prefix with its real form re-resolved against the live disk, for
    /// an exclusion, which names whatever its spelling reaches now.  The
    /// surface is kept: it is absolute and folded, as the freeze guarantees.
    pub(crate) fn live(&self) -> Self {
        debug_assert!(self.is_root() || self.is_absolute(), "{:?}", self.surface);
        Self {
            surface: self.surface.clone(),
            real: self.live_real(),
        }
    }

    /// Whether this is the universal prefix, [`root`](Self::root).
    pub fn is_root(&self) -> bool {
        super::lex::is_bare_root(&self.surface, PathRules::HOST)
    }

    /// Whether this prefix holds every path: it is the root, or a spelling
    /// whose frozen real form is.
    pub fn is_universal(&self) -> bool {
        self.contains::<super::Allow>(Self::root().real.as_path())
    }

    /// The real form of this prefix's surface against the live disk.
    pub(crate) fn live_real(&self) -> RealPath {
        self.off_root().map_or_else(
            || self.real.clone(),
            |p| RealPath::lenient(p.surface_path()),
        )
    }

    /// Consume into the owned surface `String`, for the wire and render
    /// forms that flatten the prefix back to bytes.
    pub fn into_string(self) -> String {
        self.surface
    }

    /// True iff this prefix is absolute; `guard::freeze` rejects a
    /// frozen entry that is not.
    pub fn is_absolute(&self) -> bool {
        is_absolute(self.surface_path())
    }

    /// Mint a divergent `surface`/`real` pair — the shape a real
    /// symlink freezes to, without a disk.  `#[cfg(test)]` so it can never
    /// become a production door for fabricating a real form.
    #[cfg(test)]
    pub(crate) fn for_test(surface: &str, real: &str) -> Self {
        Self {
            surface: surface.to_string(),
            real: RealPath::assumed(real),
        }
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
mod tests {
    use super::{FrozenPath, GuestPath, RealPath};
    #[cfg(unix)]
    use crate::path::utf8;

    /// An exclusion's live real form keeps a name that is not Unicode
    /// byte-exact, so it holds that name and not the U+FFFD sibling.
    #[cfg(unix)]
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "[test] a real link is retargeted at a name that is not UTF-8"
    )]
    fn a_live_exclusion_holds_a_target_that_is_not_unicode() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = RealPath::of_path(tmp.path()).unwrap();
        let link = dir.as_path().join("dlink");
        std::os::unix::fs::symlink(dir.as_path().join("tgt"), &link).unwrap();
        let prefix = FrozenPath::from_surface(utf8(&link).unwrap());

        let target = dir.as_path().join(OsStr::from_bytes(b"tgt\xFF"));
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let live = prefix.live();

        assert_eq!(live.real().as_path(), target);
        assert!(live.contains::<crate::path::Deny>(&target.join("child")));
        let lossy = dir.as_path().join("tgt\u{FFFD}");
        assert!(!live.contains::<crate::path::Deny>(&lossy.join("child")));
        assert!(!prefix.contains::<crate::path::Deny>(&target.join("child")));
    }

    /// The ceiling covers every path, on whatever drive.
    ///
    /// Windows-only because it is the only host where "the root" is not one
    /// place: `realpath("/")` answers the *current* drive there, so a ceiling
    /// frozen through `from_surface` covered `D:\` alone whenever the process
    /// ran from `D:`, and denied a `%TEMP%` on `C:` — the shape GitHub's
    /// Windows runners have.  Spelled with drives rather than with the live
    /// root, so it pins the claim on a host that has only one.
    #[cfg(windows)]
    #[test]
    fn the_root_ceiling_covers_paths_on_every_drive() {
        let root = FrozenPath::root();
        for path in [
            r"C:\Users\someone\AppData\Local\Temp\x",
            r"D:\a\repo\y",
            r"Z:\z",
        ] {
            let p = FrozenPath::from_surface(path);
            assert!(
                root.contains::<crate::path::Allow>(p.real().as_path()),
                "the ceiling must cover {path}"
            );
        }
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
        assert!(!prefix.is_universal(), "{prefix:?}");
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
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_byte_is_not_its_replacement_character() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let raw = RealPath::assumed(OsStr::from_bytes(b"/opt/\xFFtool"));
        assert_ne!(raw, RealPath::assumed("/opt/\u{FFFD}tool"));
        assert_eq!(raw.name(), None);
    }

    /// An unpaired surrogate orders apart from the U+FFFD spelling it
    /// decodes to, and is no command name.
    #[cfg(windows)]
    #[test]
    fn an_unpaired_surrogate_is_not_its_replacement_character() {
        use std::os::windows::ffi::OsStringExt;
        let wide: Vec<u16> = r"C:\opt\"
            .encode_utf16()
            .chain([0xD800])
            .chain("tool".encode_utf16())
            .collect();
        let raw = RealPath::assumed(std::ffi::OsString::from_wide(&wide));
        assert_ne!(raw, RealPath::assumed("C:\\opt\\\u{FFFD}tool"));
        assert_eq!(raw.name(), None);
        assert_eq!(RealPath::assumed(r"C:\opt\tool").name(), Some("tool"));
    }
}
