//! The six built-in capability bases, embedded from the `.ral` scripts in
//! `exarch/data/` and loaded through
//! [`load_capabilities_from_str`](ral_core::capability::load_capabilities_from_str),
//! the same surface the ral CLI's `--capabilities` flag consumes.
//!
//! Loosest to tightest: `dangerous` (the lattice top), `reasonable`,
//! `edit-only`, `read-only`, `minimal`, `confined`.  Every profile but
//! `dangerous` names `cwd:` and `tempdir:` sigils, which the loader freezes at
//! session start — so the per-invocation working directory is baked into the
//! policy and exarch never injects it dynamically.

use ral_core::io::TerminalState;
use ral_core::types::{Capabilities, FsPolicy, Shell};

const MINIMAL_RAL: &str = include_str!("../../data/minimal.exarch.ral");
const REASONABLE_RAL: &str = include_str!("../../data/reasonable.exarch.ral");
const READ_ONLY_RAL: &str = include_str!("../../data/read-only.exarch.ral");
const EDIT_ONLY_RAL: &str = include_str!("../../data/edit-only.exarch.ral");
const CONFINED_RAL: &str = include_str!("../../data/confined.exarch.ral");
const DANGEROUS_RAL: &str = include_str!("../../data/dangerous.exarch.ral");
// Gated with its only consumers, the Unix-shaped extension-join tests below.
#[cfg(all(test, unix))]
const GIT_EXTENSION_RAL: &str = include_str!("../../examples/git.exarch.ral");

/// Resolve `name` to a frozen [`Capabilities`], every sigil resolved against
/// `ctx`, so `super::for_invocation` composes on already-resolved bundles.
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
    let mut caps = ral_core::capability::load_capabilities_from_str(
        &ral_core::types::Mooring::adrift(),
        &mut shell,
        text,
        &virtual_path,
        ctx,
    )
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

/// Drop exec literals naming a coreutil this platform cannot bundle — the
/// `coreutils-unix-only` set, whose `uu_*` crates compile on Unix alone — so a
/// rendered profile never advertises a grant it cannot honour.
///
/// `unix_available` is a parameter rather than a `cfg!(unix)` read here so the
/// non-Unix shape is testable from a Unix host.
fn drop_dead_exec_grants(caps: &mut Capabilities, unix_available: bool) {
    if unix_available {
        return;
    }
    if let Some(exec) = caps.exec.as_mut() {
        exec.literals.retain(|name, _| {
            !ral_core::uutils::COREUTILS_UNIX_ONLY_TOOLS.contains(&name.as_str())
        });
    }
}

