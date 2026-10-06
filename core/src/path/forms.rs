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

use super::PathRules;
use super::identity::{Deny, Polarity, path_within};
use super::resolver::Resolver;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// A lexically-resolved path: absolute, `.`/`..`-collapsed, anchored
/// against the logical cwd.
///
/// Reified so that canonicalisation can only follow resolution — there is
/// no way to `realpath` a path that has not first been anchored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalPath(PathBuf);

impl LexicalPath {
    /// Wrap the output of [`super::lex::resolve_path`], asserting exactly
    /// what that kernel guarantees — no further normalisation happens
    /// here.  The public door is [`super::Resolver::resolve`].
    pub(super) fn from_lexed(path: PathBuf) -> Self {
        debug_assert!(
            path.is_absolute(),
            "LexicalPath must be absolute, got {}",
            path.display()
        );
        debug_assert!(
            !path
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir)),
            "LexicalPath must be `.`/`..`-collapsed, got {}",
            path.display()
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

    /// For audit fields and denial messages.
    pub(crate) fn display(&self) -> std::path::Display<'_> {
        self.0.display()
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
        super::device::is_discard_device(&self.0.to_string_lossy(), PathRules::HOST)
    }

    /// The refusal owed this path if, on Windows, its last component is a DOS
    /// reserved device name ([`super::device::reserved_device_refusal`]); `None`
    /// everywhere else.  The fs doors ask it of every name they are about to
    /// act on, as they ask [`is_discard`](Self::is_discard).
    pub(crate) fn reserved_device_refusal(&self) -> Option<String> {
        super::device::reserved_device_refusal(&self.0.to_string_lossy(), PathRules::HOST)
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

/// A frozen grant prefix: `surface` as the author wrote it (absolute and
/// `.`/`..`-collapsed, the normal form a [`LexicalPath`] also carries).
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

impl FrozenPath {
    /// Mint a prefix from a surface form: fold `path`, follow its symlinks,
    /// and wrap both forms.  The one disk consultation this type ever makes,
    /// done here at the door so authorised form and matched form are one
    /// normal form, and the one minting door, whose grant-side caller is the
    /// freeze pass in `guard::freeze`.  Idempotent on a form already normal,
    /// such as the OS-sandbox renderer emits.
    pub fn from_surface(path: impl AsRef<Path>) -> Self {
        let folded = super::lex::fold_dots(path.as_ref());
        // The root is the one path `realpath` must not be asked about.  It
        // answers with a *drive* on Windows — the process's own — so a
        // ceiling frozen here covered one volume and denied every other,
        // and the universal prefix stopped being universal the moment a
        // session ran from a drive its data was not on.  Folded to zero
        // components it matches everything, which is what naming the root
        // in a grant has always meant.
        if super::lex::is_bare_root(&folded.to_string_lossy()) {
            return Self {
                surface: "/".into(),
                real: "/".into(),
            };
        }
        let real = super::canon::canonicalise_lenient(&folded);
        Self {
            surface: folded.to_string_lossy().into_owned(),
            real: real.to_string_lossy().into_owned(),
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
    /// normaliser that must agree with the access side is Linux's
    /// ([`fold_dots_posix`](super::lex::fold_dots_posix)), not this
    /// process's — hence a `&str` here where
    /// [`from_surface`](Self::from_surface), for prefixes matched on
    /// *this* computer, takes an `AsRef<Path>`.  There is no `realpath(3)`
    /// on this host for another machine's path, so `real` is `surface`
    /// again.
    #[must_use]
    pub fn from_guest(path: &str) -> Self {
        debug_assert!(
            path.starts_with('/'),
            "a guest prefix must be absolute in the guest's namespace, got {path}"
        );
        let folded = super::lex::fold_dots_posix(path);
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
    fn surface_path(&self) -> &Path {
        Path::new(&self.surface)
    }

    /// The surface form, for the OS sandbox renderer and overlap keys.
    pub fn as_str(&self) -> &str {
        &self.surface
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

    /// Depth in components of the alias-folded real form, as
    /// [`RealPath::depth`](super::RealPath::depth) counts it.
    pub(crate) fn depth(&self) -> usize {
        super::identity::identity_depth(&self.real, PathRules::HOST)
    }

    /// This prefix frozen afresh from its surface spelling against
    /// `resolver`: unchanged but for a re-resolution of its symlinks.
    pub(crate) fn refreeze(&self, resolver: &Resolver) -> Self {
        // `Resolver::resolve` anchors a driveless path to a cwd, which on
        // Windows would narrow the universal root to one drive.
        if super::lex::is_bare_root(&self.surface) {
            return Self::root();
        }
        Self::from_surface(resolver.resolve(&self.surface).as_path())
    }

    /// True iff the in-process exec guard would let this deny decide everything the allow
    /// `other` covers, so composition may drop the allow: the two real
    /// forms contain each other under the deny's identity.
    /// Mutual containment is one rank, where the guard's tie denies.
    ///
    /// Not byte equality: containment folds macOS firmlink aliases (`/tmp` ↔
    /// `/private/tmp`) and every spelling some filesystem takes for a name,
    /// so the derived `Eq`/`Ord` cannot answer this.
    pub(crate) fn evicts(&self, other: &Self) -> bool {
        self.contains::<Deny>(other.real_path()) && other.contains::<Deny>(self.real_path())
    }

    /// Consume into the owned surface `String`, for the wire and render
    /// forms that flatten the prefix back to bytes.
    pub fn into_string(self) -> String {
        self.surface
    }

    /// True iff this prefix is absolute; `guard::freeze` rejects a
    /// frozen entry that is not.
    pub fn is_absolute(&self) -> bool {
        self.surface_path().is_absolute()
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
    use super::FrozenPath;

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

    /// Asserting on the *bytes* is deliberate: a test that instead checked
    /// admission of `/work/letter.docx` would pass on Windows even with
    /// both sides mangled, since the access side folds with the same host
    /// kernel.  Only the spelling crosses to the guest.
    #[test]
    fn a_guest_prefix_is_spelled_the_guests_way_on_every_host() {
        assert_eq!(FrozenPath::from_guest("/work").as_str(), "/work");
        assert_eq!(FrozenPath::from_guest("/tmp").as_str(), "/tmp");
        assert_eq!(
            FrozenPath::from_guest("/work/./letters/../letters").as_str(),
            "/work/letters",
            "the folding is still done, only in the right namespace"
        );
    }
}
