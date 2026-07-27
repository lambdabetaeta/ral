//! The two normal-form path types the grant pipeline trusts.
//!
//! [`ResolvedPath`] is the access-side stage-2 output: a path that is
//! absolute, `.`/`..`-collapsed, and anchored against the *logical*
//! cwd.  [`NormalizedPrefix`] is the grant-side counterpart: a record
//! minted at policy freeze that carries *both* the `surface` form (as
//! authored, `.`/`..`-folded) and the `resolved` form (symlinks
//! followed) alongside the `namespace` the two forms agree in.
//!
//! Carrying `resolved` on the value, rather than re-deriving it from
//! disk at every comparison, is what lets composition
//! ([`Capabilities::meet`](crate::types::Capabilities::meet), the
//! `ExecMap`/`FsPolicy` lattices) be a total pure function of two
//! policies: minting consults the disk once, at freeze; composing two
//! already-minted prefixes never does.  Enforcement — the gate at the
//! point of use — still re-resolves against the live filesystem, and
//! that is correct: composition is a statement about the policy,
//! enforcement is a statement about the world.
//!
//! Both types have private fields.  A [`ResolvedPath`] is minted only
//! through [`super::Resolver::resolve`]; the grant-side freeze door is
//! the lexer ([`super::sigil`]).  [`NormalizedPrefix`] also admits
//! [`NormalizedPrefix::from_surface`], which re-folds an already-normal
//! surface form for the prefix-set projection and the OS-sandbox
//! renderer.  Every door — `Resolver::resolve`, `freeze`,
//! `from_surface` — runs the one `.`/`..`-folding kernel
//! ([`super::lex::fold_dots`]), so an access-side path and a grant-side
//! prefix compare like-for-like under [`super::path_within`].  There is
//! no `From<&str>`/`From<String>` sugar: a `From` impl cannot consult
//! the oracle a `resolved` field needs, so it would be a door for
//! fabricating one.
//!
//! One door answers to a different operating system:
//! [`NormalizedPrefix::from_guest`], for a prefix this process mints but
//! never matches, because the gate that matches it runs inside a Linux
//! guest.  It runs the same law in the guest's namespace
//! ([`super::lex::fold_dots_posix`]), and — because there is no
//! `realpath(3)` on this host for a path inside another machine —
//! `resolved` is just `surface` again, tagged `Namespace::Guest`.  That
//! tag is what makes composition namespace-correct: overlap is judged
//! on `(namespace, resolved)`, so a host-namespace prefix and a
//! guest-namespace prefix never overlap, and a cross-namespace meet is
//! the empty, fail-closed intersection rather than a host kernel
//! silently folding `/work` to `\work` and matching nothing.
//!
//! Canonicalisation against `realpath(3)` is anchored on this normal
//! form: [`ResolvedPath::canonicalise_strict`] /
//! [`ResolvedPath::canonicalise_lenient`] take an already-resolved
//! path.  The lenient canonicaliser is also reachable on a bare
//! `&Path` through [`super::canon::match_variants`] (the
//! kernel-sandbox variant generator); there the `.`/`..`-defence is
//! `canonicalise_lenient`'s own [`fold_dots`](super::lex::fold_dots)
//! first step, not the `ResolvedPath` type.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// A lexically-resolved path: absolute, `.`/`..`-collapsed, anchored
/// against the logical cwd.
///
/// The stage-2 (`lex`) output of the grant
/// pipeline, reified so that canonicalisation can only follow
/// resolution — there is no way to `realpath` a path that has not
/// first been resolved against the logical cwd.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPath(PathBuf);

impl ResolvedPath {
    /// Wrap the stage-2 output of [`super::lex::resolve_path`].  The
    /// three invariants below are precisely what `resolve_path`
    /// guarantees, so this constructor is that kernel wrapped — no new
    /// normalisation.  Crate-internal: the public door is
    /// [`super::Resolver::resolve`].
    pub(super) fn from_lexed(path: PathBuf) -> Self {
        debug_assert!(
            path.is_absolute(),
            "ResolvedPath must be absolute, got {}",
            path.display()
        );
        debug_assert!(
            !path
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir)),
            "ResolvedPath must be `.`/`..`-collapsed, got {}",
            path.display()
        );
        debug_assert!(
            !path.as_os_str().is_empty(),
            "ResolvedPath must be non-empty",
        );
        Self(path)
    }

    /// The resolved path as a borrow, for the disk operation the check
    /// authorised.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume into the owned `PathBuf` a caller opens or stats.
    pub fn into_inner(self) -> PathBuf {
        self.0
    }

    /// Display the resolved path, for audit fields and denial messages.
    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }

    /// Strict `realpath(3)`: errors when the path or an intermediate
    /// component is missing.  The input is already absolute and folded,
    /// so `realpath` has nothing to anchor against the *process* cwd.
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

