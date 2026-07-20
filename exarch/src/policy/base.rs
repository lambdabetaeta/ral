//! Built-in capability bases for exarch sessions.
//!
//! Six bake-ins are embedded from `.ral` capability scripts in
//! `exarch/data/`, each a ral script whose terminal expression is a map
//! shaped like the argument of `grant [...] { body }`.  Loading goes
//! through [`ral_core::capability::load_capabilities_from_str`] — the same
//! surface a `--capabilities <path>.ral` flag at the ral CLI consumes.
//! Two surfaces, one model.
//!
//! Ordered loosely from no-attenuation down to tightest:
//!
//! - `dangerous`  — `Capabilities::root()`.  Lattice top; no attenuation.
//! - `reasonable` — everyday tooling + standard binary dirs (default).
//! - `edit-only`  — `reasonable` reads/exec, writes to working tree + scratch.
//! - `read-only`  — `reasonable` reads/exec, but writes only to scratch.
//! - `minimal`    — coreutils + cwd + /tmp + tempdir + net + chdir.
//!   Small base for additive `--extend-base` composition.
//! - `confined`   — build-jail shape (after `BrianSwift`'s `confined.sb`):
//!   tight reads/writes, no network, exec by subpath only.
//!
//! Every profile but `dangerous` names `cwd:` and `tempdir:` sigils in its
//! `fs` and `exec` entries; the freeze pass inside
//! [`ral_core::capability::decode_capability_map`] resolves them at session
//! start, so the per-invocation working directory is baked into the policy
//! without exarch having to inject it dynamically.
//!
//! See `exarch/PROFILES.md` for the per-profile shapes and guidance on
//! when to use each.

use ral_core::io::TerminalState;
use ral_core::types::{Capabilities, FsPolicy, Shell};

const MINIMAL_RAL: &str = include_str!("../../data/minimal.exarch.ral");
const REASONABLE_RAL: &str = include_str!("../../data/reasonable.exarch.ral");
const READ_ONLY_RAL: &str = include_str!("../../data/read-only.exarch.ral");
const EDIT_ONLY_RAL: &str = include_str!("../../data/edit-only.exarch.ral");
const CONFINED_RAL: &str = include_str!("../../data/confined.exarch.ral");
const DANGEROUS_RAL: &str = include_str!("../../data/dangerous.exarch.ral");
// Unix-gated with its only consumers: the extension-join tests below,
// whose fixtures are Unix-shaped (`/usr/bin`, Homebrew trees).
#[cfg(all(test, unix))]
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
        "edit-only" => EDIT_ONLY_RAL,
        "confined" => CONFINED_RAL,
        "dangerous" => DANGEROUS_RAL,
        other => {
            return Err(format!(
                "exarch: unknown base '{other}'; \
                 expected one of: dangerous, reasonable, edit-only, read-only, minimal, confined"
            ));
        }
    };
    let mut shell = Shell::new(TerminalState::default());
    let virtual_path = format!("<built-in:{name}>");
    let mut caps =
        ral_core::capability::load_capabilities_from_str(&mut shell, text, &virtual_path, ctx)
            .map_err(|e| match e {
                ral_core::types::Break::Error(err) => format!(
                    "exarch: built-in base '{name}' failed to parse: {}",
                    err.message
                ),
                other @ ral_core::types::Break::Escape(_) => {
                    format!("exarch: built-in base '{name}' failed: {other:?}")
                }
            })?;
    drop_dead_exec_grants(&mut caps, cfg!(unix));
    Ok(caps)
}

