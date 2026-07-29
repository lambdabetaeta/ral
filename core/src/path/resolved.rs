//! The two normal-form path types the grant pipeline trusts:
//! [`ResolvedPath`] on the access side, [`NormalizedPrefix`] on the grant
//! side.
//!
//! A prefix carries its symlink-followed `resolved` form on the value, so
//! composition ([`Capabilities::meet`](crate::types::Capabilities::meet)
//! and the lattices under it) is a total pure function of two policies:
//! the disk is consulted once, at freeze.  Enforcement still re-resolves
//! against the live filesystem — composition speaks about the policy,
//! enforcement about the world.
//!
//! Both types have private fields, and every door runs the one
//! `.`/`..`-folding kernel [`fold_dots`](super::lex::fold_dots), so an
//! access-side path and a grant-side prefix compare like-for-like under
//! [`path_within`](super::lex::path_within).  There is no `From<&str>`
//! sugar: a `From` impl cannot consult the disk oracle `resolved` needs,
//! so it would be a door for fabricating one.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// A lexically-resolved path: absolute, `.`/`..`-collapsed, anchored
/// against the logical cwd.
///
/// Reified so that canonicalisation can only follow resolution — there is
/// no way to `realpath` a path that has not first been anchored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPath(PathBuf);

impl ResolvedPath {
    /// Wrap the output of [`super::lex::resolve_path`], asserting exactly
    /// what that kernel guarantees — no further normalisation happens
    /// here.  The public door is [`super::Resolver::resolve`].
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

    /// A borrow, for the disk operation the check authorised.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume into the owned `PathBuf` a caller opens or stats.
    pub fn into_inner(self) -> PathBuf {
        self.0
    }

    /// For audit fields and denial messages.
    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
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

/// Which operating system's namespace a [`NormalizedPrefix`]'s
/// `resolved` form was resolved in.
///
/// [`super::meet_prefixes`] keys overlap on `(namespace, resolved)`, so a
/// prefix minted for one namespace never overlaps one minted for another:
/// the meet is fail-closed across namespaces rather than silently
/// comparing spellings that were never meant to agree.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Namespace {
    /// This process's own filesystem.
    Host,
    /// A Linux guest's filesystem — see [`NormalizedPrefix::from_guest`].
    Guest,
}

/// A frozen grant prefix: `surface` as the author wrote it (absolute and
/// `.`/`..`-collapsed, the normal form a [`ResolvedPath`] also carries).
///
/// `resolved` is that same path with symlinks followed in `namespace`.
///
/// Field order is load-bearing: the derived `Ord` sorts by `surface`
/// first, so a `BTreeSet` dedups two spellings of one directory by the
/// string the author wrote.  Deduping on `resolved` instead would fold
/// two distinct-looking grants into one and change the rendered OS rule
/// list.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NormalizedPrefix {
    surface: String,
    resolved: String,
    namespace: Namespace,
}

impl NormalizedPrefix {
    /// Fold `path`, follow its symlinks, and wrap both forms — the one
    /// disk consultation this type ever makes, done here at the door so
    /// authorised form and matched form are one normal form.  The
    /// grant-side door is the freeze pass in [`super::sigil`].
    pub(super) fn freeze(path: &Path) -> Self {
        let folded = super::lex::fold_dots(path);
        let resolved = super::canon::canonicalise_lenient(&folded);
        Self {
            surface: folded.to_string_lossy().into_owned(),
            resolved: resolved.to_string_lossy().into_owned(),
            namespace: Namespace::Host,
        }
    }

    /// Mint a prefix from an already-normal surface form — what
    /// [`PrefixSet::surface`](super::PrefixSet::surface) yields and what
    /// the OS-sandbox renderer emits.  Same fold-then-resolve kernel,
    /// idempotent on such a form.
    pub fn from_surface(path: impl AsRef<Path>) -> Self {
        Self::freeze(path.as_ref())
    }

    /// Mint a prefix naming a path inside the Linux guest, whichever host
    /// mints it.
    ///
    /// The gate that matches it runs *inside* the machine, so the
    /// normaliser that must agree with the access side is Linux's
    /// ([`fold_dots_posix`](super::lex::fold_dots_posix)), not this
    /// process's — hence a `&str` here where
    /// [`from_surface`](Self::from_surface), for prefixes matched on
    /// *this* computer, takes an `AsRef<Path>`.  There is no `realpath(3)`
    /// on this host for another machine's path, so `resolved` is `surface`
    /// again, tagged [`Namespace::Guest`]: a host-side meet against a
    /// guest prefix is then the empty, fail-closed intersection, leaving
    /// the guest's own kernel as the only thing that can narrow a guest
    /// grant.
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

    /// The surface form as a `Path`, for containment matching.
    #[allow(
        clippy::disallowed_methods,
        reason = "lexical Path::new over a surface already in normal form — no I/O behind it"
    )]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.surface)
    }

    /// The surface form, for the OS sandbox renderer and overlap keys.
    pub fn as_str(&self) -> &str {
        &self.surface
    }

    /// The symlink-followed form, for composition overlap.
    pub fn resolved(&self) -> &str {
        &self.resolved
    }

    /// Which namespace `resolved` was resolved in.
    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    /// True iff the enforcement gate (`capability::exec::longest_dir_match`)
    /// would treat `self` and `other` as one directory: mutual containment
    /// under [`path_within_str`](super::lex::path_within_str), plus a
    /// matching namespace.
    ///
    /// Not byte equality.  That containment rule folds macOS firmlink
    /// aliases (`/tmp` ↔ `/private/tmp`) and, under Windows identity, case,
    /// separator spelling and a `\\?\`-verbatim prefix, so two surfaces the
    /// gate calls one directory can differ byte-for-byte — and two prefixes
    /// frozen against different disk state can differ in `resolved`.  Any
    /// set operation asking "does this clash with something the gate calls
    /// the same dir" needs this, not the derived `Eq`/`Ord`.
    pub fn same_gate_dir(&self, other: &Self) -> bool {
        self.namespace == other.namespace
            && super::lex::path_within_str(&self.surface, &other.surface)
            && super::lex::path_within_str(&other.surface, &self.surface)
    }

    /// Consume into the owned surface `String`, for the wire and render
    /// forms that flatten the prefix back to bytes.
    pub fn into_string(self) -> String {
        self.surface
    }

    /// True iff this prefix is absolute; `capability::decode` rejects a
    /// frozen entry that is not.
    pub fn is_absolute(&self) -> bool {
        self.as_path().is_absolute()
    }

    /// Mint a divergent `surface`/`resolved` pair — the shape a real
    /// symlink freezes to, without a disk.  `#[cfg(test)]` so it can never
    /// become a production door for fabricating a resolved form.
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

    /// Asserting on the *bytes* is deliberate: a test that instead checked
    /// admission of `/work/letter.docx` would pass on Windows even with
    /// both sides mangled, since the access side folds with the same host
    /// kernel.  Only the spelling crosses to the guest.
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
