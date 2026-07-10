//! Pipeline orchestration: compose stages 1–3 in one place.
//!
//! `Resolver` bundles the per-call resolution context (`HOME`, cwd) and
//! exposes two entry points:
//!
//!   * [`Resolver::resolve`] — sigil-expand then lexically resolve,
//!     yielding a [`ResolvedPath`].
//!   * [`Resolver::check`]   — resolve, then leniently canonicalise.
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

/// Per-call resolution context: `HOME` and scoped cwd.  See module doc
/// for the pipeline.
pub struct Resolver<'a> {
    pub home: String,
    pub cwd: Option<&'a Path>,
}

impl Resolver<'_> {
    /// A resolver with no shell context: empty `HOME`, no scoped cwd.
    /// For shell-less callers (the `RAL_PATH` walker, the plugin loader,
    /// the exarch file-picker) that hold an already-absolute candidate
    /// and only need the sigil/lex/canon kernel, not a `Context`.
    pub fn shell_less() -> Self {
        Resolver {
            home: String::new(),
            cwd: None,
        }
    }

    /// Stage 1 + 2: expand `~` / `xdg:` sigils, then lexically
    /// resolve against `cwd`, minting a [`ResolvedPath`].  Pure: no
    /// filesystem access.  The sole constructor of a `ResolvedPath`.
    pub fn resolve(&self, raw: &str) -> ResolvedPath {
        let expanded = sigil::expand_path_prefix(raw, &self.home);
        ResolvedPath::from_lexed(lex::resolve_path(self.cwd, &expanded))
    }

    /// Stage 1 + 2 + 3: full pipeline.  Resolves, then leniently
    /// canonicalises — following symlinks across the existing prefix and
    /// re-appending the unresolved tail.
    pub fn check(&self, raw: &str) -> PathBuf {
        self.resolve(raw).canonicalise_lenient()
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
        };
        assert_eq!(
            r.resolve("src/lib.rs").as_path(),
            Path::new("/work/proj/src/lib.rs")
        );
    }

    /// `check` walks up to an existing ancestor when the full path is
    /// missing.  We use a child of `/tmp` (an ancestor that always
    /// exists) to assert the suffix is re-appended after the lenient
    /// canonicalisation.
    #[test]
    fn check_resolves_partial_paths_against_existing_ancestor() {
        let r = Resolver {
            home: "/h".into(),
            cwd: None,
        };
        let suffix = format!("ral-resolver-probe-{}/leaf", std::process::id());
        let probe = format!("/tmp/{suffix}");
        let out = r.check(&probe);
        assert!(
            out.ends_with(&suffix),
            "expected suffix {suffix:?} in {out:?}"
        );
    }

    /// End-to-end: `xdg:` token through every stage when the env
    /// var is unset (so the Linux default `~/.local/share` kicks
    /// in) and the path doesn't exist on disk.  `check` returns
    /// the lenient canonicalisation, which for a non-existent
    /// path under a non-existent HOME falls back to the lexical form.
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
        };
        let out = r.check("xdg:data/agda");
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        // Default xdg:data is ${XDG_DATA_HOME:-~/.local/share}.
        // With the var unset and home=/h, expansion gives
        // /h/.local/share, suffix /agda is appended.  /h does not exist,
        // so lenient canonicalisation returns the lexical form.
        assert_eq!(out, Path::new("/h/.local/share/agda"));
    }

    /// Plain absolute paths are a fixed point of the lexical pipeline
    /// (`resolve`): no sigil to expand, already absolute, no `.`/`..`
    /// to collapse.
    // Unix-only: `/etc/hostname` is a Unix-style absolute that Windows
    // reinterprets relative to the current drive (→ `C:\etc\hostname`).
    #[cfg(unix)]
    #[test]
    fn ordinary_absolute_path_is_a_fixed_point() {
        let r = Resolver {
            home: "/h".into(),
            cwd: None,
        };
        assert_eq!(
            r.resolve("/etc/hostname").as_path(),
            Path::new("/etc/hostname")
        );
    }
}