/// The implicit ceiling made concrete: `super::deny_paths` installs this so a
/// restriction file's carve-outs land on a policy rather than on nothing.  `/`
/// is minted in the normal form every other grant-side prefix carries.
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
    // Gated with their only users below, so the Windows build stays warning-free.
    #[cfg(unix)]
    use ral_core::path::sigil::expand_path_prefix;
    #[cfg(unix)]
    use ral_core::types::ExecPolicy;
    use ral_core::types::{Capabilities, Shell};
    use std::path::Path;

    /// Parse *and* freeze a bake-in: the loader resolves sigils, so a failure
    /// here is malformed source, an unknown `xdg:` token, or an xdg escape.
    fn load(name: &str, text: &str, ctx: &FreezeCtx<'_>) -> Capabilities {
        let mut shell = Shell::new(TerminalState::default());
        ral_core::capability::load_capabilities_from_str(
            &ral_core::types::Mooring::adrift(),
            &mut shell,
            text,
            &format!("<test-base:{name}>"),
            ctx,
        )
        .unwrap_or_else(|e| panic!("base '{name}' failed to load: {e:?}"))
    }

    /// Every bake-in must load against the real `$HOME`, so a broken profile
    /// fails at `cargo test` rather than at a user's first invocation.
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

    /// Every bake-in carries Unix-only literals (`/tmp`, `/opt/homebrew/`, …).
    /// On Windows they are foreign-rooted and must drop as dead grants rather
    /// than trip the freeze pass's absoluteness check, or the whole profile
    /// would refuse to load and `dangerous` would be the only usable base.
    /// Gated because that branch fires only under a real `cfg!(windows)`.
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

    /// The drop reaches fs literals and exec-dir overrides alike: a grant that
    /// can never match a real access must not show up as authority.
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
            !exec
                .allow_dirs
                .iter()
                .any(|p| p.as_str() == "/opt/homebrew")
                && !exec.deny_dirs.iter().any(|p| p.as_str() == "/opt/homebrew"),
            "the foreign-rooted '/opt/homebrew/' override must not survive freeze on Windows"
        );
    }

    /// The three load-bearing differences from `reasonable`, pinned so an edit
    /// cannot quietly widen the build jail.  `confined` names no `~`/`xdg:`
    /// path, so a synthetic home suffices — and nothing may resolve under it.
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

    /// The one difference from `reasonable`: the working tree is readable but
    /// not writable.  Guards against `cwd:` creeping back into `write`.
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

    /// `xdg:bin` freezes to `${XDG_BIN_HOME:-~/.local/bin}` and must land in
    /// the `dirs` half of the exec map, where directory keys live.
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
            exec.allow_dirs.iter().any(|p| p.as_str() == xdg_bin),
            "reasonable should list the resolved xdg:bin ({xdg_bin}) in exec dirs"
        );
    }

    /// The per-invocation working tree and temp dir reach both halves of the
    /// policy: directory keys in `exec`, plain sigils in `fs`.
    #[cfg(unix)]
    #[test]
    fn minimal_and_reasonable_carry_cwd_and_tempdir_sigils() {
        let home = ral_core::path::home_from_env();
        let cwd = Path::new("/work/proj");
        let ctx = FreezeCtx { home: &home, cwd };
        // Freeze folds away the trailing separator macOS `$TMPDIR` carries, so
        // compare in the same normal form the frozen keys hold.
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
                exec.allow_dirs.iter().any(|p| p.as_str() == cwd_resolved),
                "{name} exec missing resolved cwd"
            );
            assert!(
                exec.allow_dirs
                    .iter()
                    .any(|p| p.as_str() == tempdir_resolved),
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

    /// End-to-end: once freeze rewrites `cwd:` into the working directory, a
    /// path-style exec beneath it is admitted.
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

    /// `system:`'s Homebrew root admits cmake by short name and by full path
    /// alike, though cmake is no per-name entry.  That root exists only where
    /// Homebrew does, so on a brew-less host this passes vacuously rather than
    /// failing falsely; CI's macOS runners ship Homebrew, so it still bites.
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
                .is_some_and(|m| m.allow_dirs.iter().any(|p| p.as_str() == "/opt/homebrew")),
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

    /// Rustup resolves `cargo` under `~/.rustup/toolchains/`, a path shape easy
    /// to miss when hand-enumerating exec roots.
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

    /// Both the versioned install and the shim layer pyenv puts on `$PATH`.
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

    /// Both admit `git` and read `~/.gitconfig`, so a commit works without
    /// `--extend-base`.  Push and signed commits still don't: their credential
    /// and signing helpers need `~/.ssh` and `~/.gnupg`, which the fs grant
    /// leaves unreadable.
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

    /// `minimal`'s explicit `/opt/homebrew/` deny is what keeps `system:`,
    /// which folds a Homebrew tree in when the host has one, from widening it:
    /// brew tools stay opt-in, and the git extension is the opt-in.  The cwd
    /// must not be `/`, or the `cwd:/` allow would admit everything.
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

    /// The extension widens `minimal` — gitconfig readable, `git` admitted —
    /// while `reasonable`'s credential denies survive: `FsPolicy::join` unions
    /// deny sets, so a veto sticks without the overlay re-stating it, which is
    /// what lets an overlay stay purely additive.  Both sides freeze against
    /// the same home, so their `xdg:config/*` paths coincide.
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

    /// `system:` must expand to an `Allow` verdict for every live tool root —
    /// merely finding the key would pass on a `Deny` too, which is the whole
    /// point.  `minimal` is the one exception: its Homebrew override must
    /// survive as a `Deny`.  Ungated, since `system_tool_roots` walks the
    /// platform's own branch and the assertion holds either way.
    #[test]
    fn every_exec_base_admits_live_system_tool_roots() {
        let home = ral_core::path::home_from_env();
        // Absolute on every platform, unlike the `/work` literal the
        // `cfg(unix)`-gated tests use — Windows reads that as rootless.
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
                let denied = name == "minimal" && root == "/opt/homebrew";
                let (expected, in_set) = if denied {
                    (
                        "Deny",
                        exec.deny_dirs.iter().any(|p| p.as_str() == normalized),
                    )
                } else {
                    (
                        "Allow",
                        exec.allow_dirs.iter().any(|p| p.as_str() == normalized),
                    )
                };
                assert!(
                    in_set,
                    "{name} should carry a {expected} verdict for the live system tool root {normalized}"
                );
            }
        }
    }

    /// The Windows branch, driven with synthetic env and a synthetic install
    /// probe so it can be checked from any host.
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

    /// Passing [`drop_dead_exec_grants`] `false` simulates a non-Unix host:
    /// the `coreutils-unix-only` grants go, ordinary and cross-platform names
    /// stay.  Unix-gated because it first checks the host really bundles them.
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
        for dead in ral_core::uutils::COREUTILS_UNIX_ONLY_TOOLS {
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
