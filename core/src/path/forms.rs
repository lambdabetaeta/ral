//! The two normal-form path types the grant pipeline trusts:
//! [`LexicalPath`] on the access side, [`FrozenPath`] on the grant
//! side.
//!
//! A prefix carries its symlink-followed `real` form on the value, so
//! the meet of two policies is a total pure function of them: the disk is
//! consulted once, at freeze.  Enforcement still re-resolves
//! against the live filesystem — the algebra speaks about the policy,
//! enforcement about the world.
//!
//! Both types have private fields, and every door runs the one
//! `.`/`..`-folding kernel [`fold_dots`](super::lex::fold_dots), so an
//! access-side path and a grant-side prefix compare like-for-like under
//! [`path_within`](super::identity::path_within).  There is no `From<&str>`
//! sugar: a `From` impl cannot consult the disk oracle `real` needs,
//! so it would be a door for fabricating one.
//!
//! A prefix's two forms are not a normal form and a spelling of it: fs and
//! exec alike are judged over *objects*, on `real`; `surface` is the
//! author's spelling, kept for display and for the order of a set.  Hence
//! the containment doors, [`contains`](FrozenPath::contains) and
//! [`real_path`](FrozenPath::real_path) for fs and
//! [`RealPath::frozen`](super::RealPath::frozen) for exec, and no other:
//! `surface` leaves the type only as a `String`, for rendering.

use super::identity::{Polarity, path_within};
use super::lex::{is_absolute, is_working_directory};
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

/// A lexically-resolved path: absolute, `.`/`..`-collapsed, anchored
/// against the logical cwd.
///
/// Collapsed means a fixed point of [`fold_dots`](super::lex::fold_dots):
/// no `.` or `..` past the root it opens with, and on Windows one
/// separator throughout, `\`, but in a head whose kind or meaning turns on
/// its spelling (a verbatim `//?/` head, say), which is kept as written.  A
/// malformed or mixed head
/// ([`HeadKind::Malformed`](super::lex::HeadKind::Malformed)) is all head, so
/// such a path is carried as written, `..` and all, for the kernel to read: a
/// fold across it could assemble a share or a drive it never named, and the
/// walk refuses what `std::path` cannot parse
/// ([`is_unparsed_head`](super::lex::is_unparsed_head)).
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
    /// a [`CwdPath`], whose root each reading keeps, so the join is absolute;
    /// the fold keeps the root it reads and is its own fixed point; and an
    /// absolute path, folded, still has its root, so it is not empty.  A
    /// check here would have no refusal to give a caller.
    pub(super) fn from_lexed(path: PathBuf) -> Self {
        debug_assert!(
            is_absolute(&path),
            "LexicalPath must be absolute, got {}",
            shown(&path)
        );
        debug_assert!(
            super::lex::fold_dots(&path) == path,
            "LexicalPath must be `.`/`..`-collapsed, got {}",
            shown(&path)
        );
        debug_assert!(
            !path.as_os_str().is_empty(),
            "LexicalPath must be non-empty",
        );
        Self(path)
    }

    /// A borrow, for the disk operation the check authorised.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume into the owned `PathBuf` a caller opens or stats.
    pub fn into_inner(self) -> PathBuf {
        self.0
    }

    /// This path as a working directory, for `cd`, `within [dir: …]`, or a
    /// script's own directory.
    ///
    /// # Errors
    /// [`CwdPath::new`]'s: a lexical path may be on a malformed or mixed
    /// head, which no working directory is.
    pub fn into_cwd(self) -> Result<CwdPath, NotACwd> {
        CwdPath::new(self.0)
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
        #[allow(
            clippy::disallowed_methods,
            reason = "native: the host-spelled name the device tables match"
        )]
        let name = self.0.to_string_lossy();
        super::device::is_discard_device(&name, PathRules::HOST)
    }

    /// The refusal owed this path if, on Windows, its last component is a DOS
    /// reserved device name ([`super::device::reserved_device_refusal`]); `None`
    /// everywhere else.  The fs doors ask it of every name they are about to
    /// act on, as they ask [`is_discard`](Self::is_discard).
    pub(crate) fn reserved_device_refusal(&self) -> Option<String> {
        #[allow(
            clippy::disallowed_methods,
            reason = "native: the host-spelled name the device tables match"
        )]
        let name = self.0.to_string_lossy();
        super::device::reserved_device_refusal(&name, PathRules::HOST)
    }

    /// Strict `realpath(3)`.  The input is already absolute and folded, so
    /// nothing here can anchor against the *process* cwd.
    ///
    /// # Errors
    /// Returns `Err` if the path or any intermediate component does not
    /// exist, or on any other `realpath(3)` failure (a non-directory in the
    /// prefix, a permission or symlink-loop error).
    pub fn canonicalise_strict(&self) -> std::io::Result<PathBuf> {
        super::canon::canonicalise_strict(&self.0)
    }

    /// Lenient canonicalisation: resolve the longest existing prefix and
    /// re-append the unresolved tail.  Infallible.
    pub fn canonicalise_lenient(&self) -> PathBuf {
        super::canon::canonicalise_lenient(&self.0)
    }
}

