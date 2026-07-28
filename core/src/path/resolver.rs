//! Stages 1–3 of `crate::path` composed against one `HOME`/cwd pair.
//!
//! [`Resolver::resolve`] is the sole constructor of a [`ResolvedPath`], and
//! outside `crate::path` the canonicalisers are reachable only through one —
//! so the stage order is enforced by the types, not by convention.

use std::path::{Path, PathBuf};

use super::{ResolvedPath, lex, sigil};

/// The `HOME` and logical cwd one resolution runs against.
pub struct Resolver<'a> {
    pub home: String,
    pub cwd: Option<&'a Path>,
}

impl Resolver<'_> {
    /// A resolver for callers with no shell — the `RAL_PATH` walker, the
    /// plugin loader, exarch's fff index — who already hold an absolute
    /// candidate.  Safe only for those: the empty `home` expands `~/x` to
    /// `/x`, and a `None` cwd sends `lex::resolve_path` to the process cwd.
    pub fn shell_less() -> Self {
        Resolver {
            home: String::new(),
            cwd: None,
        }
    }

    /// Stages 1 and 2: expand `~`/`xdg:`, then anchor and fold against `cwd`.
    /// Pure — no filesystem access.
    pub fn resolve(&self, raw: &str) -> ResolvedPath {
        let expanded = sigil::expand_path_prefix(raw, &self.home);
        ResolvedPath::from_lexed(lex::resolve_path(self.cwd, &expanded))
    }

    /// The whole pipeline: [`Self::resolve`], then canonicalise leniently —
    /// symlinks followed across the existing prefix, unresolved tail
    /// re-appended.
    pub fn check(&self, raw: &str) -> PathBuf {
        self.resolve(raw).canonicalise_lenient()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    // Unix-only: `/h` is not absolute under Windows rules, so the expanded
    // form would anchor to the process cwd instead of standing alone.
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

    /// Stage 2 exists so grant prefix matching only ever sees absolute paths.
    #[test]
    fn resolve_anchors_relative_paths_to_cwd() {
        // A driveless path is not absolute on Windows, so the fixture is
        // per-host and the anchoring is pinned on both.
        let cwd = if cfg!(windows) {
            Path::new(r"C:\work\proj")
        } else {
            Path::new("/work/proj")
        };
        let r = Resolver {
            home: "/h".into(),
            cwd: Some(cwd),
        };
        assert_eq!(r.resolve("src/lib.rs").as_path(), cwd.join("src/lib.rs"));
    }

    /// A child of `/tmp` gives the ancestor walk something that always
    /// exists, so the assertion is about the tail being re-appended.
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

    // Unix-only: Linux XDG defaults, and `/h` is not Windows-absolute.
    #[cfg(unix)]
    #[test]
    fn check_pipeline_handles_xdg_with_unset_env() {
        // Held for the whole scope: the suite mutates process env in
        // parallel, and this test's answer depends on XDG_DATA_HOME.
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
        // `/h` does not exist, so the lenient canonicaliser finds no ancestor
        // to resolve and hands back the lexical form untouched.
        assert_eq!(out, Path::new("/h/.local/share/agda"));
    }

    // Unix-only: Windows reads `/etc/hostname` relative to the current drive.
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
