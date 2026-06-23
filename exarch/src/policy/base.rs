//! Built-in capability bases for exarch sessions.
//!
//! Five bake-ins are embedded from `.ral` capability scripts in
//! `exarch/data/`, ordered loosely from no-attenuation down to tightest:
//!
//! - `dangerous`  — `Capabilities::root()`.  Lattice top; no attenuation.
//! - `reasonable` — everyday tooling + standard binary dirs (default).
//! - `read-only`  — `reasonable` reads/exec, but writes only to scratch.
//! - `minimal`    — coreutils + cwd + /tmp + tempdir + net + chdir.
//!   Small base for additive `--extend-base` composition.
//! - `confined`   — build-jail shape (after BrianSwift's `confined.sb`):
//!   tight reads/writes, no network, exec by subpath only.
//!
//! `minimal`, `confined`, `read-only`, and `reasonable` use `cwd:` and
//! `tempdir:` sigils in their `fs` and `exec` entries; the freeze pass
//! inside [`ral_core::capability::decode_capability_map`] resolves them
//! at session start, so the per-invocation working directory is baked
//! into the policy without exarch having to inject it dynamically.
//!
//! Each profile is a ral script whose terminal expression is a map
//! shaped like the argument of `grant [...] { body }`.  Loading goes
//! through [`ral_core::capability::load_capabilities_from_str`] —
//! the same surface a `--capabilities <path>.ral` flag at the ral CLI
//! consumes.  Two surfaces, one model.

use ral_core::types::{Capabilities, FsPolicy, Shell};

const MINIMAL_RAL: &str = include_str!("../../data/minimal.exarch.ral");
const REASONABLE_RAL: &str = include_str!("../../data/reasonable.exarch.ral");
const READ_ONLY_RAL: &str = include_str!("../../data/read-only.exarch.ral");
const CONFINED_RAL: &str = include_str!("../../data/confined.exarch.ral");
const DANGEROUS_RAL: &str = include_str!("../../data/dangerous.exarch.ral");
#[cfg(test)]
const GIT_EXTENSION_RAL: &str = include_str!("../../examples/git.exarch.ral");

/// Resolve `name` to a frozen [`Capabilities`], resolving every sigil
/// against `ctx`.  The orchestrator (`policy::for_invocation`) then joins
/// an extend-base and meets restrict files — all on resolved bundles.
pub(super) fn resolve_base(
    name: &str,
    ctx: &ral_core::path::sigil::FreezeCtx<'_>,
) -> Result<Capabilities, String> {
    let text = match name {
        "minimal" => MINIMAL_RAL,
        "reasonable" => REASONABLE_RAL,
        "read-only" => READ_ONLY_RAL,
        "confined" => CONFINED_RAL,
        "dangerous" => DANGEROUS_RAL,
        other => {
            return Err(format!(
                "exarch: unknown base '{other}'; \
                 expected one of: minimal, reasonable, read-only, confined, dangerous"
            ));
        }
    };
    let mut shell = Shell::new(Default::default());
    let virtual_path = format!("<built-in:{name}>");
    ral_core::capability::load_capabilities_from_str(&mut shell, text, &virtual_path, ctx).map_err(
        |e| match e {
            ral_core::types::Break::Error(err) => format!(
                "exarch: built-in base '{name}' failed to parse: {}",
                err.message
            ),
            other => format!("exarch: built-in base '{name}' failed: {other:?}"),
        },
    )
}

