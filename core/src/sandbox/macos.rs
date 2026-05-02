//! macOS sandbox using the Seatbelt (sandbox_init) API.
//!
//! Single mode of operation: a ral subprocess spawned by
//! `eval_grant_sandboxed` enters the Seatbelt profile once at startup via
//! `enter_current_process`, then evaluates the grant body in-process with
//! every external it spawns inheriting the confinement.  `process-exec`
//! is gated when the projection's `exec` field is `Restricted`: the
//! profile renders a single combined `file-read* process-exec` allow rule
//! over the exec_dirs and resolved [exec] literals, mirroring the idiom
//! used by Apple-blessed build profiles (see BrianSwift/macOSSandboxBuild
//! `confined.sb`).  `Unrestricted` keeps the wildcard allow so plain
//! `grant [fs: …]` blocks without exec attenuation behave as before.
//!
//! We deliberately do *not* apply per-command Seatbelt profiles in the
//! parent ral process or inside plugin handlers: the overhead-vs-benefit
//! is upside-down for ral's use case (an external like fzf needs a sprawl
//! of Seatbelt rules — process-info, IOKit, mach-bootstrap, symlink
//! resolution for the binary itself — and authorising a binary via
//! `exec:` already shifts trust to that binary anyway).  Plugin handlers
//! run externals with the user's full authority; only `grant { fs: ... }
//! / net: ...} body` opts in to OS-level enforcement, via the
//! sandboxed-child path.
//!
//! Network filtering is all-or-nothing at the OS level: Seatbelt does not
//! support per-address rules.  `SandboxProjection::net` is therefore a boolean
//! allow/deny bit, not an endpoint list.

use crate::path::{match_variants_list, proper_ancestors};
use crate::types::{ExecProjection, FsProjection, SandboxProjection};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::Path;

pub(super) fn apply_current_process_policy(policy: &SandboxProjection) -> std::io::Result<()> {
    let profile = build_profile(policy);
    apply_profile(&profile, std::iter::empty::<(&str, &str)>())
}

/// Apply `policy` to the current process and mark the sandbox as active so
/// children inherit the flag and skip re-entry.
pub(super) fn enter_current_process(
    policy: &SandboxProjection,
    active_env: &str,
) -> Result<(), String> {
    apply_current_process_policy(policy)
        .map_err(|e| format!("ral: failed to enter sandbox: {e}"))?;
    unsafe {
        std::env::set_var(active_env, "1");
    }
    Ok(())
}

fn apply_profile<'a>(
    profile: &str,
    parameters: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> std::io::Result<()> {
    fn cstr(s: &str, what: &str) -> std::io::Result<CString> {
        CString::new(s).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{what} contains NUL byte"),
            )
        })
    }
    let profile_cstr = cstr(profile, "sandbox profile")?;
    let mut parameter_storage = Vec::new();
    for (key, value) in parameters {
        parameter_storage.push(cstr(key, "sandbox parameter key")?);
        parameter_storage.push(cstr(value, "sandbox parameter value")?);
    }
    let mut parameter_ptrs: Vec<*const c_char> =
        parameter_storage.iter().map(|s| s.as_ptr()).collect();
    parameter_ptrs.push(std::ptr::null());

    let mut errorbuf: *mut c_char = std::ptr::null_mut();
    let rc = unsafe {
        sandbox_init_with_parameters(
            profile_cstr.as_ptr(),
            0,
            parameter_ptrs.as_ptr(),
            &mut errorbuf,
        )
    };
    if rc != 0 {
        let message = if errorbuf.is_null() {
            "sandbox_init_with_parameters failed".to_string()
        } else {
            unsafe { CStr::from_ptr(errorbuf) }
                .to_string_lossy()
                .into_owned()
        };
        return Err(std::io::Error::other(message));
    }
    Ok(())
}

/// Policy-independent SBPL preamble — `(version 1)`, `(deny default)`,
/// the Apple-required carve-outs (mach-lookup for dyld, process-fork
/// next to exec, root-literal for path resolution, /dev/{null,tty,…}
/// writes for shell redirection).  Lifted into a sibling file so the
/// rules live as readable SBPL rather than `format!()`'d strings, and
/// to make the source idiom — the deny-default + folded `file-read*
/// process-exec` shape from BrianSwift/macOSSandboxBuild's
/// `confined.sb` — citable in one place.
const BASE_PROFILE: &str = include_str!("macos-base.sbpl");

