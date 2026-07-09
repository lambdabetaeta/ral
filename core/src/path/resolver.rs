//! Pipeline orchestration: compose stages 1–3 in one place.
//!
//! `Resolver` bundles the per-call resolution context (`HOME`,
//! cwd, canonicalisation mode) and exposes two entry points:
//!
//!   * [`Resolver::resolve`] — sigil-expand then lexically resolve,
//!     yielding a [`ResolvedPath`].
//!   * [`Resolver::check`]   — resolve, then canonicalise according
//!     to [`CanonMode`].
//!
//! [`Resolver::resolve`] is the *sole* constructor of a
//! [`ResolvedPath`], and a `ResolvedPath` is the only thing a
//! canonicaliser accepts — so canonicalisation cannot run before
//! sigil-expansion-then-lex.  The pipeline ordering is encoded in the
//! type, not in convention.
//!
//! Stack-allocated; constructed afresh per call from a `Context`
//! (see [`Context::resolver`](crate::types::Context::resolver)).
//! Owns its `home` to keep call-site lifetimes simple.

use std::path::{Path, PathBuf};

use super::{ResolvedPath, lex, sigil};

/// How [`Resolver::check`] and [`ResolvedPath::canonicalise`] perform
/// stage 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonMode {
    /// Realpath; on missing components, walk up to an existing
    /// ancestor and re-attach the unresolved tail.  Default for
    /// grant prefixes (which may name not-yet-created targets)
    /// and for grant-side access checks outside a sandboxed
    /// child.
    Lenient,
    /// Skip canonicalisation entirely.  Used inside a sandboxed
    /// child where the OS sandbox is the real gate and
    /// `realpath(3)` may fail spuriously on intermediate
    /// components.  Containment then relies on alias awareness
    /// (`/tmp` ↔ `/private/tmp` on macOS) to bridge the gap.
    LexicalOnly,
}

/// Per-call resolution context: `HOME`, scoped cwd, and
/// canonicalisation mode.  See module doc for the pipeline.
pub struct Resolver<'a> {
    pub home: String,
    pub cwd: Option<&'a Path>,
    pub mode: CanonMode,
}

