//! macOS sandbox using the Seatbelt (sandbox_init) API.
//!
//! Single mode of operation: a per-command ral re-exec child carrying
//! `--sandbox-projection` enters the Seatbelt profile once at startup via
//! `enter_current_process`, then execs the confined target (a host binary
//! via `--ral-sandbox-exec`, or a bundled tool in-process) inheriting the
//! confinement.
//! `process-exec` is gated when the projection's `exec` field is
//! `Restricted` (see [`emit_exec_rules`] for the folded `file-read*
//! process-exec` rule); `Unrestricted` emits a wildcard `(allow
//! process-exec)` so an fs-only `grant [fs: …]` block does not attenuate
//! exec at the OS layer.
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
use std::fmt::Write;
use std::os::raw::{c_char, c_int};

/// Apply `policy` to the current process.
pub(super) fn enter_current_process(policy: &SandboxProjection) -> Result<(), String> {
    let profile = build_profile(policy);
    apply_profile(&profile).map_err(|e| format!("ral: failed to enter sandbox: {e}"))
}

fn apply_profile(profile: &str) -> std::io::Result<()> {
    fn cstr(s: &str, what: &str) -> std::io::Result<CString> {
        CString::new(s).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{what} contains NUL byte"),
            )
        })
    }
    let profile_cstr = cstr(profile, "sandbox profile")?;
    // No SBPL parameters are used; pass a single null-terminated array.
    let parameter_ptrs: [*const c_char; 1] = [std::ptr::null()];

    let mut errorbuf: *mut c_char = std::ptr::null_mut();
    let rc = unsafe {
        sandbox_init_with_parameters(
            profile_cstr.as_ptr(),
            0,
            parameter_ptrs.as_ptr(),
            &raw mut errorbuf,
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
/// rules live as readable SBPL rather than `format!()`'d strings.
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
    emit_read_subpaths(lines, read_prefixes.iter().map(String::as_str));
    emit_read_subpaths(lines, write_prefixes.iter().map(String::as_str));
    for prefix in &write_prefixes {
        lines.push(format!(
            "(allow file-write* (subpath \"{}\"))",
            escape_path(prefix)
        ));
    }
    deny_paths
}

/// Render the `process-exec` rules.  `Unrestricted` emits a wildcard
/// `(allow process-exec)` so an fs-only `grant [fs: …]` block does not
/// attenuate exec at the OS layer.  `Restricted` emits a single combined `file-read*
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
/// -exec) by denying paths *outside* the granted dirs/literals.  A
/// per-name `Deny` *inside* an admitted dir is enforced here too, via
/// the `deny_basenames` final-component match, so a denied command name
/// cannot be re-execed through the covering subpath by an interpreter
/// the in-ral gate never sees.
///
/// The combined rule is emitted only when at least one filter clause
/// was collected; an admit set that folds to no operand leaves
/// deny-default in force rather than rendering an operand-less rule,
/// which SBPL would read as an unconditional allow.  In practice the
/// platform exec base (`/bin`, `/usr`, the toolchain dirs) is folded
/// in whenever those paths exist, so a user policy with no `[exec]`
/// entries still admits the system binaries.
fn emit_exec_rules(lines: &mut Vec<String>, exec: &ExecProjection) {
    match exec {
        ExecProjection::Unrestricted => {
            lines.push("(allow process-exec)".to_string());
        }
        ExecProjection::Restricted {
            allow_paths,
            allow_dirs,
            deny_paths,
            deny_dirs,
            deny_basenames,
        } => {
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
            // Bundled coreutils / diffutils / ripgrep names dispatch
            // through `--ral-bundled-tool`, which re-execs the running
            // binary so the in-process uutils path can fire inside the
            // child.  ral's own self-path is therefore admitted
            // unconditionally here, so any bundled-tool re-exec works
            // inside every restricted profile without each TOML naming
            // wherever exarch (or any other ral-embedding binary) lives;
            // the per-tool admission gate lives in
            // runtime/command/vet.rs.
            let self_exec = super::reexec::self_exec_path_string();
            let mut clauses = String::new();
            for path in allow_paths.iter().chain(self_exec.as_ref()) {
                let _ = write!(clauses, "\n  (literal \"{}\")", escape_path(path));
            }
            for dir in user_dirs.iter().chain(system_dirs.iter()) {
                let _ = write!(clauses, "\n  (subpath \"{}\")", escape_path(dir));
            }
            // An operand-less `(allow file-read* process-exec)` is an
            // unconditional allow under SBPL — emit the rule only when a
            // filter clause was collected, so an empty admit set leaves
            // deny-default in force rather than opening a wildcard.
            if !clauses.is_empty() {
                lines.push(format!("(allow file-read* process-exec{clauses})"));
            }
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
            // Denies emitted *after* the broad allow so SBPL's
            // last-match-wins semantics give them precedence.  Both
            // file-read* and process-exec are denied — Seatbelt
            // requires both ops to spawn a binary, so denying read
            // alone would let exec through with EACCES later, but
            // denying exec alone wouldn't stop a read of the binary.
            for path in deny_paths {
                let escaped = escape_path(path);
                lines.push(format!("(deny file-read* (literal \"{escaped}\"))"));
                lines.push(format!("(deny process-exec (literal \"{escaped}\"))"));
            }
            for dir in &deny_dirs {
                let escaped = escape_path(dir);
                lines.push(format!("(deny file-read* (subpath \"{escaped}\"))"));
                lines.push(format!("(deny process-exec (subpath \"{escaped}\"))"));
            }
            // Bare-name denies veto a command by its final path component
            // wherever it resolves, so they render as a `/name$` regex
            // rather than one resolved literal.  Only `process-exec` is
            // denied: that alone blocks the spawn, and denying reads of
            // every same-named file would over-reach past the exec veto
            // the gate actually carries.
            for name in deny_basenames {
                let pattern = format!("/{}$", escape_regex(name));
                lines.push(format!("(deny process-exec (regex #\"{pattern}\"))"));
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
/// user grant.  `Exec`-tagged entries are folded into the same combined
/// `(allow file-read* process-exec …)` rule the user's `[exec]` admits
/// go into (see [`emit_exec_rules`]), so every platform exec subpath
/// comes with a free read.
///
/// User temp/workspace paths are deliberately absent; they must
/// arrive via the active fs grant.
fn system_paths() -> &'static [(&'static str, SystemAccess)] {
    use SystemAccess::{Exec, Read};
    &[
        ("/bin", Exec),
        ("/usr", Exec),
        ("/Library/Apple/usr", Exec),
        ("/Library/Developer/CommandLineTools", Exec),
        ("/Applications/Xcode.app/Contents/Developer", Exec),
        ("/opt/homebrew", Exec),
        ("/lib", Read),
        ("/System", Read),
        ("/dev", Read),
        ("/private/var/db/dyld", Read),
        // System config under /etc (firmlinked to /private/etc).  Allowed
        // wholesale rather than cherry-picked: tools read whatever they
        // read (gitconfig, paths.d, zshenv, ssh_config, nix.conf, …) and
        // omitting one breaks them mysteriously.  Nothing user-secret
        // lives here on macOS — master.passwd is 0600 and Seatbelt
        // enforces inode permissions on top of the profile.
        ("/private/etc", Read),
        // xcode-select state.  /usr/bin/git and the other CommandLineTools
        // shims read /var/select/developer_dir to find the active toolchain;
        // libtool and make also probe /var/select/sh.  Without read access
        // here both fail with "Operation not permitted", which build drivers
        // then misreport as a missing or broken xcode-select install.
        ("/private/var/select", Read),
        // configd's runtime state.  /etc/resolv.conf is a symlink to
        // /var/run/resolv.conf, and mDNSResponder's Unix socket lives at
        // /var/run/mDNSResponder, so DNS resolution goes through here.
        // Read-only grant: contents are sockets, PID files, locks — system
        // state, no user secrets.  If DNS still fails, the next missing
        // piece is the socket connect, which needs a separate write rule.
        ("/private/var/run", Read),
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
        .filter(|p| crate::path::exists(p))
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

fn emit_ancestor_metadata<'a>(lines: &mut Vec<String>, paths: impl IntoIterator<Item = &'a str>) {
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

/// Escape a literal command name for embedding in an SBPL `(regex …)`
/// pattern: backslash-escape every metacharacter so a name like `c++`
/// or `python3.11` matches itself, then escape the `"` the pattern
/// string is quoted with.  The name is a bare exec key (no slash — see
/// the decoder), so path separators need no handling.
fn escape_regex(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if r".^$*+?()[]{}|\/".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out.replace('"', "\\\"")
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
    use super::build_profile;
    use crate::path::proper_ancestors;
    use crate::types::{ExecProjection, FsPolicy, FsProjection, SandboxProjection};

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
                deny_paths: Vec::new(),
                deny_dirs: Vec::new(),
                deny_basenames: Vec::new(),
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
        if !crate::path::exists("/Library/Developer/CommandLineTools") {
            return; // No toolchain on this host; nothing to assert.
        }
        let policy = SandboxProjection {
            exec: ExecProjection::Restricted {
                allow_paths: Vec::new(),
                allow_dirs: vec!["/usr/bin".into()],
                deny_paths: Vec::new(),
                deny_dirs: Vec::new(),
                deny_basenames: Vec::new(),
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
                deny_paths: Vec::new(),
                deny_dirs: Vec::new(),
                deny_basenames: Vec::new(),
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
        // No user subpaths or literals leak in (none were granted).
    }

    #[test]
    fn mac_profile_emits_subpath_deny_after_broad_allow() {
        let policy = SandboxProjection {
            exec: ExecProjection::Restricted {
                allow_paths: Vec::new(),
                allow_dirs: vec!["/usr/bin".into()],
                deny_paths: vec!["/usr/bin/git".into()],
                deny_dirs: vec!["/usr/bin/sensitive".into()],
                deny_basenames: Vec::new(),
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
        let deny_git_idx = profile
            .find("(deny process-exec (literal \"/usr/bin/git\"))")
            .expect("missing deny process-exec for /usr/bin/git");
        // Last-match-wins: deny rules must follow the broad allow.
        assert!(allow_idx < deny_read_idx, "deny read must follow allow");
        assert!(allow_idx < deny_exec_idx, "deny exec must follow allow");
        assert!(allow_idx < deny_git_idx, "literal deny must follow allow");
    }

    /// A bare-name deny renders as a `/name$` final-component regex after
    /// the broad allow, so the name is exec-denied wherever it resolves
    /// under an admitted dir — a metacharacter-bearing name (`c++`) is
    /// escaped so it matches itself, not a pattern.
    #[test]
    fn mac_profile_emits_basename_deny_as_final_component_regex() {
        let policy = SandboxProjection {
            exec: ExecProjection::Restricted {
                allow_paths: Vec::new(),
                allow_dirs: vec!["/usr/bin".into()],
                deny_paths: Vec::new(),
                deny_dirs: Vec::new(),
                deny_basenames: vec!["git".into(), "c++".into()],
            },
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy);
        let allow_idx = profile
            .find("(allow file-read* process-exec")
            .expect("missing broad allow");
        let deny_git_idx = profile
            .find(r#"(deny process-exec (regex #"/git$"))"#)
            .expect("missing basename deny for git");
        assert!(
            profile.contains(r#"(deny process-exec (regex #"/c\+\+$"))"#),
            "metacharacters in a denied name must be escaped:\n{profile}"
        );
        assert!(
            allow_idx < deny_git_idx,
            "basename deny must follow the broad allow so last-match-wins"
        );
        // Exec-scoped only: the gate's basename veto never denies reads.
        assert!(
            !profile.contains(r#"(deny file-read* (regex #"/git$"))"#),
            "basename deny must not carve reads:\n{profile}"
        );
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
    fn mac_profile_leaves_tty_ioctl_available_for_tui_children() {
        let profile = build_profile(&SandboxProjection::default());
        assert!(profile.contains("(allow file-ioctl)"));
        assert!(
            !profile.contains("(deny file-ioctl (literal \"/dev/tty\"))"),
            "sandboxed full-screen TUI children need termios/window-size ioctls"
        );
    }

    #[test]
    fn mac_profile_names_notification_center_as_posix_shm() {
        let profile = build_profile(&SandboxProjection::default());
        assert!(
            profile.contains(
                "(allow ipc-posix-shm (ipc-posix-name \"apple.shm.notification_center\"))"
            )
        );
        assert!(
            !profile.contains("(global-name \"apple.shm.notification_center\")"),
            "notification_center is a POSIX shared-memory name, not a Mach service"
        );
    }

    #[test]
    fn mac_profile_grants_toolchain_ancestor_metadata() {
        let ancestors = proper_ancestors(["/Library/Developer/CommandLineTools/usr/bin/ld"]);
        assert!(ancestors.contains(&"/Library".to_string()));
        assert!(ancestors.contains(&"/Library/Developer".to_string()));
        assert!(ancestors.contains(&"/Library/Developer/CommandLineTools/usr/bin".to_string()));
        assert!(!ancestors.contains(&"/".to_string()));
    }

    #[test]
    fn mac_profile_allows_command_line_tools_lookup_when_installed() {
        if !crate::path::exists("/Library/Developer/CommandLineTools") {
            return;
        }
        // System read paths are only emitted explicitly when fs is
        // Restricted (otherwise the wildcard `(allow file-read*)`
        // already covers them).
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy::default()),
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy);
        assert!(
            profile
                .contains("(allow file-read* (subpath \"/Library/Developer/CommandLineTools\"))")
        );
        assert!(profile.contains("(allow file-read-metadata (literal \"/Library\"))"));
        assert!(profile.contains("(allow file-read-metadata (literal \"/Library/Developer\"))"));
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
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: Vec::new(),
                write_prefixes: vec!["/tmp/work".into()],
                deny_paths: vec!["/tmp/work/.exarch.toml".into()],
            }),
            net: true,
            exec: ExecProjection::default(),
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
                assert!(
                    allow_idx < deny_idx,
                    "{op} deny must follow allow for {form}"
                );
            }
        }
    }
}