pub(super) fn build_profile(policy: &SandboxProjection) -> String {
    let mut lines: Vec<String> = vec![BASE_PROFILE.to_string()];
    let deny_paths = match &policy.fs {
        FsProjection::Restricted(fs) => emit_fs_restricted(&mut lines, fs),
        FsProjection::Unrestricted => {
            // No fs attenuation in the stack: pass fs through.  Lets
            // exec-only grants enter the OS sandbox for the sake of
            // exec gating without clamping the agent's cwd or HOME.
            lines.push("(allow file-read*)".to_string());
            lines.push("(allow file-write*)".to_string());
            Vec::new()
        }
    };

    emit_exec_rules(&mut lines, &policy.exec);

    // Per-path deny rules.  Emitted *after* the broad allows so
    // Seatbelt's last-match-wins semantics let the deny override.
    // `subpath` (not `literal`) so a directory entry covers everything
    // under it — `xdg:config/gh` denies the whole gh-CLI dir, not just
    // the literal `gh` inode.  `file-link` (no wildcard — Seatbelt has
    // no `file-link*` group) blocks `link(2)` against the source path,
    // closing the hardlink hole where a new name elsewhere would let
    // writes bypass the path-based deny.
    for path in &deny_paths {
        let escaped = escape_path(path);
        lines.push(format!("(deny file-read* (subpath \"{escaped}\"))"));
        lines.push(format!("(deny file-write* (subpath \"{escaped}\"))"));
        lines.push(format!("(deny file-link (subpath \"{escaped}\"))"));
    }
    if policy.net {
        lines.push("(allow network*)".to_string());
    }

    lines.join("\n")
}

/// Emit the per-prefix `(allow file-read* …)` / `(allow file-write* …)`
/// rules and ancestor-metadata carve-outs for a restricted fs policy.
/// Returns the expanded deny_paths so the caller can layer them after
/// every allow rule has been written (Seatbelt is last-match-wins).
fn emit_fs_restricted(lines: &mut Vec<String>, fs: &crate::types::FsPolicy) -> Vec<String> {
    let read_prefixes = match_variants_list(&fs.read_prefixes);
    let write_prefixes = match_variants_list(&fs.write_prefixes);
    let deny_paths = match_variants_list(&fs.deny_paths);
    let system_read_paths = existing_system_read_paths();
    emit_ancestor_metadata(lines, system_read_paths.iter().map(String::as_str));
    emit_read_subpaths(lines, system_read_paths.iter().map(String::as_str));
    // For each grant prefix, also allow file-read-metadata on its
    // ancestors.  Seatbelt checks parent metadata during lookup;
    // without these, path traversal and posix_spawn can report
    // ENOENT even when the final subpath is allowed.
    emit_ancestor_metadata(
        lines,
        read_prefixes
            .iter()
            .chain(write_prefixes.iter())
            .map(String::as_str),
    );
    for prefix in &read_prefixes {
        lines.push(format!(
            "(allow file-read* (subpath \"{}\"))",
            escape_path(prefix)
        ));
    }
    for prefix in &write_prefixes {
        let escaped = escape_path(prefix);
        lines.push(format!("(allow file-read* (subpath \"{escaped}\"))"));
        lines.push(format!("(allow file-write* (subpath \"{escaped}\"))"));
    }
    deny_paths
}

