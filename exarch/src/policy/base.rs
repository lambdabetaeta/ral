//! Built-in capability bases for exarch sessions.
//!
//! Five bake-ins are embedded from TOML files in `exarch/data/`,
//! ordered loosely from no-attenuation down to tightest:
//!
//! - `dangerous`  — `Capabilities::root()`.  Lattice top; no attenuation.
//! - `reasonable` — everyday tooling + standard binary dirs (default).
//! - `read-only`  — `reasonable` reads/exec, but writes only to scratch.
//! - `minimal`    — coreutils + cwd + /tmp + tempdir + net + chdir.
//!                  Small base for additive `--extend-base` composition.
//! - `confined`   — build-jail shape (after BrianSwift's `confined.sb`):
//!                  tight reads/writes, no network, exec by subpath only.
//!
//! `minimal`, `confined`, `read-only`, and `reasonable` use `cwd:` and
//! `tempdir:` sigils in their `[fs]` and `[exec]` entries; the freeze
//! pass in [`ral_core::types::RawCapabilities::freeze`] resolves them
//! at session start, so the per-invocation working directory is baked
//! into the policy without exarch having to inject it dynamically.

use ral_core::types::{FsPolicy, RawCapabilities};

const MINIMAL_TOML:    &str = include_str!("../../data/minimal.exarch.toml");
const REASONABLE_TOML: &str = include_str!("../../data/reasonable.exarch.toml");
const READ_ONLY_TOML:  &str = include_str!("../../data/read-only.exarch.toml");
const CONFINED_TOML:   &str = include_str!("../../data/confined.exarch.toml");
const DANGEROUS_TOML:  &str = include_str!("../../data/dangerous.exarch.toml");

/// Resolve `name` to a [`RawCapabilities`].  Returns unfrozen so
/// the orchestrator (`policy::for_invocation`) can join an
/// extend-base and meet restrict files before a single freeze
/// pass settles every sigil in the composed result.
pub(super) fn resolve_base(name: &str) -> Result<RawCapabilities, String> {
    let text = match name {
        "minimal"    => MINIMAL_TOML,
        "reasonable" => REASONABLE_TOML,
        "read-only"  => READ_ONLY_TOML,
        "confined"   => CONFINED_TOML,
        "dangerous"  => DANGEROUS_TOML,
        other => {
            return Err(format!(
                "exarch: unknown base '{other}'; \
                 expected one of: minimal, reasonable, read-only, confined, dangerous"
            ));
        }
    };
    toml::from_str(text)
        .map_err(|e| format!("exarch: built-in base '{name}' failed to parse: {e}"))
}

