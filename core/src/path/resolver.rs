//! Stages 1–3 of `crate::path` composed against one `HOME`/cwd pair.
//!
//! [`Resolver::resolve`] and [`Resolver::anchor`] are the sole constructors
//! of a [`LexicalPath`], and
//! outside `crate::path` the canonicalisers are reachable only through one —
//! so the stage order is enforced by the types, not by convention.

use std::path::{Path, PathBuf};

use super::{CwdPath, LexicalPath, NotACwd, lex, sigil};
use crate::types::EnvVars;

/// The environment and logical cwd one resolution runs against: `~` reads
/// `env.home()`, `xdg:` the `XDG_*_HOME` of `env`, each overlay before the
/// host, and a `~` with no home resolves to itself.
///
/// The pair a [`FreezeCtx`](crate::guard::freeze::FreezeCtx) owns: a freeze
/// resolves `cwd:` against its cwd once and for good, a runtime resolution
/// against this one each time it is asked.  The cwd is a [`CwdPath`], checked
/// where it was minted, so every path read against it has an absolute answer.
pub struct Resolver<'a> {
    env: &'a EnvVars,
    cwd: &'a CwdPath,
}

impl<'a> Resolver<'a> {
    /// A resolver against `env` and `cwd`, for a caller with no shell to
    /// mint one; an empty `env` reads the host.
    pub fn at(env: &'a EnvVars, cwd: &'a CwdPath) -> Self {
        Resolver { env, cwd }
    }

    /// `HOME` as this resolver's `~` reads it.
    pub fn home(&self) -> Option<String> {
        self.env.home()
    }

    /// Whether `raw` resolves against `cwd`: no sigil head, and relative.
    pub fn is_relative(&self, raw: &str) -> bool {
        !lex::is_absolute(sigil::expand_path_prefix(raw, self.env))
    }