/// Render the `process-exec` rules.  `Unrestricted` keeps the historic
/// wildcard so plain `grant [fs: …]` blocks don't accidentally lose exec
/// at the OS layer.  `Restricted` emits a single combined `file-read*
/// process-exec` allow over the meet-folded `exec_dirs` and the
/// resolved `[exec]` literals — folded because Seatbelt requires both
/// operations to spawn a binary (read for posix_spawn, then exec) and
/// scattering the read across `system_read_paths` doesn't cover
/// user-installed toolchain dirs like `~/.rustup/.../bin`.
///
/// The `(subpath …)` rules cover any binary under an admitted dir —
/// matching the in-ral gate's `exec_dirs` semantics — which means the
/// OS layer admits e.g. `/usr/bin/cargo` even when `[exec]` doesn't
/// list it.  This is intentional: the OS layer's job here is to close
/// the *interpreter-bypass* class (sh -c, env CMD, xargs CMD, find
/// -exec) by denying paths *outside* the granted dirs/literals.
/// Per-name `Deny` entries inside an admitted dir are handled by the
/// in-ral gate, which still runs first.
///
/// An empty `Restricted` (no allow_paths, no allow_dirs) emits no
/// `(allow process-exec …)` rule at all — deny-default kills every
/// spawn from inside the grant.
fn emit_exec_rules(lines: &mut Vec<String>, exec: &ExecProjection) {
    match exec {
        ExecProjection::Unrestricted => {
            lines.push("(allow process-exec)".to_string());
        }
        ExecProjection::Restricted { allow_paths, allow_dirs, deny_dirs } => {
            // Combined rule covers user policy admits *and* the
            // platform exec base (Apple toolchain dirs).  Folding the
            // base in keeps multi-stage exec chains (`gcc → cc1 → as
            // → ld`) working when the user's `[exec]` only names
            // `/usr/bin/`: Apple's real binaries live under
            // CommandLineTools / Xcode and those would otherwise be
            // exec-denied even though they're readable via
            // `system_paths`.  Same idiom as
            // BrianSwift/macOSSandboxBuild's `confined.sb`.
            let user_dirs = match_variants_list(allow_dirs);
            let system_dirs = existing_system_exec_paths();
            let deny_dirs = match_variants_list(deny_dirs);
            // The bundled-uutils helper re-execs the running binary with
            // `--ral-uutils-helper`; admit its path so `pwd`, `ls`, … work
            // inside every restricted profile without each TOML naming
            // wherever exarch happens to live.
            let self_exec = super::reexec::SANDBOX_SELF
                .get()
                .map(|s| s.exec_path.to_string_lossy().into_owned());
            let mut rule = String::from("(allow file-read* process-exec");
            for path in allow_paths.iter().chain(self_exec.as_ref()) {
                rule.push_str(&format!("\n  (literal \"{}\")", escape_path(path)));
            }
            for dir in user_dirs.iter().chain(system_dirs.iter()) {
                rule.push_str(&format!("\n  (subpath \"{}\")", escape_path(dir)));
            }
            rule.push(')');
            lines.push(rule);
            // Ancestor metadata for the binary literals: posix_spawn
            // walks the parent directories and Seatbelt gates each
            // lookup independently of the final allow on the binary
            // itself.
            emit_ancestor_metadata(
                lines,
                allow_paths
                    .iter()
                    .map(String::as_str)
                    .chain(self_exec.as_deref()),
            );
            // Deny subpaths emitted *after* the broad allow so SBPL's
            // last-match-wins semantics give them precedence.  Both
            // file-read* and process-exec are denied — Seatbelt
            // requires both ops to spawn a binary, so denying read
            // alone would let exec through with EACCES later, but
            // denying exec alone wouldn't stop a read of the binary.
            for dir in &deny_dirs {
                let escaped = escape_path(dir);
                lines.push(format!("(deny file-read* (subpath \"{escaped}\"))"));
                lines.push(format!("(deny process-exec (subpath \"{escaped}\"))"));
            }
        }
    }
}

/// What ops a baseline system path needs to admit.  Every entry is
/// `Read` (libc, dyld, configd, gitconfig, …); toolchain dirs that
/// host real binaries — gcc/clang, ld, as, codesign — are also
/// `Exec`, so a multi-stage chain like `gcc → cc1 → as → ld` runs
/// even when the user's `[exec]` map only names `/usr/bin/`.
///
/// Keeping reads and execs in one tagged list (rather than two
/// parallel constants) keeps the data right next to the comment
/// that explains why each path is here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SystemAccess {
    Read,
    Exec, // implies read; emitted in the folded `file-read* process-exec` rule
}

