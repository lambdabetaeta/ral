//! The two normal-form path types the grant pipeline trusts.
//!
//! [`ResolvedPath`] is the access-side stage-2 output: a path that is
//! absolute, `.`/`..`-collapsed, and anchored against the *logical*
//! cwd.  [`NormalizedPrefix`] is the grant-side counterpart: a frozen
//! prefix string in the same normal form, minted at policy freeze.
//!
//! Both have private fields.  A [`ResolvedPath`] is minted only
//! through [`super::Resolver::resolve`]; the grant-side freeze door
//! is the lexer ([`super::sigil`]).  [`NormalizedPrefix`] also admits
//! [`NormalizedPrefix::from_surface`] (and the `From<&str>` /
//! `From<String>` sugar), which re-fold an already-normal surface form
//! for the prefix-set projection and the OS-sandbox renderer.  Every
//! door — `Resolver::resolve`, `freeze`, `from_surface` — runs the one
//! `.`/`..`-folding kernel ([`super::lex::fold_dots`]), so an
//! access-side path and a grant-side prefix compare like-for-like
//! under [`super::path_within`].
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

    /// Stage 3 under the resolver's [`CanonMode`](super::CanonMode):
    /// lenient canonicalisation outside a sandboxed child, lexical-only
    /// inside it.
    pub fn canonicalise(&self, mode: super::CanonMode) -> PathBuf {
        match mode {
            super::CanonMode::Lenient => self.canonicalise_lenient(),
            super::CanonMode::LexicalOnly => self.0.clone(),
        }
    }
}

/// A frozen grant prefix in the same normal form a [`ResolvedPath`]
/// carries: absolute and `.`/`..`-collapsed.
///
/// Held as a `String`
/// because grant prefixes freeze against a different cwd/home than the
/// access, but minted by the same `fold_dots` kernel so they match
/// like-for-like.  Private field; sole constructor is the freeze lexer.
///
/// `Serialize`/`Deserialize` are transparent — the wire form is the
/// bare inner `String`.  Decoding bypasses the freeze constructor
/// because the IPC boundary (`WireContext` across the re-exec'd child)
/// is trusted: the parent has already frozen every prefix.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NormalizedPrefix(String);

impl NormalizedPrefix {
    /// Fold `path` and wrap it.  The constructor enforces the
    /// `.`/`..`-collapsed invariant the gate would otherwise apply at
    /// match time, performed once at the door so authorised-form and
    /// matched-form are one normal form.  Crate-internal: the grant-side
    /// door is the freeze lexer in [`super::sigil`].
    pub(super) fn freeze(path: &Path) -> Self {
        let folded = super::lex::fold_dots(path);
        Self(folded.to_string_lossy().into_owned())
    }

    /// Mint a prefix from an already-resolved surface form — the output
    /// of [`PrefixSet::surface`](super::PrefixSet::surface) and the bytes
    /// the OS-sandbox renderer emits.  Runs the same `fold_dots` kernel
    /// (idempotent on a surface form, which is already normal), so the
    /// projection's prefixes pass through the identical normaliser the
    /// access-side [`ResolvedPath`] and the grant-side freeze do.
    pub fn from_surface(path: impl AsRef<Path>) -> Self {
        Self::freeze(path.as_ref())
    }

    /// The prefix as a borrow, for containment matching.
    #[allow(clippy::disallowed_methods)]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// The prefix string, for the OS sandbox renderer and overlap keys.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the owned `String`, for the wire/render forms that
    /// flatten the prefix back to bytes.
    pub fn into_string(self) -> String {
        self.0
    }

    /// True iff this prefix is absolute.  The freeze pass asserts this
    /// after minting, surfacing the same error the post-hoc check did.
    pub fn is_absolute(&self) -> bool {
        self.as_path().is_absolute()
    }
}

impl AsRef<str> for NormalizedPrefix {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Sugar for [`NormalizedPrefix::from_surface`] — the OS-projection and
/// wire/render sites that already hold a normal-form surface string.
/// The grant-side door stays the freeze lexer; this never reaches a
/// `decode_*` path.
impl From<&str> for NormalizedPrefix {
    fn from(s: &str) -> Self {
        Self::from_surface(s)
    }
}

impl From<String> for NormalizedPrefix {
    fn from(s: String) -> Self {
        Self::from_surface(s)
    }
}

impl PartialEq<str> for NormalizedPrefix {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for NormalizedPrefix {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for NormalizedPrefix {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}
