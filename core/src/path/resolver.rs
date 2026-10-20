//! Stages 1–3 of `crate::path` composed against one `HOME`/cwd pair.
//!
//! [`Resolver::resolve`] and [`Resolver::anchor`] are the sole constructors
//! of a [`LexicalPath`], and
//! outside `crate::path` the canonicalisers are reachable only through one —
//! so the stage order is enforced by the types, not by convention.

use std::path::{Path, PathBuf};

use super::{LexicalPath, lex, sigil};

/// The `HOME` and logical cwd one resolution runs against; `home` is `None`
/// where nothing binds one, and a `~` then resolves to itself.
///
/// Field-for-field [`FreezeCtx`](crate::guard::freeze::FreezeCtx) bar the one that matters: a freeze
/// resolves `cwd:` once and for good, so its cwd is total, where a runtime
/// resolution may defer to the process cwd.
pub struct Resolver<'a> {
    pub home: Option<String>,
    pub cwd: Option<&'a Path>,
}

impl<'a> Resolver<'a> {
    /// A resolver for callers with no shell — the `RAL_PATH` walker, the
    /// plugin loader, exarch's skill scan — who already hold an absolute
    /// candidate.  Safe only for those: with no `home`, `~/x` resolves to
    /// itself, and a `None` cwd sends `lex::resolve_path` to the process cwd.
    pub fn shell_less() -> Self {
        Resolver {
            home: None,
            cwd: None,
        }
    }

    /// A resolver against `home` and the host-held `cwd`, for a caller
    /// outside the crate with no shell to mint one.
    pub fn at(home: Option<String>, cwd: &'a Path) -> Self {
        Resolver {
            home,
            cwd: Some(cwd),
        }
    }

    /// Whether `raw` resolves against `cwd`: no sigil head, and relative.
    pub fn is_relative(&self, raw: &str) -> bool {
        !lex::is_absolute(sigil::expand_path_prefix(raw, self.home.as_deref()))
    }

    /// Stages 1 and 2: expand `~`/`xdg:`, then anchor and fold against `cwd`.
    /// Pure — no filesystem access.
    pub fn resolve(&self, raw: &str) -> LexicalPath {
        let expanded = sigil::expand_path_prefix(raw, self.home.as_deref());
        LexicalPath::from_lexed(lex::resolve_path(self.cwd, &expanded))
    }

    /// Stage 2 alone, for a path the host already holds, such as a walked
    /// entry: no sigil may reinterpret it, so a file named `~` is that file,
    /// and no string round trip may rename it.
    pub fn anchor(&self, path: &Path) -> LexicalPath {
        LexicalPath::from_lexed(lex::anchor(self.cwd, path))
    }

    /// [`Self::resolve`] for a path written in a ral script file in `dir`,
    /// which reads it beside itself: `raw` resolved as [`Self::resolve`]
    /// resolves it, with `dir` for the cwd.
    ///
    /// Sigils come first: `raw` is ral source, so a `~`/`xdg:` head expands
    /// as in any written path, and only a still-relative result joins `dir`;
    /// an absolute one, a malformed head included, is kept as `resolve`
    /// keeps it, never joined onto `dir`, which could lend it a drive.  The
    /// directory is host-held, so it is anchored as [`Self::anchor`] takes
    /// one — never read for sigils, a script under a directory named `~`
    /// staying there.
    pub(crate) fn resolve_in_script(&self, raw: &str, dir: &Path) -> LexicalPath {
        let dir = self.anchor(dir);
        let beside = Resolver {
            home: self.home.clone(),
            cwd: Some(dir.as_path()),
        };
        beside.resolve(raw)
    }

    /// The whole pipeline: [`Self::resolve`], then canonicalise leniently —
    /// symlinks followed across the existing prefix, unresolved tail
    /// re-appended.
    pub fn check(&self, raw: &str) -> PathBuf {
        self.resolve(raw).canonicalise_lenient()
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures build literal paths and save and restore `XDG_DATA_HOME`"
)]
mod tests {
    use super::*;

    // Unix-only: `/h` is not absolute under Windows rules, so the expanded
    // form would anchor to the process cwd instead of standing alone.
    #[cfg(unix)]
    #[test]
    fn resolve_expands_tilde_and_normalises() {
        let r = Resolver {
            home: Some("/h".into()),
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
            home: Some("/h".into()),
            cwd: Some(cwd),
        };
        assert_eq!(r.resolve("src/lib.rs").as_path(), cwd.join("src/lib.rs"));
    }

    /// A script's path resolves as an ordinary one with the script's
    /// directory for the cwd, row by row: a relative path joins it, and an
    /// absolute one, a malformed or mixed head included, is kept as
    /// `resolve` keeps it rather than lent the directory's drive.
    #[test]
    fn a_script_path_resolves_as_an_ordinary_one_beside_the_script() {
        let dir = if cfg!(windows) {
            Path::new(r"C:\work\proj")
        } else {
            Path::new("/work/proj")
        };
        let r = Resolver {
            home: Some("/h".into()),
            cwd: None,
        };
        let beside = Resolver {
            home: Some("/h".into()),
            cwd: Some(dir),
        };
        for raw in [
            "lib.ral",
            "../x/./y",
            "/abs/x",
            r"\x\y",
            "C:x",
            r"C:\x\..\y",
            r"\\x",
            "//x/../y",
            r"\\\srv\sh",
            r"/\x\..\y",
            r"\/srv/sh",
            r"\\?\x",
            r"\\.\pipe\x",
            r"\\srv\sh\x",
            "~/x",
        ] {
            let in_script = r.resolve_in_script(raw, dir);
            assert_eq!(
                in_script.as_path(),
                beside.resolve(raw).as_path(),
                "{raw:?}"
            );
            if lex::is_absolute(raw) {
                assert_eq!(
                    in_script.as_path(),
                    lex::fold_dots(Path::new(raw)),
                    "{raw:?}"
                );
            }
        }
    }

    /// A child of `/tmp` gives the ancestor walk something that always
    /// exists, so the assertion is about the tail being re-appended.
    #[test]
    fn check_resolves_partial_paths_against_existing_ancestor() {
        let r = Resolver {
            home: Some("/h".into()),
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
            home: Some("/h".into()),
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
            home: Some("/h".into()),
            cwd: None,
        };
        assert_eq!(
            r.resolve("/etc/hostname").as_path(),
            Path::new("/etc/hostname")
        );
    }
}