/// Remove exec literal entries naming a bundled coreutils tool that
/// does not exist as a bundled tool on this platform — the
/// `coreutils-unix-only` set (`id`, `kill`, `stat`, `tac`, `test`,
/// `timeout`), whose upstream `uu_*` crates never link on Windows —
/// so a rendered profile never advertises a grant it cannot honour.
///
/// `unix_available` is a parameter rather than a `cfg(unix)` read
/// inside this function so the drop has a unit test that runs on
/// every host: the real call site in [`resolve_base`] passes
/// `cfg!(unix)`, and the test below passes `false` directly to check
/// the Windows shape without a Windows machine.
fn drop_dead_exec_grants(caps: &mut Capabilities, unix_available: bool) {
    if unix_available {
        return;
    }
    if let Some(exec) = caps.exec.as_mut() {
        exec.literals.retain(|name, _| {
            !ral_core::builtins::uutils::COREUTILS_UNIX_ONLY_TOOLS.contains(&name.as_str())
        });
    }
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
    use ral_core::path::sigil::FreezeCtx;
    // Used only by the Unix-fixture tests below; gated so the Windows
    // build of this module stays warning-free.
    #[cfg(unix)]
    use ral_core::path::sigil::expand_path_prefix;
    #[cfg(unix)]
    use ral_core::types::ExecPolicy;
    use ral_core::types::{Capabilities, ExecDir, Shell};
    use std::path::Path;

    /// Load and freeze a bake-in against `ctx`.  Freezing happens inside
    /// the loader now, so this both parses the script and resolves every
    /// sigil — a failure here is a malformed profile, an unknown `xdg:`
    /// token, or an xdg-escape violation.
    fn load(name: &str, text: &str, ctx: &FreezeCtx<'_>) -> Capabilities {
        let mut shell = Shell::new(TerminalState::default());
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
            ("edit-only", EDIT_ONLY_RAL),
            ("confined", CONFINED_RAL),
            ("dangerous", DANGEROUS_RAL),
        ] {
            load(name, text, &ctx);
        }
    }

    /// C1 regression, Windows fixture: every bake-in carries Unix-only
    /// absolute literals (`/tmp`, `/usr/local/bin/`, `/opt/homebrew/`,
    /// …) — rooted paths with no drive letter.  Before the fix these
    /// failed the freeze pass's absoluteness check and the *entire*
    /// profile refused to load, leaving `dangerous` the only usable
    /// Windows base.  Now they are dropped as dead grants at freeze
    /// time and every base loads cleanly.  Windows-only: the freeze
    /// pass's foreign-rooted branch only fires under a real
    /// `cfg!(windows)`.
    #[cfg(windows)]
    #[test]
    fn bakeins_parse_on_windows() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new(r"C:\work"),
        };
        for (name, text) in [
            ("minimal", MINIMAL_RAL),
            ("reasonable", REASONABLE_RAL),
            ("read-only", READ_ONLY_RAL),
            ("edit-only", EDIT_ONLY_RAL),
            ("confined", CONFINED_RAL),
            ("dangerous", DANGEROUS_RAL),
        ] {
            load(name, text, &ctx);
        }
    }

    /// C1 regression, Windows fixture: the Unix-only `/tmp` fs literal
    /// and the `/opt/homebrew/` exec-dir override in `minimal` are
    /// both foreign-rooted on Windows and must be dropped rather than
    /// carried forward as grants that can never match a real access —
    /// `policy show` should never advertise authority it can't back.
    #[cfg(windows)]
    #[test]
    fn minimal_drops_foreign_rooted_grants_on_windows() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new(r"C:\work"),
        };
        let caps = load("minimal", MINIMAL_RAL, &ctx);
        let fs = caps.fs.as_ref().expect("minimal declares fs");
        assert!(
            !fs.read_prefixes.iter().any(|p| p.as_str() == "/tmp"),
            "the Unix-only '/tmp' literal must not survive freeze on Windows"
        );
        let exec = caps.exec.as_ref().expect("minimal declares exec");
        assert!(
            !exec.dirs.contains_key("/opt/homebrew"),
            "the foreign-rooted '/opt/homebrew/' override must not survive freeze on Windows"
        );
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
    /// where someone re-adds `cwd:` to `write_prefixes`.  `cwd:` freezes
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
                    sh.check_exec_args("./configure", &["./configure", "/work/proj/configure"], &[])
                })
            })
            .expect("./configure under cwd: must be admitted");
    }

    /// Regression: a command at /opt/homebrew/bin/cmake — invoked
    /// by short name OR full absolute path — must be admitted by
    /// reasonable's `system:` exec grant even though cmake itself is
    /// not a per-name entry.  `system:` only contributes a Homebrew
    /// root when one exists on the host (the plan's "when present"
    /// qualifier), so this test is a no-op on a Homebrew-less
    /// machine rather than a false regression — GitHub's hosted macOS
    /// runners ship Homebrew pre-installed, so the assertion still
    /// runs in CI.
    ///
    /// Unix-only: the path under test (`/opt/homebrew/bin/cmake`) has
    /// no Windows analogue in the bake-in policy.
    #[cfg(unix)]
    #[test]
    fn reasonable_admits_cmake_under_homebrew_when_present() {
        if !ral_core::path::exists("/opt/homebrew") {
            return;
        }
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let caps = load("reasonable", REASONABLE_RAL, &ctx);
        assert!(
            caps.exec
                .as_ref()
                .is_some_and(|m| m.dirs.contains_key("/opt/homebrew")),
            "reasonable should list /opt/homebrew in exec dirs when it exists on this host"
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
        let cargo = format!("{home}/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo");
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
        let gobin = format!("{home}/go/bin/goimports");
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
        let node = format!("{home}/.nvm/versions/node/v22.0.0/bin/node");
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
        let versioned = format!("{home}/.pyenv/versions/3.12.0/bin/python3");
        let shim = format!("{home}/.pyenv/shims/python3");
        for (name, abs) in [("python3", versioned.as_str()), ("python3", shim.as_str())] {
            let mut shell = Shell::default();
            shell
                .with_capabilities(caps.clone(), |sh| {
                    sh.check_exec_args(name, &[name, abs], &[])
                })
                .unwrap_or_else(|_| panic!("{name} at {abs} must be admitted"));
        }
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

    /// `minimal` admits the *system* git under `/usr/bin/` (folded into
    /// `system:`'s tool-root grant) but not a Homebrew git under
    /// `/opt/homebrew/bin` — minimal carries an explicit
    /// `/opt/homebrew/': 'deny'` override precisely so `system:`
    /// including Homebrew when present doesn't widen minimal's
    /// documented "no homebrew tree" narrowing; user-installed brew
    /// tools stay opt-in.  A non-root cwd keeps the `cwd:/` allow from
    /// masking the test: under `cwd: /` everything resolves inside the
    /// working tree.  The git extension's `git: 'allow'` is what
    /// carries a Homebrew install.
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

    /// Every base that declares `exec` must *admit* (not merely
    /// reference) the platform's live tool roots — i.e. `system:`
    /// really expanded, wiring-wise, to an `Allow` verdict.  Asserting
    /// only `contains_key` would pass on a `Deny` entry too, missing
    /// the whole point of "admits".  Not gated on `cfg(unix)`: on a
    /// real Windows host (`windows-check`) `system_tool_roots` walks
    /// the Windows branch instead, and this same assertion holds.
    ///
    /// One documented exception: `minimal` explicitly denies Homebrew
    /// even though `system:` folds it in when present (see
    /// `minimal_admits_system_git_not_homebrew`) — that override must
    /// survive, so this test asserts `Deny` there instead of `Allow`.
    #[test]
    fn every_exec_base_admits_live_system_tool_roots() {
        let home = ral_core::path::home_from_env();
        // An absolute path on every platform (unlike the Unix-shaped
        // `/work` literal other tests in this file use, which is not
        // absolute under Windows path semantics — those tests are
        // `cfg(unix)`-gated for exactly that reason; this one is not).
        let cwd = std::env::temp_dir();
        let ctx = FreezeCtx {
            home: &home,
            cwd: &cwd,
        };
        let roots = ral_core::path::sigil::system_tool_roots();
        for (name, text) in [
            ("reasonable", REASONABLE_RAL),
            ("edit-only", EDIT_ONLY_RAL),
            ("read-only", READ_ONLY_RAL),
            ("minimal", MINIMAL_RAL),
            ("confined", CONFINED_RAL),
        ] {
            let caps = load(name, text, &ctx);
            let exec = caps
                .exec
                .as_ref()
                .unwrap_or_else(|| panic!("{name} should declare exec"));
            for root in &roots {
                let normalized = ral_core::path::NormalizedPrefix::from_surface(root).into_string();
                let expected = if name == "minimal" && root == "/opt/homebrew" {
                    ExecDir::Deny
                } else {
                    ExecDir::Allow
                };
                assert_eq!(
                    exec.dirs.get(&normalized),
                    Some(&expected),
                    "{name} should carry a {expected:?} verdict for the live system tool root {normalized}"
                );
            }
        }
    }

    /// Windows fixture: `system_tool_roots`' Windows branch, exercised
    /// directly with synthetic env values so it produces a sane
    /// Windows grant set from any host — `%SystemRoot%\System32`, the
    /// bundled PowerShell home, and Git-for-Windows' `usr\bin` when a
    /// synthetic "install" is present.
    #[test]
    fn windows_tool_roots_produce_a_sane_grant_set() {
        let roots =
            ral_core::path::sigil::windows_tool_roots(r"C:\Windows", &[r"C:\Program Files"], |p| {
                p == r"C:\Program Files\Git\usr\bin"
            });
        assert!(
            roots.contains(&r"C:\Windows\System32".to_string()),
            "{roots:?}"
        );
        assert!(
            roots.contains(&r"C:\Windows\System32\WindowsPowerShell\v1.0".to_string()),
            "{roots:?}"
        );
        assert!(
            roots.contains(&r"C:\Program Files\Git\usr\bin".to_string()),
            "{roots:?}"
        );
    }

    /// Windows fixture: the `coreutils-unix-only` grants (`tac`,
    /// `test`, among the shipped bases) are dropped when the platform
    /// can't back them, so `reasonable`'s rendered exec map never
    /// advertises a bundled tool Windows doesn't have.  `false`
    /// simulates "not unix" from any host — see [`drop_dead_exec_grants`].
    #[cfg(unix)]
    #[test]
    fn reasonable_drops_unix_only_bundled_tool_grants_off_unix() {
        let home = ral_core::path::home_from_env();
        let ctx = FreezeCtx {
            home: &home,
            cwd: Path::new("/"),
        };
        let mut caps = load("reasonable", REASONABLE_RAL, &ctx);
        {
            let exec = caps.exec.as_ref().expect("reasonable declares exec");
            assert!(
                exec.literals.contains_key("tac"),
                "host build should still bundle tac"
            );
            assert!(
                exec.literals.contains_key("test"),
                "host build should still bundle test"
            );
        }

        drop_dead_exec_grants(&mut caps, false);

        let exec = caps.exec.as_ref().expect("reasonable declares exec");
        for dead in ral_core::builtins::uutils::COREUTILS_UNIX_ONLY_TOOLS {
            assert!(
                !exec.literals.contains_key(*dead),
                "'{dead}' should be dropped when the platform can't bundle it"
            );
        }
        assert!(
            exec.literals.contains_key("git"),
            "an ordinary named binary must survive the drop"
        );
        assert!(
            exec.literals.contains_key("cat"),
            "a cross-platform bundled tool must survive the drop"
        );
    }
}
