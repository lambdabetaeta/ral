//! Capability composition for exarch.
//!
//! ```text
//!   ceiling   = base ∨ extend_base?
//!   effective = ceiling ⊓ restrict₁ ⊓ restrict₂ ⊓ ...
//! ```
//!
//! Two phases over the same lattice: a single optional join widens
//! the ceiling, then any number of meets attenuate from it.  Both
//! phases are commutative within themselves; meet within itself is
//! also order-independent.  Composition is explicit only — nothing
//! is auto-loaded.
//!
//! `base` is selected by `--base <name>`.  Three bake-ins, no other
//! resolution:
//!
//!   - `minimal`    — coreutils + cwd + /tmp + net + chdir.  The
//!                    practical bottom for any real session.
//!   - `reasonable` — coreutils + everyday tooling (shells, git,
//!                    curl/wget, search, scripting), plus standard
//!                    system/package-manager binary dirs so build
//!                    drivers work.  Default when `--base` is omitted.
//!   - `dangerous`  — `Capabilities::root()`.  Lattice top; no
//!                    attenuation.  Use only when something else is
//!                    the trust boundary (e.g. a Docker container).
//!                    To "start permissive" expressed as a TOML,
//!                    combine `--base dangerous --restrict <FILE>`:
//!                    `root ⊓ file = file`.
//!
//! Each bake-in is authored as a TOML file in `exarch/data/` and
//! baked into the binary at build time via `include_str!`.  Edit the
//! TOML and rebuild to change what a base allows.  For `minimal` and
//! `reasonable`, any `[fs]` block in the TOML is *unioned* with cwd +
//! /tmp + tempdir at runtime, since cwd is per-invocation.
//!
//! `--extend-base <FILE>` (single, optional) widens the ceiling
//! before attenuation runs.  Source it from a path you control;
//! never auto-loaded — widening from project-controlled bytes would
//! defeat the point of the base.
//!
//! `--restrict <FILE>` (repeatable) attenuates the ceiling.  Each
//! loaded file's absolute path is added to `fs.deny_paths`, so the
//! agent can read the file (to inspect what constrains it) but
//! cannot modify it.

use ral_core::types::{Capabilities, FsPolicy};
use std::path::{Path, PathBuf};

const MINIMAL_TOML: &str = include_str!("../data/minimal.exarch.toml");
const REASONABLE_TOML: &str = include_str!("../data/reasonable.exarch.toml");
const DANGEROUS_TOML: &str = include_str!("../data/dangerous.exarch.toml");

/// Compute the effective `Capabilities` for a session.
///
/// `base_name` selects the ceiling: `minimal`, `reasonable`, or
/// `dangerous`.  `extend_base`, if `Some`, is loaded and joined into
/// the ceiling.  Each entry in `restrict_files` is loaded and meet'd
/// in.
///
/// Every restrict file's absolute path (lexical *and* canonical) is
/// added to `fs.deny_paths`, making the input bytes structurally
/// unreachable to the agent.  The extend-base file is *not* added to
/// deny_paths: it widens authority, so denying writes to it is a
/// trust-source concern — the user owns where it lives.
pub fn for_invocation(
    cwd: &str,
    base_name: &str,
    extend_base: Option<&Path>,
    restrict_files: &[PathBuf],
) -> Result<(Capabilities, Vec<PathBuf>), String> {
    let mut caps = resolve_base(cwd, base_name)?;

    if let Some(path) = extend_base {
        let abs = absolute_in(cwd, path);
        caps = caps.join(load_capabilities_toml(&abs, "--extend-base")?);
    }

    let restricts: Vec<PathBuf> = restrict_files.iter().map(|p| absolute_in(cwd, p)).collect();
    for path in &restricts {
        caps = caps.meet(load_capabilities_toml(path, "--restrict")?);
    }

    if !restricts.is_empty() {
        let fs = caps.fs.get_or_insert_with(root_fs_policy);
        for p in &restricts {
            let s = p.to_string_lossy().into_owned();
            fs.deny_paths.push(s.clone());
            fs.deny_paths.push(canon(&s));
        }
        fs.deny_paths.sort();
        fs.deny_paths.dedup();
    }

    Ok((caps, restricts))
}