    /// Stages 1 and 2: expand `~`/`xdg:`, then anchor against `cwd`.  No
    /// filesystem access, and no fold ([`LexicalPath::folded`]).
    pub fn resolve(&self, raw: &str) -> LexicalPath {
        let expanded = sigil::expand_path_prefix(raw, self.env);
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
    ///
    /// # Errors
    /// [`NotACwd`] when `dir`, anchored, is on a malformed or mixed head,
    /// against which no path can be read.
    pub(crate) fn resolve_in_script(&self, raw: &str, dir: &Path) -> Result<LexicalPath, NotACwd> {
        let dir = self.anchor(dir).into_cwd()?;
        Ok(Resolver::at(self.env, &dir).resolve(raw))
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

    fn cwd(path: &str) -> CwdPath {
        CwdPath::new(PathBuf::from(path)).expect("a fixture cwd is absolute")
    }

    /// A cwd no fixture path is under, so an answer that read it would show.
    fn elsewhere() -> CwdPath {
        cwd(if cfg!(windows) {
            r"Q:\elsewhere"
        } else {
            "/elsewhere"
        })
    }

    // Unix-only: `/h` is not absolute under Windows rules, so the expanded
    // form would anchor to the cwd's drive instead of standing alone.
    #[cfg(unix)]
    #[test]
    fn resolve_expands_tilde_and_normalises() {
        let elsewhere = elsewhere();
        let env = std::iter::once(("HOME", "/h")).collect::<EnvVars>();
        let r = Resolver::at(&env, &elsewhere);
        assert_eq!(
            r.resolve("~/foo/./bar/../baz").folded().as_path(),
            Path::new("/h/foo/baz")
        );
    }

    /// Stage 2 exists so grant prefix matching only ever sees absolute paths.
    #[test]
    fn resolve_anchors_relative_paths_to_cwd() {
        // A driveless path is not absolute on Windows, so the fixture is
        // per-host and the anchoring is pinned on both.
        let cwd = cwd(if cfg!(windows) {
            r"C:\work\proj"
        } else {
            "/work/proj"
        });
        let env = std::iter::once(("HOME", "/h")).collect::<EnvVars>();
        let r = Resolver::at(&env, &cwd);
        assert_eq!(
            r.resolve("src/lib.rs").as_path(),
            cwd.as_path().join("src").join("lib.rs")
        );
    }

    /// A working directory is absolute and, under Windows rules, on a drive
    /// or a share: a malformed or mixed head is none, since a name joined
    /// past it could assemble a share it never named.
    #[test]
    fn a_working_directory_is_absolute_on_a_well_formed_root() {
        for raw in ["x", "./x", ""] {
            assert!(CwdPath::new(PathBuf::from(raw)).is_err(), "{raw:?}");
        }
        let (good, bad): (&[&str], &[&str]) = if cfg!(windows) {
            (
                &[
                    r"C:\x",
                    "C:/",
                    r"\\srv\sh\x",
                    r"\\?\C:\x",
                    r"\\?\UNC\srv\sh",
                ],
                &[
                    r"\x",
                    "/x",
                    "C:x",
                    r"\\srv",
                    r"\\x\..\y",
                    r"/\x",
                    r"\\.\pipe\x",
                ],
            )
        } else {
            (&["/", "/x", "//x"], &[r"C:\x", r"\x"])
        };
        for raw in good {
            assert!(CwdPath::new(PathBuf::from(raw)).is_ok(), "{raw:?}");
        }
        for raw in bad {
            assert!(CwdPath::new(PathBuf::from(raw)).is_err(), "{raw:?}");
        }
    }

    /// Win32's readings of the two Windows spellings relative to less than
    /// a whole path: a root-relative one lands on the cwd's drive or share,
    /// a drive-relative one on the cwd's drive joins the cwd, and one on
    /// another drive is absolute on that drive, wherever Win32 puts it.
    #[cfg(windows)]
    #[test]
    fn windows_reads_root_and_drive_relative_paths_as_win32_does() {
        for (dir, raw, want) in [
            (r"C:\work\proj", r"\x\y", r"C:\x\y"),
            (r"C:\work\proj", "/x", r"C:\x"),
            (r"\\srv\sh\work", r"\x", r"\\srv\sh\x"),
            (r"\\?\C:\work", r"\x", r"\\?\C:\x"),
            (r"C:\work\proj", "C:x", r"C:\work\proj\x"),
            (r"C:\work\proj", r"c:..\x", r"C:\work\x"),
            (r"C:\work\proj", "C:", r"C:\work\proj"),
            (r"\\?\C:\work", "C:x", r"\\?\C:\work\x"),
        ] {
            let dir = cwd(dir);
            let got = Resolver::at(&EnvVars::new(), &dir).resolve(raw).folded();
            assert_eq!(got.as_path(), Path::new(want), "{raw:?} in {dir:?}");
        }
        let dir = cwd(r"\\srv\sh\work");
        for raw in [r"Q:x\y", "Q:"] {
            let got = Resolver::at(&EnvVars::new(), &dir).resolve(raw);
            let text = got.as_path().to_str().expect("a fixture is UTF-8");
            assert!(lex::is_absolute(got.as_path()), "{raw:?} -> {text:?}");
            assert!(
                text.to_ascii_uppercase().starts_with(r"Q:\"),
                "{raw:?} -> {text:?}"
            );
        }
    }

    /// A script's path resolves as an ordinary one with the script's
    /// directory for the cwd, row by row: a relative path joins it, a
    /// drive-relative one on its drive too, and an absolute one, a malformed
    /// or mixed head included, is kept as `resolve` keeps it rather than lent
    /// the directory's drive.
    #[test]
    fn a_script_path_resolves_as_an_ordinary_one_beside_the_script() {
        let dir = if cfg!(windows) {
            Path::new(r"C:\work\proj")
        } else {
            Path::new("/work/proj")
        };
        let (elsewhere, here) = (elsewhere(), cwd(dir.to_str().expect("UTF-8")));
        let env = std::iter::once(("HOME", "/h")).collect::<EnvVars>();
        let r = Resolver::at(&env, &elsewhere);
        let beside = Resolver::at(&env, &here);
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
            let in_script = r
                .resolve_in_script(raw, dir)
                .expect("the directory is a cwd");
            assert_eq!(
                in_script.as_path(),
                beside.resolve(raw).as_path(),
                "{raw:?}"
            );
            if lex::is_absolute(raw) {
                assert_eq!(
                    in_script.folded().as_path(),
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
        let here = crate::host::cwd().expect("the test process has a cwd");
        let env = std::iter::once(("HOME", "/h")).collect::<EnvVars>();
        let r = Resolver::at(&env, &here);
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
        let elsewhere = elsewhere();
        let env = std::iter::once(("HOME", "/h")).collect::<EnvVars>();
        let r = Resolver::at(&env, &elsewhere);
        let out = r.check("xdg:data/agda");
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        // `/h` does not exist, so the lenient canonicaliser finds no ancestor
        // to resolve and hands back the lexical form untouched.
        assert_eq!(out, Path::new("/h/.local/share/agda"));
    }

    // Unix-only: Windows reads `/etc/hostname` relative to the cwd's drive.
    #[cfg(unix)]
    #[test]
    fn ordinary_absolute_path_is_a_fixed_point() {
        let elsewhere = elsewhere();
        let env = std::iter::once(("HOME", "/h")).collect::<EnvVars>();
        let r = Resolver::at(&env, &elsewhere);
        assert_eq!(
            r.resolve("/etc/hostname").as_path(),
            Path::new("/etc/hostname")
        );
    }
}