/// A frozen grant prefix: `surface` as the author wrote it, in the normal
/// form a [`LexicalPath`] also carries.
///
/// That form is absolute, `.`/`..`-collapsed, and on Windows spelled with
/// `\` wherever its head allows (`C:/a/b` is frozen as `C:\a\b`).
///
/// `real` is that same path with symlinks followed.
///
/// Field order is load-bearing: the derived `Ord` sorts by `surface`
/// first, so a `BTreeSet` dedups two spellings of one directory by the
/// string the author wrote.  Deduping on `real` instead would fold
/// two distinct-looking grants into one and change the rendered OS rule
/// list.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FrozenPath {
    surface: String,
    real: String,
}

/// A path that holds no faithful `String` form, with the lossy one.
struct Unicode {
    lossy: FrozenPath,
    surface: PathBuf,
    real: PathBuf,
}

impl FrozenPath {
    /// Mint a prefix from a surface form: fold `path`, follow its symlinks,
    /// and wrap both forms.  The one disk consultation this type ever makes,
    /// done here at the door so authorised form and matched form are one
    /// normal form, and the one minting door, whose grant-side caller is the
    /// freeze pass in `guard::freeze`.  Idempotent on a form already normal,
    /// such as the OS-sandbox renderer emits.
    pub fn from_surface(path: impl AsRef<Path>) -> Self {
        match Self::mint(path.as_ref()) {
            Ok(frozen) | Err(Unicode { lossy: frozen, .. }) => frozen,
        }
    }

    /// [`from_surface`](Self::from_surface) for a grant, an identity that is
    /// kept for good: `Err` when the surface or the real path it follows is not
    /// valid Unicode, since the lossy `String` form would then name another
    /// object, a sibling holding U+FFFD, in its place.
    ///
    /// # Errors
    /// The refusal to hold `path` as a grant, saying why.
    pub fn freeze(path: impl AsRef<Path>) -> Result<Self, String> {
        Self::mint(path.as_ref()).map_err(|Unicode { surface, real, .. }| {
            format!(
                "ral cannot hold '{}' as a grant: it resolves to '{}', which is not valid \
                 Unicode; grant a path that is, or one that does not pass through it",
                shown(&surface),
                shown(&real)
            )
        })
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "native: the one door from a native path to the stored string forms; a form that is not UTF-8 is refused by `freeze`"
    )]
    fn mint(path: &Path) -> Result<Self, Unicode> {
        let folded = super::lex::fold_dots(path);
        // The root is the one path `realpath` must not be asked about.  It
        // answers with a *drive* on Windows — the process's own — so a
        // ceiling frozen here covered one volume and denied every other, and
        // the universal prefix stopped being universal the moment a session
        // ran from a drive its data was not on.  Folded to zero components it
        // matches everything, which is what the internal ceiling
        // [`Self::root`] means (an authored `absolute /` is a POSIX grant and
        // dead on Windows).
        if super::lex::is_bare_root(&folded, PathRules::HOST) {
            return Ok(Self {
                surface: "/".into(),
                real: "/".into(),
            });
        }
        let real = super::canon::canonicalise_lenient(&folded);
        let lossy = Self {
            surface: folded.to_string_lossy().into_owned(),
            real: real.to_string_lossy().into_owned(),
        };
        if folded.to_str().is_some() && real.to_str().is_some() {
            Ok(lossy)
        } else {
            Err(Unicode {
                lossy,
                surface: folded,
                real,
            })
        }
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
            real: folded,
        }
    }

    /// The surface form as a `Path` — the author's spelling.
    ///
    /// Private, and staying so: authority is judged on the real form
    /// ([`real_path`](Self::real_path)), so the choice of form is
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

    /// The symlink-followed form as a `Path`, for containment matching
    /// against an access path that has itself been canonicalised.
    #[allow(
        clippy::disallowed_methods,
        reason = "lexical Path::new over a real form already in normal form: no I/O behind it"
    )]
    pub(crate) fn real_path(&self) -> &Path {
        Path::new(&self.real)
    }

    /// The symlink-followed form, for messages.
    pub(crate) fn real(&self) -> &str {
        &self.real
    }

    /// Whether `path` lies within this prefix as a rule of polarity `P`
    /// reads names.
    pub(crate) fn contains<P: Polarity>(&self, path: &Path) -> bool {
        path_within(path, self.real_path(), P::IDENTITY)
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
        #[allow(
            clippy::disallowed_methods,
            reason = "native: the stored real form, re-resolved"
        )]
        let real = self.live_real().to_string_lossy().into_owned();
        Self {
            surface: self.surface.clone(),
            real,
        }
    }

    /// Whether this is the universal prefix, [`root`](Self::root).
    pub fn is_root(&self) -> bool {
        super::lex::is_bare_root(&self.surface, PathRules::HOST)
    }

    /// Whether this prefix holds every path: it is the root, or a spelling
    /// whose frozen real form is.
    pub fn is_universal(&self) -> bool {
        self.contains::<super::Allow>(Self::root().real_path())
    }

    /// The real form of this prefix's surface against the live disk.
    fn live_real(&self) -> PathBuf {
        self.off_root().map_or_else(
            || self.real_path().to_path_buf(),
            |p| super::canon::canonicalise_lenient(p.surface_path()),
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
            real: real.to_string(),
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
    use super::{FrozenPath, GuestPath};

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
                root.contains::<crate::path::Allow>(p.real_path()),
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
        let prefix = FrozenPath::from_surface(std::path::PathBuf::from("\\").join("."));
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
}