/// Baseline system paths the runtime needs available regardless of
/// user grant.  `Exec`-tagged entries are folded into the same
/// combined `(allow file-read* process-exec …)` rule the user's
/// `[exec]` admits go into — the idiom from
/// BrianSwift/macOSSandboxBuild's `confined.sb` (`(allow file-read*
/// process-exec (subpath "/bin") (subpath "/usr/bin")
/// (subpath "/Applications/Xcode.app"))`) generalised here so every
/// platform exec subpath comes with a free read.
///
/// User temp/workspace paths are deliberately absent; they must
/// arrive via the active fs grant.
fn system_paths() -> &'static [(&'static str, SystemAccess)] {
    use SystemAccess::*;
    &[
        ("/bin",                                       Exec),
        ("/usr",                                       Exec),
        ("/Library/Apple/usr",                         Exec),
        ("/Library/Developer/CommandLineTools",        Exec),
        ("/Applications/Xcode.app/Contents/Developer", Exec),
        ("/opt/homebrew",                              Exec),
        ("/lib",                                       Read),
        ("/System",                                    Read),
        ("/dev",                                       Read),
        ("/private/var/db/dyld",                       Read),
        // System config under /etc (firmlinked to /private/etc).  Allowed
        // wholesale rather than cherry-picked: tools read whatever they
        // read (gitconfig, paths.d, zshenv, ssh_config, nix.conf, …) and
        // omitting one breaks them mysteriously.  Nothing user-secret
        // lives here on macOS — master.passwd is 0600 and Seatbelt
        // enforces inode permissions on top of the profile.
        ("/private/etc",                               Read),
        // xcode-select state.  /usr/bin/git and the other CommandLineTools
        // shims read /var/select/developer_dir to find the active toolchain;
        // libtool and make also probe /var/select/sh.  Without read access
        // here both fail with "Operation not permitted", which build drivers
        // then misreport as a missing or broken xcode-select install.
        ("/private/var/select",                        Read),
        // configd's runtime state.  /etc/resolv.conf is a symlink to
        // /var/run/resolv.conf, and mDNSResponder's Unix socket lives at
        // /var/run/mDNSResponder, so DNS resolution goes through here.
        // Read-only grant: contents are sockets, PID files, locks — system
        // state, no user secrets.  If DNS still fails, the next missing
        // piece is the socket connect, which needs a separate write rule.
        ("/private/var/run",                           Read),
    ]
}

/// Host-existing system paths admitted for read.  All entries —
/// every `Read` *and* every `Exec` — appear here, since `Exec`
/// implies read.  Each is expanded to its firmlink-equivalent forms
/// (`/private/etc` → `[/etc, /private/etc]`) so the rendered
/// profile matches whichever form Seatbelt presents at MAC-hook
/// time.
fn existing_system_read_paths() -> Vec<String> {
    match_variants_list(&filter_existing(system_paths().iter().map(|(p, _)| *p)))
}

/// Host-existing system paths admitted for exec — the `Exec`-tagged
/// subset of [`system_paths`].  Folded into the combined exec rule
/// alongside user policy admits when exec is `Restricted`.
fn existing_system_exec_paths() -> Vec<String> {
    match_variants_list(&filter_existing(
        system_paths()
            .iter()
            .filter(|(_, k)| *k == SystemAccess::Exec)
            .map(|(p, _)| *p),
    ))
}

fn filter_existing<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    paths
        .into_iter()
        .filter(|p| Path::new(p).exists())
        .map(str::to_string)
        .collect()
}

fn emit_read_subpaths<'a>(lines: &mut Vec<String>, paths: impl IntoIterator<Item = &'a str>) {
    for path in paths {
        lines.push(format!(
            "(allow file-read* (subpath \"{}\"))",
            escape_path(path)
        ));
    }
}

fn emit_ancestor_metadata<'a>(
    lines: &mut Vec<String>,
    paths: impl IntoIterator<Item = &'a str>,
) {
    for ancestor in proper_ancestors(paths) {
        lines.push(format!(
            "(allow file-read-metadata (literal \"{}\"))",
            escape_path(&ancestor)
        ));
    }
}