/// Preserve otherwise-unrestricted filesystem authority while still
/// carving out `deny_paths` for active restriction files.  The `/`
/// ceiling is a grant-side prefix, so it is minted in the same normal
/// form every other prefix carries.
pub(super) fn root_fs_policy() -> FsPolicy {
    let root = || ral_core::path::NormalizedPrefix::from_surface("/");
    FsPolicy {
        read_prefixes: vec![root()],
        write_prefixes: vec![root()],
        deny_paths: Vec::new(),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use ral_core::path::sigil::{FreezeCtx, expand_path_prefix};
    use ral_core::types::{Capabilities, ExecPolicy, Shell};
    use std::path::Path;

    /// Load and freeze a bake-in against `ctx`.  Freezing happens inside
    /// the loader now, so this both parses the script and resolves every
    /// sigil — a failure here is a malformed profile, an unknown `xdg:`
    /// token, or an xdg-escape violation.
    fn load(name: &str, text: &str, ctx: &FreezeCtx<'_>) -> Capabilities {
        let mut shell = Shell::new(Default::default());
        ral_core::capability::load_capabilities_from_str(
            &mut shell,
            text,
            &format!("<test-base:{name}>"),
            ctx,
        )
        .unwrap_or_else(|e| panic!("base '{name}' failed to load: {e:?}"))
    }

    /// Every bake-in must parse, validate, and freeze against the real
    /// `$HOME`.  Catches malformed ral source, unknown `xdg:` tokens, and
    /// xdg-escape violations at `cargo test` time rather than at first
    /// user invocation.
    #[cfg(unix)]
    #[test]
    fn bakeins_parse() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        for (name, text) in [
            ("minimal", MINIMAL_RAL),
            ("reasonable", REASONABLE_RAL),
            ("read-only", READ_ONLY_RAL),
            ("confined", CONFINED_RAL),
            ("dangerous", DANGEROUS_RAL),
        ] {
            load(name, text, &ctx);
        }
    }

    /// `confined` is the build-jail profile: net off, no user-home
    /// reads, exec by subpath only.  These three properties are the
    /// load-bearing differences vs `reasonable`; pin them so a future
    /// edit doesn't accidentally widen the build jail.  `confined` names
    /// no `~`/`xdg:` path, so a synthetic home is enough to freeze it,
    /// and no resolved prefix may fall under that home.
    #[cfg(unix)]
    #[test]
    fn confined_is_offline_subpath_only_no_home_reads() {
        let ctx = FreezeCtx {
            home: "/h",
            cwd: Path::new("/work/proj"),
        };
        let caps = load("confined", CONFINED_RAL, &ctx);
        assert_eq!(caps.net, Some(false), "confined must have net off");
        let exec = caps.exec.as_ref().expect("confined declares exec");
        // No bare-name admits — every admit is a directory prefix.
        assert!(
            exec.literals.is_empty(),
            "confined exec has literal keys; build jail uses directory prefixes only"
        );
        let fs = caps.fs.as_ref().expect("confined declares fs");
        for prefix in fs.read_prefixes.iter().chain(fs.write_prefixes.iter()) {
            assert!(
                !prefix.as_str().starts_with("/h"),
                "confined fs prefix '{}' reaches into home — build jail must not",
                prefix.as_str()
            );
        }
    }

    /// `read-only` differs from `reasonable` only in that writes
    /// don't include the working tree.  Fold a future regression
    /// where someone re-adds `cwd:` to write_prefixes.  `cwd:` freezes
    /// to the synthetic working dir, independent of the environment.
    #[cfg(unix)]
    #[test]
    fn read_only_does_not_write_cwd() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/work/proj"),
        };
        let caps = load("read-only", READ_ONLY_RAL, &ctx);
        let fs = caps.fs.as_ref().expect("read-only declares fs");
        assert!(
            !fs.write_prefixes.iter().any(|p| p == "/work/proj"),
            "read-only must not write the working tree"
        );
        assert!(
            fs.read_prefixes.iter().any(|p| p == "/work/proj"),
            "read-only must read the working tree"
        );
    }

    #[test]
    fn dangerous_is_root() {
        let ctx = FreezeCtx {
            home: "/h",
            cwd: Path::new("/"),
        };
        assert_eq!(
            load("dangerous", DANGEROUS_RAL, &ctx),
            Capabilities::default()
        );
    }

    /// Reasonable's `exec` includes the `xdg:bin` directory key, which
    /// freezes to `${XDG_BIN_HOME:-~/.local/bin}`.  Assert the resolved
    /// prefix lands in the `dirs` half (where directory keys live).
    #[cfg(unix)]
    #[test]
    fn reasonable_carries_xdg_bin_subpath_in_exec() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let caps = load("reasonable", REASONABLE_RAL, &ctx);
        let exec = caps.exec.as_ref().expect("reasonable should declare exec");
        let xdg_bin = expand_path_prefix("xdg:bin", &home);
        assert!(
            exec.dirs.contains_key(&xdg_bin),
            "reasonable should list the resolved xdg:bin ({xdg_bin}) in exec dirs"
        );
    }

    /// `cwd:` and `tempdir:` directory keys land in `exec` and matching
    /// plain sigils land in `fs` for both `minimal` and `reasonable`, so a
    /// per-invocation working tree is admitted without exarch injecting it
    /// dynamically.  After freeze the keys are the resolved working dir
    /// and platform temp dir.
    #[cfg(unix)]
    #[test]
    fn minimal_and_reasonable_carry_cwd_and_tempdir_sigils() {
        let home = ral_core::path::home_from_env();
        let cwd = Path::new("/work/proj");
        let ctx = FreezeCtx { home: &home, cwd };
        // Freeze folds away any trailing separator the platform temp
        // dir carries (macOS `$TMPDIR` ends in `/`), so compare against
        // the same normal form the frozen keys hold.
        let normal = |p: &str| ral_core::path::NormalizedPrefix::from_surface(p).into_string();
        let cwd_resolved = normal(&cwd.to_string_lossy());
        let tempdir_resolved = normal(&std::env::temp_dir().to_string_lossy());
        for (name, text) in [("minimal", MINIMAL_RAL), ("reasonable", REASONABLE_RAL)] {
            let caps = load(name, text, &ctx);
            let exec = caps
                .exec
                .as_ref()
                .unwrap_or_else(|| panic!("{name} should declare exec"));
            assert!(
                exec.dirs.contains_key(&cwd_resolved),
                "{name} exec missing resolved cwd"
            );
            assert!(
                exec.dirs.contains_key(&tempdir_resolved),
                "{name} exec missing resolved tempdir"
            );
            let fs = caps
                .fs
                .as_ref()
                .unwrap_or_else(|| panic!("{name} should declare fs"));
            for token in [&cwd_resolved, &tempdir_resolved] {
                assert!(
                    fs.read_prefixes.iter().any(|p| p == token),
                    "{name} fs.read_prefixes missing {token}"
                );
                assert!(
                    fs.write_prefixes.iter().any(|p| p == token),
                    "{name} fs.write_prefixes missing {token}"
                );
            }
        }
    }

    /// End-to-end: a path-style exec under cwd is admitted after
    /// freeze rewrites `cwd:` into the project's working directory.
    ///
    /// Unix-only: the test inputs `/work/proj` and `/work/proj/configure`
    /// are Unix-shaped absolute paths, and the bake-in policy's exec
    /// admittance keys (`/bin/`, `/usr/bin/`, …) are Unix paths.  The
    /// Windows path-comparison machinery is exercised by integration
    /// tests under `cargo test --test windows_*`.
    #[cfg(unix)]
    #[test]
    fn freeze_admits_relative_exec_under_cwd_sigil() {
        let work = std::path::Path::new("/work/proj");
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: work,
        };
        let caps = load("minimal", MINIMAL_RAL, &ctx);

        let mut shell = Shell::default();
        shell
            .with_cwd(work.to_path_buf(), |sh| {
                sh.with_capabilities(caps, |sh| {
                    sh.check_exec_args(
                        "./configure",
                        &["./configure", "/work/proj/configure"],
                        &[],
                    )
                })
            })
            .expect("./configure under cwd: must be admitted");
    }

    /// Regression: a command at /opt/homebrew/bin/cmake — invoked
    /// by short name OR full absolute path — must be admitted by
    /// reasonable's `/opt/homebrew/bin/` subpath key in `exec`
    /// even though cmake itself is not a per-name entry.
    ///
    /// Unix-only: the path under test (`/opt/homebrew/bin/cmake`) has
    /// no Windows analogue in the bake-in policy.
    #[cfg(unix)]
    #[test]
    fn reasonable_admits_cmake_under_opt_homebrew_bin() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let caps = load("reasonable", REASONABLE_RAL, &ctx);
        assert!(
            caps.exec
                .as_ref()
                .is_some_and(|m| m.dirs.contains_key("/opt/homebrew/bin")),
            "reasonable should list /opt/homebrew/bin in exec dirs"
        );

        let mut shell = Shell::default();
        shell
            .with_capabilities(caps.clone(), |sh| {
                sh.check_exec_args("cmake", &["cmake", "/opt/homebrew/bin/cmake"], &[])
            })
            .expect("short-name cmake under /opt/homebrew/bin must be admitted");

        let mut shell2 = Shell::default();
        shell2
            .with_capabilities(caps, |sh| {
                sh.check_exec_args("/opt/homebrew/bin/cmake", &["/opt/homebrew/bin/cmake"], &[])
            })
            .expect("full-path cmake under /opt/homebrew/bin must be admitted");
    }

    /// `cargo` invoked through rustup resolves to a binary under
    /// `~/.rustup/toolchains/`.  Regression: this path was absent from
    /// the exec map and was denied by the reasonable profile.
    #[cfg(unix)]
    #[test]
    fn reasonable_admits_cargo_under_rustup_toolchain() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let caps = load("reasonable", REASONABLE_RAL, &ctx);
        let cargo = format!(
            "{}/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo",
            home
        );
        let mut shell = Shell::default();
        shell
            .with_capabilities(caps, |sh| {
                sh.check_exec_args("cargo", &["cargo", &cargo], &[])
            })
            .expect("rustup toolchain cargo must be admitted");
    }

    /// `go` at `/usr/local/go/bin/go` (official installer) and user
    /// tools at `~/go/bin/` must be admitted.
    #[cfg(unix)]
    #[test]
    fn reasonable_admits_go_official_and_user_tools() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let caps = load("reasonable", REASONABLE_RAL, &ctx);
        let gobin = format!("{}/go/bin/goimports", home);
        for (name, abs) in [
            ("go", "/usr/local/go/bin/go"),
            ("goimports", gobin.as_str()),
        ] {
            let mut shell = Shell::default();
            shell
                .with_capabilities(caps.clone(), |sh| {
                    sh.check_exec_args(name, &[name, abs], &[])
                })
                .unwrap_or_else(|_| panic!("{name} at {abs} must be admitted"));
        }
    }

    /// `node` resolved by nvm to `~/.nvm/versions/node/<v>/bin/node`
    /// must be admitted.
    #[cfg(unix)]
    #[test]
    fn reasonable_admits_nvm_node() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let caps = load("reasonable", REASONABLE_RAL, &ctx);
        let node = format!("{}/.nvm/versions/node/v22.0.0/bin/node", home);
        let mut shell = Shell::default();
        shell
            .with_capabilities(caps, |sh| sh.check_exec_args("node", &["node", &node], &[]))
            .expect("nvm node must be admitted");
    }

    /// `python3` resolved by pyenv to `~/.pyenv/versions/<v>/bin/python3`
    /// must be admitted; same for the pyenv shim layer.
    #[cfg(unix)]
    #[test]
    fn reasonable_admits_pyenv_python() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let caps = load("reasonable", REASONABLE_RAL, &ctx);
        let versioned = format!("{}/.pyenv/versions/3.12.0/bin/python3", home);
        let shim = format!("{}/.pyenv/shims/python3", home);
        for (name, abs) in [("python3", versioned.as_str()), ("python3", shim.as_str())] {
            let mut shell = Shell::default();
            shell
                .with_capabilities(caps.clone(), |sh| {
                    sh.check_exec_args(name, &[name, abs], &[])
                })
                .unwrap_or_else(|_| panic!("{name} at {abs} must be admitted"));
        }
    }

    /// `reasonable` lists the standard system `bin` directories as
    /// subpath keys in `exec`, which would otherwise admit
    /// `/bin/bash` and `/usr/bin/zsh`.  An explicit `'deny'` per-name
    /// entry in the same map is the override knob: literal-match
    /// wins over subpath-match, so the agent cannot reach those
    /// tools through the admitted dirs.  `/bin/sh` remains allowed:
    /// it is build infrastructure, not an interactive shell surface.
    #[cfg(unix)]
    #[test]
    fn reasonable_denies_bash_and_zsh_despite_bin_in_exec_dirs() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let caps = load("reasonable", REASONABLE_RAL, &ctx);

        for (name, abs) in [("bash", "/bin/bash"), ("zsh", "/bin/zsh")] {
            let mut shell = Shell::default();
            let r = shell.with_capabilities(caps.clone(), |sh| {
                sh.check_exec_args(name, &[name, abs], &[])
            });
            assert!(
                r.is_err(),
                "{name} should be denied even though its parent dir is in exec_dirs"
            );
        }

        let mut shell = Shell::default();
        shell
            .with_capabilities(caps, |sh| sh.check_exec_args("sh", &["sh", "/bin/sh"], &[]))
            .expect("sh should remain allowed for build infrastructure");
    }

    /// `git` is admitted in `reasonable` and `read-only` so commit
    /// flows ("commit please") work without `--extend-base`.  Pin
    /// this so a future profile edit doesn't silently re-deny.
    /// Network-git surfaces (push, signed commit) still depend on
    /// credential / signing helpers — those are gated by the fs
    /// grant (`~/.ssh`, `~/.gnupg` unreadable) plus, on macOS,
    /// whatever the user has configured in their keychain.
    #[cfg(unix)]
    #[test]
    fn reasonable_and_read_only_admit_git() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let gitconfig = expand_path_prefix("~/.gitconfig", &home);
        for (name, text) in [("reasonable", REASONABLE_RAL), ("read-only", READ_ONLY_RAL)] {
            let caps = load(name, text, &ctx);
            let exec = caps.exec.as_ref().expect("base declares exec");
            assert_eq!(
                exec.literals.get("git"),
                Some(&ExecPolicy::Allow),
                "{name} should admit git"
            );
            let fs = caps.fs.as_ref().expect("base declares fs");
            assert!(
                fs.read_prefixes.iter().any(|p| p == &gitconfig),
                "{name} should add resolved ~/.gitconfig ({gitconfig}) to read prefixes"
            );
        }
    }

    /// `minimal` admits the *system* git under `/usr/bin/` (its own
    /// header lists git among what the subpath rule allows) but not a
    /// Homebrew git under `/opt/homebrew/bin` — minimal admits no
    /// homebrew tree, so user-installed brew tools stay opt-in.  A
    /// non-root cwd keeps the `cwd:/` allow from masking the test: under
    /// `cwd: /` everything resolves inside the working tree.  The git
    /// extension's `git: 'allow'` is what carries a Homebrew install.
    #[cfg(unix)]
    #[test]
    fn minimal_admits_system_git_not_homebrew() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/work"),
        };
        let admits = |caps: ral_core::types::Capabilities, names: &[&str]| {
            let mut shell = Shell::default();
            shell
                .with_capabilities(caps, |sh| sh.check_exec_args(names[0], names, &[]))
                .is_ok()
        };
        assert!(
            admits(load("minimal", MINIMAL_RAL, &ctx), &["git", "/usr/bin/git"]),
            "minimal admits the system git via the /usr/bin/ subpath"
        );
        assert!(
            !admits(
                load("minimal", MINIMAL_RAL, &ctx),
                &["git", "/opt/homebrew/bin/git"]
            ),
            "minimal does not admit a Homebrew git — brew trees are opt-in"
        );
        let widened =
            load("minimal", MINIMAL_RAL, &ctx).join(load("git-ext", GIT_EXTENSION_RAL, &ctx));
        assert!(
            admits(widened, &["git", "/opt/homebrew/bin/git"]),
            "the git extension's git: 'allow' carries a Homebrew install"
        );
    }

    /// The git extension adds gitconfig *reads* (and a portable `git`
    /// exec grant) to a tight base: `minimal` and `confined` grant no
    /// home reads and admit only the system git under `/usr/bin/`, while
    /// `reasonable` and `read-only` already read `~/.gitconfig`.
    ///
    /// Two facts about the join are pinned here:
    ///
    /// 1. Joining against `minimal` keeps git admitted (the extension's
    ///    one-sided `git: 'allow'` survives) and makes gitconfig
    ///    readable.
    /// 2. The base's gh/op/gcloud credential denies survive the join:
    ///    `FsPolicy::join` unions deny sets, so a veto is preserved even
    ///    though the extension no longer re-states it.  This is the
    ///    sticky-veto property that lets an overlay stay purely additive.
    ///
    /// Both sides freeze against the same home, so the resolved
    /// `xdg:config/*` paths coincide.
    #[cfg(unix)]
    #[test]
    fn git_extension_widens_into_git_capable_profile() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let gitconfig = expand_path_prefix("~/.gitconfig", &home);
        let xdg_config_git = expand_path_prefix("xdg:config/git", &home);

        let widened_minimal =
            load("minimal", MINIMAL_RAL, &ctx).join(load("git-extension", GIT_EXTENSION_RAL, &ctx));
        let exec = widened_minimal
            .exec
            .as_ref()
            .expect("extension should keep exec map");
        assert_eq!(exec.literals.get("git"), Some(&ExecPolicy::Allow));
        let fs = widened_minimal
            .fs
            .as_ref()
            .expect("extension should keep fs map");
        assert!(
            fs.read_prefixes.iter().any(|p| p == &gitconfig),
            "join with minimal should add ~/.gitconfig"
        );
        assert!(
            fs.read_prefixes.iter().any(|p| p == &xdg_config_git),
            "join with minimal should add xdg:config/git"
        );

        let widened_reasonable = load("reasonable", REASONABLE_RAL, &ctx).join(load(
            "git-extension",
            GIT_EXTENSION_RAL,
            &ctx,
        ));
        let fs = widened_reasonable
            .fs
            .as_ref()
            .expect("extension should keep fs map");
        for denied in ["xdg:config/gh", "xdg:config/op", "xdg:config/gcloud"] {
            let resolved = expand_path_prefix(denied, &home);
            assert!(
                fs.deny_paths.iter().any(|p| p == &resolved),
                "join with reasonable should preserve {denied} deny ({resolved})"
            );
        }
    }
}