/// Resolve `base_name` to a `Capabilities`.  Each bake-in is parsed
/// from its embedded TOML; for `minimal` and `reasonable` the dynamic
/// fs prefixes (cwd / /tmp / tempdir) are unioned with whatever `[fs]`
/// the TOML declares — never overwritten — so `reasonable` can ship
/// build-toolchain caches statically.
fn resolve_base(cwd: &str, name: &str) -> Result<Capabilities, String> {
    let (text, dynamic_fs) = match name {
        "minimal" => (MINIMAL_TOML, true),
        "reasonable" => (REASONABLE_TOML, true),
        "dangerous" => (DANGEROUS_TOML, false),
        other => {
            return Err(format!(
                "exarch: unknown base '{other}'; expected 'minimal', 'reasonable', or 'dangerous'"
            ));
        }
    };
    let mut caps: Capabilities = toml::from_str(text)
        .map_err(|e| format!("exarch: built-in base '{name}' failed to parse: {e}"))?;
    if dynamic_fs {
        let runtime_caps = Capabilities {
            fs: Some(runtime_fs_policy(cwd)),
            ..Capabilities::default()
        };
        caps = caps.join(runtime_caps);
    }
    Ok(caps)
}

/// fs policy for `minimal` and `reasonable`: cwd + /tmp + tempdir,
/// both lexical and canonical forms, on read and write.  Empty deny
/// list — `for_invocation` appends to it as files are loaded.
fn runtime_fs_policy(cwd: &str) -> FsPolicy {
    let tmp = std::env::temp_dir().to_string_lossy().into_owned();
    let mut prefixes = Vec::new();
    for p in [cwd, "/tmp", &tmp] {
        prefixes.push(p.to_string());
        prefixes.push(canon(p));
    }
    prefixes.sort();
    prefixes.dedup();
    FsPolicy {
        read_prefixes: prefixes.clone(),
        write_prefixes: prefixes,
        deny_paths: Vec::new(),
    }
}

/// Preserve otherwise-unrestricted filesystem authority while still
/// carving out `deny_paths` for active restriction files.
fn root_fs_policy() -> FsPolicy {
    FsPolicy {
        read_prefixes: vec!["/".into()],
        write_prefixes: vec!["/".into()],
        deny_paths: Vec::new(),
    }
}

/// Read a capabilities profile from `path`.  `flag` is the CLI flag
/// the path arrived through, used in error messages.  Missing files
/// are an error: composition is explicit, so a path the user typed
/// must resolve.
fn load_capabilities_toml(path: &Path, flag: &str) -> Result<Capabilities, String> {
    let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            format!("exarch: {flag} path does not exist: {}", path.display())
        }
        _ => format!("exarch: failed to read {}: {e}", path.display()),
    })?;
    toml::from_str(&text).map_err(|e| format!("exarch: failed to parse {}: {e}", path.display()))
}

fn absolute_in(cwd: &str, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

/// Canonicalise `p`; fall back to the input when resolution fails.
fn canon(p: &str) -> String {
    std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All three bake-ins must parse — catches malformed TOML at
    /// `cargo test` time rather than at first user invocation.
    #[test]
    fn bakeins_parse() {
        for (name, text) in [
            ("minimal", MINIMAL_TOML),
            ("reasonable", REASONABLE_TOML),
            ("dangerous", DANGEROUS_TOML),
        ] {
            toml::from_str::<Capabilities>(text)
                .unwrap_or_else(|e| panic!("base '{name}' failed to parse: {e}"));
        }
    }

    #[test]
    fn dangerous_is_root() {
        let caps: Capabilities = toml::from_str(DANGEROUS_TOML).unwrap();
        assert_eq!(caps, Capabilities::root());
    }

    #[test]
    fn restrict_files_are_denied_even_under_dangerous_base() {
        let path = std::env::temp_dir().join(format!(
            "exarch-restrict-test-{}-{}.toml",
            std::process::id(),
            "dangerous",
        ));
        std::fs::write(&path, "[exec]\nls = \"Allow\"\n").unwrap();

        let (caps, _) =
            for_invocation("/", "dangerous", None, std::slice::from_ref(&path)).unwrap();
        let fs = caps.fs.expect("restrict file should install fs carve-out");
        assert_eq!(fs.read_prefixes, vec!["/"]);
        assert_eq!(fs.write_prefixes, vec!["/"]);
        assert!(
            fs.deny_paths.iter().any(|p| p == &path.to_string_lossy()),
            "restrict file path should be write-denied"
        );

        let _ = std::fs::remove_file(path);
    }
}
