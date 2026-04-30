//! Built-in capability bases for exarch sessions.
//!
//! Three bake-ins are embedded from TOML files in `exarch/data/`:
//!
//! - `minimal`    — coreutils + cwd + /tmp + tempdir + net + chdir.
//! - `reasonable` — everyday tooling + standard binary dirs (default).
//! - `dangerous`  — `Capabilities::root()`.  Lattice top; no attenuation.
//!
//! `minimal` and `reasonable` use `cwd:` and `tempdir:` sigils in
//! their `[fs]` and `exec_dirs` lists; the freeze pass in
//! [`ral_core::types::RawCapabilities::freeze`] resolves both at
//! session start, so the per-invocation working directory is baked
//! into the policy without exarch having to inject it dynamically.

use ral_core::types::{FsPolicy, RawCapabilities};

const MINIMAL_TOML: &str = include_str!("../../data/minimal.exarch.toml");
const REASONABLE_TOML: &str = include_str!("../../data/reasonable.exarch.toml");
const DANGEROUS_TOML: &str = include_str!("../../data/dangerous.exarch.toml");

/// Resolve `name` to a [`RawCapabilities`].  Returns unfrozen so
/// the orchestrator (`policy::for_invocation`) can join an
/// extend-base and meet restrict files before a single freeze
/// pass settles every sigil in the composed result.
pub(super) fn resolve_base(name: &str) -> Result<RawCapabilities, String> {
    let text = match name {
        "minimal" => MINIMAL_TOML,
        "reasonable" => REASONABLE_TOML,
        "dangerous" => DANGEROUS_TOML,
        other => {
            return Err(format!(
                "exarch: unknown base '{other}'; \
                 expected 'minimal', 'reasonable', or 'dangerous'"
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

    /// All three bake-ins must parse and validate.  Catches both
    /// malformed TOML and unknown `xdg:` tokens at `cargo test` time
    /// rather than at first user invocation.
    #[test]
    fn bakeins_parse() {
        for (name, text) in [
            ("minimal", MINIMAL_TOML),
            ("reasonable", REASONABLE_TOML),
            ("dangerous", DANGEROUS_TOML),
        ] {
            let raw: RawCapabilities = toml::from_str(text)
                .unwrap_or_else(|e| panic!("base '{name}' failed to parse: {e}"));
            raw.validate_paths()
                .unwrap_or_else(|e| panic!("base '{name}' failed validation: {e}"));
        }
    }

    #[test]
    fn dangerous_is_root() {
        let raw: RawCapabilities = toml::from_str(DANGEROUS_TOML).unwrap();
        assert_eq!(raw, RawCapabilities::default());
    }

    /// Reasonable's exec_dirs includes `xdg:bin`, which expands to
    /// `${XDG_BIN_HOME:-~/.local/bin}` at freeze time.  Pre-freeze
    /// the entry is a literal string; this asserts it survives
    /// parsing.
    #[test]
    fn reasonable_carries_xdg_bin_in_exec_dirs() {
        let raw: RawCapabilities = toml::from_str(REASONABLE_TOML).unwrap();
        assert!(
            raw.exec_dirs.as_ref().is_some_and(|d| d.iter().any(|p| p == "xdg:bin")),
            "reasonable should list xdg:bin in exec_dirs"
        );
    }

    /// `cwd:` and `tempdir:` sigils land in both the fs and exec_dirs
    /// lists for `minimal` and `reasonable` so a per-invocation
    /// working tree is admitted without exarch injecting it dynamically.
    #[test]
    fn minimal_and_reasonable_carry_cwd_and_tempdir_sigils() {
        for (name, text) in [("minimal", MINIMAL_TOML), ("reasonable", REASONABLE_TOML)] {
            let raw: RawCapabilities = toml::from_str(text).unwrap();
            let dirs = raw.exec_dirs.as_ref().unwrap_or_else(|| {
                panic!("{name} should declare exec_dirs")
            });
            assert!(dirs.iter().any(|p| p == "cwd:"), "{name} exec_dirs missing cwd:");
            assert!(dirs.iter().any(|p| p == "tempdir:"), "{name} exec_dirs missing tempdir:");
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
    /// reasonable's exec_dirs even though cmake is not in [exec].
    #[test]
    fn reasonable_admits_cmake_under_opt_homebrew_bin() {
        let raw: RawCapabilities = toml::from_str(REASONABLE_TOML).unwrap();
        assert!(
            raw.exec_dirs.as_ref().is_some_and(|d| d.iter().any(|p| p == "/opt/homebrew/bin")),
            "reasonable should list /opt/homebrew/bin in exec_dirs"
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

    /// `reasonable` lists the standard system `bin` directories in
    /// `exec_dirs`, which would otherwise admit `/bin/bash`,
    /// `/usr/bin/zsh`, …  An explicit `Deny` in `[exec]` is the
    /// override knob: name-match wins over directory match, so the
    /// agent cannot reach those shells through the admitted dirs.
    /// `sh` itself stays allowed — autoconf-generated `configure`
    /// scripts and `make` recipes shell out via `/bin/sh -c`, and
    /// denying it breaks every portable build system.
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