fn escape_path(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

unsafe extern "C" {
    fn sandbox_init_with_parameters(
        profile: *const c_char,
        flags: u64,
        parameters: *const *const c_char,
        errorbuf: *mut *mut c_char,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::{build_profile, metadata_ancestors};
    use crate::types::{ExecProjection, SandboxProjection};

    #[test]
    fn mac_shell_profile_allows_general_exec_when_unrestricted() {
        let profile = build_profile(&SandboxProjection::default());
        assert!(profile.contains("(allow process-exec)"));
        // The restricted form must not appear when exec is unrestricted.
        assert!(!profile.contains("(allow file-read* process-exec"));
    }

    #[test]
    fn mac_profile_emits_combined_read_exec_rule_when_restricted() {
        let policy = SandboxProjection {
            exec: ExecProjection::Restricted {
                allow_paths: vec!["/usr/bin/git".into()],
                allow_dirs: vec!["/usr/bin".into(), "/opt/homebrew/bin".into()],
                deny_dirs: Vec::new(),
            },
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy);
        // Folded `file-read* process-exec` rule (idiom from
        // BrianSwift/macOSSandboxBuild's confined.sb).
        assert!(
            profile.contains("(allow file-read* process-exec"),
            "missing combined read+exec rule:\n{profile}"
        );
        assert!(profile.contains("(literal \"/usr/bin/git\")"));
        assert!(profile.contains("(subpath \"/usr/bin\")"));
        assert!(profile.contains("(subpath \"/opt/homebrew/bin\")"));
        // The wildcard exec must NOT appear in restricted mode — that's
        // the bypass we're closing.
        assert!(
            !profile.contains("(allow process-exec)\n"),
            "wildcard process-exec leaked into restricted profile"
        );
    }

    /// Apple's toolchain spawns its real binaries from
    /// `/Library/Developer/CommandLineTools/usr/bin` (and on systems
    /// with full Xcode, `/Applications/Xcode.app/...`).  When exec is
    /// restricted, those dirs must be folded into the combined rule
    /// alongside user policy admits — otherwise `gcc → cc1 → as →
    /// ld` dies at the first descendant exec even though `/usr/bin/`
    /// is in the user's `[exec]`.  Mirrors confined.sb's `(subpath
    /// "/Applications/Xcode.app")` line.
    #[test]
    fn mac_profile_folds_toolchain_into_combined_exec_rule_when_restricted() {
        if !std::path::Path::new("/Library/Developer/CommandLineTools").exists() {
            return; // No toolchain on this host; nothing to assert.
        }
        let policy = SandboxProjection {
            exec: ExecProjection::Restricted {
                allow_paths: Vec::new(),
                allow_dirs: vec!["/usr/bin".into()],
                deny_dirs: Vec::new(),
            },
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy);
        // Both the user's admit and the system base appear in one
        // combined rule — confined.sb idiom.
        let combined = profile
            .find("(allow file-read* process-exec")
            .expect("missing combined rule");
        let user = profile[combined..]
            .find("(subpath \"/usr/bin\")")
            .expect("user admit missing from combined rule");
        let toolchain = profile[combined..]
            .find("(subpath \"/Library/Developer/CommandLineTools\")")
            .expect("toolchain not folded into combined rule");
        // No standalone exec allow for the toolchain — it shares the
        // rule with the user's admits, just like confined.sb.
        assert!(user > 0 && toolchain > 0);
    }

    #[test]
    fn mac_profile_emits_only_system_base_when_restricted_to_empty() {
        let policy = SandboxProjection {
            exec: ExecProjection::Restricted {
                allow_paths: Vec::new(),
                allow_dirs: Vec::new(),
                deny_dirs: Vec::new(),
            },
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy);
        // Empty user policy => only the platform exec base admitted.
        // Same shape as `system_read_paths`: an empty user fs grant
        // doesn't deny libc and dyld, and an empty exec map doesn't
        // deny the platform toolchain.  Users wanting full lockdown
        // can subpath-Deny the system roots explicitly.
        assert!(profile.contains("(allow file-read* process-exec"));
        // The wildcard exec must not appear.
        assert!(!profile.contains("(allow process-exec)\n"));
        // No user literals or dirs should appear (none were granted).
        assert!(!profile.contains("(literal \""));
    }

    #[test]
    fn mac_profile_emits_subpath_deny_after_broad_allow() {
        let policy = SandboxProjection {
            exec: ExecProjection::Restricted {
                allow_paths: Vec::new(),
                allow_dirs: vec!["/usr/bin".into()],
                deny_dirs: vec!["/usr/bin/sensitive".into()],
            },
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy);
        let allow_idx = profile
            .find("(allow file-read* process-exec")
            .expect("missing broad allow");
        let deny_exec_idx = profile
            .find("(deny process-exec (subpath \"/usr/bin/sensitive\"))")
            .expect("missing deny process-exec for /usr/bin/sensitive");
        let deny_read_idx = profile
            .find("(deny file-read* (subpath \"/usr/bin/sensitive\"))")
            .expect("missing deny file-read* for /usr/bin/sensitive");
        // Last-match-wins: deny rules must follow the broad allow.
        assert!(allow_idx < deny_read_idx, "deny read must follow allow");
        assert!(allow_idx < deny_exec_idx, "deny exec must follow allow");
    }

    #[test]
    fn mac_profile_denies_network_when_disabled() {
        let profile = build_profile(&SandboxProjection {
            net: false,
            ..SandboxProjection::default()
        });
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn mac_profile_allows_common_dev_writes() {
        let profile = build_profile(&SandboxProjection::default());
        for path in ["/dev/null", "/dev/zero", "/dev/dtracehelper", "/dev/tty"] {
            assert!(
                profile.contains(&format!("(allow file-write* (literal \"{path}\"))")),
                "missing write allowance for {path}"
            );
        }
    }

    #[test]
    fn mac_profile_grants_toolchain_ancestor_metadata() {
        let ancestors = metadata_ancestors(["/Library/Developer/CommandLineTools/usr/bin/ld"]);
        assert!(ancestors.contains(&"/Library".to_string()));
        assert!(ancestors.contains(&"/Library/Developer".to_string()));
        assert!(ancestors.contains(&"/Library/Developer/CommandLineTools/usr/bin".to_string()));
        assert!(!ancestors.contains(&"/".to_string()));
    }

    #[test]
    fn mac_profile_allows_command_line_tools_lookup_when_installed() {
        if !std::path::Path::new("/Library/Developer/CommandLineTools").exists() {
            return;
        }
        let profile = build_profile(&SandboxProjection::default());
        assert!(
            profile
                .contains("(allow file-read* (subpath \"/Library/Developer/CommandLineTools\"))")
        );
        assert!(profile.contains("(allow file-read-metadata (literal \"/Library\"))"));
        assert!(
            profile.contains("(allow file-read-metadata (literal \"/Library/Developer\"))")
        );
    }

    #[test]
    fn mac_profile_does_not_grant_tmp_as_system_read_path() {
        let profile = build_profile(&SandboxProjection::default());
        assert!(!profile.contains("(allow file-read* (subpath \"/tmp\"))"));
        assert!(!profile.contains("(allow file-read* (subpath \"/private/tmp\"))"));
    }

    #[test]
    fn mac_profile_emits_deny_rules_for_deny_paths() {
        use crate::types::FsPolicy;
        // /tmp -> /private/tmp on macOS; both forms must appear so
        // Seatbelt matches whichever the kernel presents at MAC-hook
        // time.  Each deny_paths entry produces file-read*, file-write*
        // and file-link denies (full untouchability), each emitted
        // *after* the covering allow for last-match-wins.
        let policy = SandboxProjection {
            fs: FsPolicy {
                read_prefixes: Vec::new(),
                write_prefixes: vec!["/tmp/work".into()],
                deny_paths: vec!["/tmp/work/.exarch.toml".into()],
            },
            net: true,
        };
        let profile = build_profile(&policy);
        for form in ["/tmp/work", "/private/tmp/work"] {
            let allow_idx = profile
                .find(&format!("(allow file-write* (subpath \"{form}\"))"))
                .unwrap_or_else(|| panic!("write allow for {form} missing"));
            for op in ["file-read*", "file-write*", "file-link"] {
                let deny_idx = profile
                    .find(&format!("(deny {op} (subpath \"{form}/.exarch.toml\"))"))
                    .unwrap_or_else(|| panic!("{op} deny for {form}/.exarch.toml missing"));
                assert!(allow_idx < deny_idx, "{op} deny must follow allow for {form}");
            }
        }
    }

}