impl Resolver<'_> {
    /// Stage 1 + 2: expand `~` / `xdg:` sigils, then lexically
    /// resolve against `cwd`, minting a [`ResolvedPath`].  Pure: no
    /// filesystem access.  The sole constructor of a `ResolvedPath`.
    pub fn resolve(&self, raw: &str) -> ResolvedPath {
        let expanded = sigil::expand_path_prefix(raw, &self.home);
        ResolvedPath::from_lexed(lex::resolve_path(self.cwd, &expanded))
    }

    /// Stage 1 + 2 + 3: full pipeline.  Touches the filesystem
    /// only in [`CanonMode::Lenient`]; in [`CanonMode::LexicalOnly`]
    /// it is identical to the [`ResolvedPath`] [`Resolver::resolve`]
    /// returns.
    pub fn check(&self, raw: &str) -> PathBuf {
        self.resolve(raw).canonicalise(self.mode)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    /// `resolve` composes stage 1 (sigil) and stage 2 (cwd-anchor +
    /// `.`/`..`).  A tilde-headed path against a fresh `home`
    /// expands and stays absolute; `cwd` is irrelevant once the
    /// expanded form is absolute.
    // Unix-only: `PathBuf::join` produces backslashes on Windows, and the
    // synthetic `/h` home is a Unix shape with no Windows analogue.  The
    // grant subsystem this serves is Unix-only.
    #[cfg(unix)]
    #[test]
    fn resolve_expands_tilde_and_normalises() {
        let r = Resolver {
            home: "/h".into(),
            cwd: None,
            mode: CanonMode::LexicalOnly,
        };
        assert_eq!(
            r.resolve("~/foo/./bar/../baz").as_path(),
            Path::new("/h/foo/baz")
        );
    }

    /// A non-sigil relative path joins to `cwd` and normalises.
    /// The whole point of stage 2 is that grant prefix matching
    /// always sees absolute paths.
    #[test]
    fn resolve_anchors_relative_paths_to_cwd() {
        let cwd = Path::new("/work/proj");
        let r = Resolver {
            home: "/h".into(),
            cwd: Some(cwd),
            mode: CanonMode::LexicalOnly,
        };
        assert_eq!(
            r.resolve("src/lib.rs").as_path(),
            Path::new("/work/proj/src/lib.rs")
        );
    }

    /// `check` in `Lenient` mode walks up to an existing ancestor
    /// when the full path is missing.  We use a child of `/tmp`
    /// (an ancestor that always exists) to assert the suffix is
    /// re-appended after the lenient canonicalisation.
    #[test]
    fn check_lenient_resolves_partial_paths_against_existing_ancestor() {
        let r = Resolver {
            home: "/h".into(),
            cwd: None,
            mode: CanonMode::Lenient,
        };
        let suffix = format!("ral-resolver-probe-{}/leaf", std::process::id());
        let probe = format!("/tmp/{suffix}");
        let out = r.check(&probe);
        assert!(
            out.ends_with(&suffix),
            "expected suffix {suffix:?} in {out:?}"
        );
    }

    /// `check` in `LexicalOnly` mode stops at stage 2.  This is
    /// the in-sandbox path: never touches the filesystem, never
    /// canonicalises.  Result must equal what `resolve` returned.
    #[test]
    fn check_lexical_only_is_identical_to_resolve() {
        let r_lenient = Resolver {
            home: "/h".into(),
            cwd: None,
            mode: CanonMode::Lenient,
        };
        let r_lex_only = Resolver {
            home: "/h".into(),
            cwd: None,
            mode: CanonMode::LexicalOnly,
        };
        // Use a path that doesn't exist so canonicalise_lenient
        // walks up to `/` — its output and `resolve`'s output diverge
        // only when an ancestor is a symlink (e.g. /tmp on macOS).
        // For lexical-only mode, the input shape is preserved.
        let p = "/no/such/path/at/all";
        assert_eq!(r_lex_only.check(p), r_lex_only.resolve(p).into_inner());
        // And both modes agree on the lexical part.
        assert_eq!(
            r_lex_only.resolve(p).into_inner(),
            r_lenient.resolve(p).into_inner()
        );
    }

    /// End-to-end: `xdg:` token through every stage when the env
    /// var is unset (so the Linux default `~/.local/share` kicks
    /// in) and the path doesn't exist on disk.  `check` returns
    /// the lenient canonicalisation, which for a non-existent
    /// path under HOME ends in the expected suffix.
    // Unix-only: Linux XDG default, Unix path shapes throughout.
    #[cfg(unix)]
    #[test]
    fn check_pipeline_handles_xdg_with_unset_env() {
        // Snapshot+restore XDG_DATA_HOME under the shared env guard so the
        // test doesn't race other env-mutating tests under parallel runs.
        let _guard = crate::test_env::env_guard();
        let prev = std::env::var_os("XDG_DATA_HOME");
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        let r = Resolver {
            home: "/h".into(),
            cwd: None,
            mode: CanonMode::LexicalOnly,
        };
        let out = r.check("xdg:data/agda");
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        // Default xdg:data is ${XDG_DATA_HOME:-~/.local/share}.
        // With the var unset and home=/h, expansion gives
        // /h/.local/share, suffix /agda is appended.
        assert_eq!(out, Path::new("/h/.local/share/agda"));
    }

    /// Plain absolute paths pass through every stage unchanged
    /// (in `LexicalOnly` mode).  No sigil to expand, already
    /// absolute, no `.`/`..` to collapse.
    // Unix-only: `/etc/hostname` is a Unix-style absolute that Windows
    // reinterprets relative to the current drive (→ `C:\etc\hostname`).
    #[cfg(unix)]
    #[test]
    fn ordinary_absolute_path_is_a_fixed_point() {
        let r = Resolver {
            home: "/h".into(),
            cwd: None,
            mode: CanonMode::LexicalOnly,
        };
        assert_eq!(r.check("/etc/hostname"), Path::new("/etc/hostname"));
    }
}