/// Preserve otherwise-unrestricted filesystem authority while still
/// carving out `deny_paths` for active restriction files.
pub(super) fn root_fs_policy() -> FsPolicy {
    FsPolicy {
        read_prefixes: vec!["/".into()],
        write_prefixes: vec!["/".into()],
        deny_paths: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::types::{Capabilities, RawCapabilities, Shell};

    /// Every bake-in must parse and validate.  Catches both malformed
    /// TOML and unknown `xdg:` tokens at `cargo test` time rather
    /// than at first user invocation.
    #[test]
    fn bakeins_parse() {
        for (name, text) in [
            ("minimal",    MINIMAL_TOML),
            ("reasonable", REASONABLE_TOML),
            ("read-only",  READ_ONLY_TOML),
            ("confined",   CONFINED_TOML),
            ("dangerous",  DANGEROUS_TOML),
        ] {
            let raw: RawCapabilities = toml::from_str(text)
                .unwrap_or_else(|e| panic!("base '{name}' failed to parse: {e}"));
            raw.validate_paths()
                .unwrap_or_else(|e| panic!("base '{name}' failed validation: {e}"));
        }
    }

    /// `confined` is the build-jail profile: net off, no user-home
    /// reads, exec by subpath only.  These three properties are the
    /// load-bearing differences vs `reasonable`; pin them so a future
    /// edit doesn't accidentally widen the build jail.
    #[test]
    fn confined_is_offline_subpath_only_no_home_reads() {
        let raw: RawCapabilities = toml::from_str(CONFINED_TOML).unwrap();
        assert_eq!(raw.net, Some(false), "confined must have net off");
        let exec = raw.exec.as_ref().expect("confined declares [exec]");
        // No bare-name admits — every key is a subpath (trailing /).
        for key in exec.keys() {
            assert!(
                key.ends_with('/'),
                "confined [exec] '{key}' is bare-name; build jail uses subpaths only"
            );
        }
        let fs = raw.fs.as_ref().expect("confined declares [fs]");
        for prefix in fs.read_prefixes.iter().chain(fs.write_prefixes.iter()) {
            assert!(
                !prefix.starts_with('~'),
                "confined fs prefix '{prefix}' reaches into ~ — build jail must not"
            );
        }
    }

    /// `read-only` differs from `reasonable` only in that writes
    /// don't include the working tree.  Fold a future regression
    /// where someone re-adds `cwd:` to write_prefixes.
    #[test]
    fn read_only_does_not_write_cwd() {
        let raw: RawCapabilities = toml::from_str(READ_ONLY_TOML).unwrap();
        let fs = raw.fs.as_ref().expect("read-only declares [fs]");
        assert!(
            !fs.write_prefixes.iter().any(|p| p == "cwd:"),
            "read-only must not list cwd: in write_prefixes"
        );
        assert!(
            fs.read_prefixes.iter().any(|p| p == "cwd:"),
            "read-only must list cwd: in read_prefixes"
        );
    }

    #[test]
    fn dangerous_is_root() {
        let raw: RawCapabilities = toml::from_str(DANGEROUS_TOML).unwrap();
        assert_eq!(raw, RawCapabilities::default());
    }

    /// Reasonable's `[exec]` includes the `xdg:bin/` subpath key,
    /// which expands to `${XDG_BIN_HOME:-~/.local/bin}/` at freeze
    /// time.  Pre-freeze the entry is a literal string; this asserts
    /// it survives parsing.
    #[test]
    fn reasonable_carries_xdg_bin_subpath_in_exec() {
        let raw: RawCapabilities = toml::from_str(REASONABLE_TOML).unwrap();
        let exec = raw.exec.as_ref().expect("reasonable should declare [exec]");
        assert!(
            exec.contains_key("xdg:bin/"),
            "reasonable should list xdg:bin/ in [exec]"
        );
    }

    /// `cwd:/` and `tempdir:/` subpath keys land in `[exec]` and
    /// matching plain sigils land in `[fs]` for both `minimal` and
    /// `reasonable`, so a per-invocation working tree is admitted
    /// without exarch injecting it dynamically.
    #[test]
    fn minimal_and_reasonable_carry_cwd_and_tempdir_sigils() {
        for (name, text) in [("minimal", MINIMAL_TOML), ("reasonable", REASONABLE_TOML)] {
            let raw: RawCapabilities = toml::from_str(text).unwrap();
            let exec = raw.exec.as_ref().unwrap_or_else(|| {
                panic!("{name} should declare [exec]")
            });
            assert!(exec.contains_key("cwd:/"), "{name} [exec] missing cwd:/");
            assert!(exec.contains_key("tempdir:/"), "{name} [exec] missing tempdir:/");
            let fs = raw.fs.as_ref().unwrap_or_else(|| {
                panic!("{name} should declare [fs]")
            });
            for token in ["cwd:", "tempdir:"] {
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
    /// Replaces the old runtime_fs_policy injection with one round
    /// trip through the freeze pass.
    #[test]
    fn freeze_admits_relative_exec_under_cwd_sigil() {
        let raw: RawCapabilities = toml::from_str(MINIMAL_TOML).unwrap();
        let work = std::path::Path::new("/work/proj");
        let caps = raw
            .freeze(&ral_core::path::sigil::FreezeCtx {
                home: &ral_core::path::home::home_from_env(),
                cwd: work,
            })
            .expect("freeze");

        let mut shell = Shell::default();
        shell.dynamic.cwd = Some(work.to_path_buf());
        shell
            .with_capabilities(caps, |sh| {
                sh.check_exec_args(
                    "./configure",
                    &["./configure", "/work/proj/configure"],
                    &[],
                )
            })
            .expect("./configure under cwd: must be admitted");
    }

    /// Regression: a command at /opt/homebrew/bin/cmake — invoked
    /// by short name OR full absolute path — must be admitted by
    /// reasonable's `/opt/homebrew/bin/` subpath key in `[exec]`
    /// even though cmake itself is not a per-name entry.
    #[test]
    fn reasonable_admits_cmake_under_opt_homebrew_bin() {
        let raw: RawCapabilities = toml::from_str(REASONABLE_TOML).unwrap();
        assert!(
            raw.exec
                .as_ref()
                .is_some_and(|m| m.contains_key("/opt/homebrew/bin/")),
            "reasonable should list /opt/homebrew/bin/ in [exec]"
        );
        let caps: Capabilities = raw
            .freeze(&ral_core::path::sigil::FreezeCtx {
                home: &ral_core::path::home::home_from_env(),
                cwd: std::path::Path::new("/"),
            })
            .expect("freeze");

        let mut shell = Shell::default();
        shell
            .with_capabilities(caps.clone(), |sh| {
                sh.check_exec_args("cmake", &["cmake", "/opt/homebrew/bin/cmake"], &[])
            })
            .expect("short-name cmake under /opt/homebrew/bin must be admitted");

        let mut shell2 = Shell::default();
        shell2
            .with_capabilities(caps, |sh| {
                sh.check_exec_args(
                    "/opt/homebrew/bin/cmake",
                    &["/opt/homebrew/bin/cmake"],
                    &[],
                )
            })
            .expect("full-path cmake under /opt/homebrew/bin must be admitted");
    }

    /// `reasonable` lists the standard system `bin` directories as
    /// subpath keys in `[exec]`, which would otherwise admit
    /// `/bin/bash`, `/usr/bin/zsh`, …  An explicit `Deny` per-name
    /// entry in the same map is the override knob: literal-match
    /// wins over subpath-match, so the agent cannot reach those
    /// shells through the admitted dirs.  `sh` itself stays allowed
    /// — autoconf-generated `configure` scripts and `make` recipes
    /// shell out via `/bin/sh -c`, and denying it breaks every
    /// portable build system.
    #[test]
    fn reasonable_denies_shells_despite_bin_in_exec_dirs() {
        let raw: RawCapabilities = toml::from_str(REASONABLE_TOML).unwrap();
        let caps: Capabilities = raw
            .freeze(&ral_core::path::sigil::FreezeCtx {
                home: &ral_core::path::home::home_from_env(),
                cwd: std::path::Path::new("/"),
            })
            .expect("freeze");

        for (name, abs) in [
            ("bash", "/bin/bash"),
            ("zsh",  "/bin/zsh"),
        ] {
            let mut shell = Shell::default();
            let r = shell.with_capabilities(caps.clone(), |sh| {
                sh.check_exec_args(name, &[name, abs], &[])
            });
            assert!(r.is_err(), "{name} should be denied even though /bin is in exec_dirs");
        }
    }
}