/// Which operating system's namespace a [`NormalizedPrefix`]'s
/// `resolved` form was resolved in.
///
/// Composition keys overlap on `(namespace, resolved)`
/// ([`super::meet_prefixes`]), so a prefix minted for one namespace
/// never overlaps one minted for another — the meet is fail-closed
/// across namespaces rather than silently comparing spellings that
/// were never meant to agree.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Namespace {
    /// Resolved by and matched against this process's own filesystem.
    Host,
    /// Resolved by and matched against a Linux guest's filesystem —
    /// see [`NormalizedPrefix::from_guest`].
    Guest,
}

/// A frozen grant prefix, carrying both the form the author wrote and
/// the form the gate matches against.
///
/// `surface` is absolute and `.`/`..`-collapsed, in the same normal
/// form a [`ResolvedPath`] carries.  `resolved` is `surface` with
/// symlinks followed, in `namespace`.  Carrying both — rather than
/// re-deriving `resolved` from disk on every comparison — is what lets
/// the lattice meets be pure: minting is the one place this type
/// touches the filesystem.
///
/// Private fields; the grant-side door is the freeze lexer
/// ([`super::sigil`]), save for a prefix bound for another OS's
/// namespace — see [`NormalizedPrefix::from_guest`].  Field order is
/// load-bearing: the derived `Ord` sorts by `surface` first, so a
/// `BTreeSet` dedups two spellings of one directory by the string the
/// author wrote, not by where it resolves — collapsing on `resolved`
/// instead would fold two distinct-looking grants into one and change
/// the rendered OS rule list.
///
/// `Serialize`/`Deserialize` are the ordinary derived (struct) form —
/// not transparent — since the wire boundary (`WireContext` across the
/// re-exec'd child) is a trusted, same-build hop and the whole record,
/// not just `surface`, must survive it.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NormalizedPrefix {
    surface: String,
    resolved: String,
    namespace: Namespace,
}

impl NormalizedPrefix {
    /// Fold `path`, follow its symlinks, and wrap both forms.  The
    /// constructor enforces the `.`/`..`-collapsed invariant the gate
    /// would otherwise apply at match time and does the one disk
    /// consultation this type ever needs, performed once at the door so
    /// authorised-form and matched-form are one normal form.
    /// Crate-internal: the grant-side door is the freeze lexer in
    /// [`super::sigil`].
    pub(super) fn freeze(path: &Path) -> Self {
        let folded = super::lex::fold_dots(path);
        let resolved = super::canon::canonicalise_lenient(&folded);
        Self {
            surface: folded.to_string_lossy().into_owned(),
            resolved: resolved.to_string_lossy().into_owned(),
            namespace: Namespace::Host,
        }
    }

    /// Mint a prefix from an already-resolved surface form — the output
    /// of [`PrefixSet::surface`](super::PrefixSet::surface) and the bytes
    /// the OS-sandbox renderer emits.  Runs the same fold-then-resolve
    /// kernel (idempotent on a surface form, which is already normal),
    /// so the projection's prefixes pass through the identical
    /// normaliser the access-side [`ResolvedPath`] and the grant-side
    /// freeze do.
    pub fn from_surface(path: impl AsRef<Path>) -> Self {
        Self::freeze(path.as_ref())
    }

    /// Mint a prefix that names a path inside the Linux guest, whichever
    /// host is doing the minting.
    ///
    /// Synod is the case this exists for: the granted folder is admitted
    /// at its guest mount point (`/work`), and the gate that will match
    /// against the prefix runs *inside* the machine.  So the normaliser
    /// that has to agree with the access side is Linux's, not this
    /// process's — see [`fold_dots_posix`](super::lex::fold_dots_posix)
    /// for what the host's own does to `/work` on Windows, and
    /// `MachineSpec::resolve` for the same reasoning applied to
    /// absoluteness (judged with `starts_with('/')`, never
    /// `Path::is_absolute`).
    ///
    /// Not a general-purpose door, and not interchangeable with
    /// [`from_surface`](Self::from_surface): a prefix that will be matched
    /// on *this* computer must go through that one, which is why it takes
    /// an `AsRef<Path>` and this takes a `&str`.  There is no host whose
    /// paths this normalises correctly except by coincidence.
    ///
    /// This namespace has no `realpath(3)` of its own to consult from
    /// this host, so `resolved` is just `surface` again: the door does
    /// no disk I/O because there is no oracle for it to consult, which
    /// is exactly why `namespace` exists — so composition can tell a
    /// guest-namespace prefix apart from a host one instead of silently
    /// comparing spellings that were never meant to agree.
    ///
    /// One constraint travels with a prefix minted here: it must not be
    /// *reduced* on the host.  Synod never narrows a guest grant — its
    /// trunk runs with `fuel: 0`, so no sub-agent composition ever runs
    /// against it — and a nested `grant` block inside the machine is
    /// reduced *there*, by the guest's own kernel, which is the right
    /// one.  A guest-namespace policy that ever does need host-side
    /// narrowing is exactly what the `namespace` tag protects: the meet
    /// keys overlap on `(namespace, resolved)`, so it can never fold a
    /// guest prefix through the host's separator by mistake.
    #[must_use]
    pub fn from_guest(path: &str) -> Self {
        debug_assert!(
            path.starts_with('/'),
            "a guest prefix must be absolute in the guest's namespace, got {path}"
        );
        let folded = super::lex::fold_dots_posix(path);
        Self {
            surface: folded.clone(),
            resolved: folded,
            namespace: Namespace::Guest,
        }
    }

    /// The prefix as a borrow, for containment matching.
    #[allow(clippy::disallowed_methods)]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.surface)
    }

    /// The surface form, for the OS sandbox renderer and overlap keys.
    pub fn as_str(&self) -> &str {
        &self.surface
    }

    /// The resolved (symlink-followed) form, for composition overlap.
    pub fn resolved(&self) -> &str {
        &self.resolved
    }

    /// Which namespace `resolved` was resolved in.
    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    /// Consume into the owned surface `String`, for the wire/render
    /// forms that flatten the prefix back to bytes.
    pub fn into_string(self) -> String {
        self.surface
    }

    /// True iff this prefix is absolute.  The freeze pass asserts this
    /// after minting, surfacing the same error the post-hoc check did.
    pub fn is_absolute(&self) -> bool {
        self.as_path().is_absolute()
    }

    /// Test-only mint with an explicit, possibly divergent
    /// `surface`/`resolved` pair — the shape a real symlink would
    /// produce, without touching a disk to produce it.  Every
    /// production door derives `resolved` from `surface` itself; this
    /// is the one place that invariant is deliberately broken, and it
    /// exists only under `#[cfg(test)]` so it can never become a fifth
    /// production door for fabricating a resolved form.
    #[cfg(test)]
    pub(crate) fn for_test(surface: &str, resolved: &str, namespace: Namespace) -> Self {
        Self {
            surface: surface.to_string(),
            resolved: resolved.to_string(),
            namespace,
        }
    }
}

impl AsRef<str> for NormalizedPrefix {
    fn as_ref(&self) -> &str {
        &self.surface
    }
}

impl PartialEq<str> for NormalizedPrefix {
    fn eq(&self, other: &str) -> bool {
        self.surface == other
    }
}

impl PartialEq<&str> for NormalizedPrefix {
    fn eq(&self, other: &&str) -> bool {
        self.surface == *other
    }
}

impl PartialEq<String> for NormalizedPrefix {
    fn eq(&self, other: &String) -> bool {
        &self.surface == other
    }
}

#[cfg(test)]
mod tests {
    use super::NormalizedPrefix;

    /// A guest prefix survives being minted on this computer, whichever
    /// computer this is.  The assertion is on the *bytes*, deliberately:
    /// a test that instead checked whether the prefix admits
    /// `/work/letter.docx` would pass on Windows even when both sides are
    /// mangled, because the access side folds with the same host kernel.
    /// Only the spelling shows the split, because only the spelling is
    /// what crosses to the guest.
    #[test]
    fn a_guest_prefix_is_spelled_the_guests_way_on_every_host() {
        assert_eq!(NormalizedPrefix::from_guest("/work").as_str(), "/work");
        assert_eq!(NormalizedPrefix::from_guest("/tmp").as_str(), "/tmp");
        assert_eq!(
            NormalizedPrefix::from_guest("/work/./letters/../letters").as_str(),
            "/work/letters",
            "the folding is still done — it is only done in the right namespace"
        );
    }
}
