//! macOS sandbox using the Seatbelt (`sandbox_init`) API.
//!
//! Only the per-command re-exec child is confined: it carries
//! `--sandbox-projection`, enters the profile at startup through
//! `enter_current_process`, then execs the target — a host binary via
//! `--ral-sandbox-exec`, or a bundled tool in-process — which inherits the
//! confinement.  The parent ral process is never confined; authorising a
//! binary through `exec:` already shifts trust to that binary.
//!
//! Seatbelt has no per-address network rules, so `SandboxProjection::net` is
//! one allow/deny bit rather than an endpoint list.

use crate::path::{match_variants_list, proper_ancestors};
use crate::types::{ExecProjection, FsProjection, SandboxBindSpec, SandboxProjection};
use std::ffi::{CStr, CString};
use std::fmt::Write;
use std::os::raw::{c_char, c_int};

/// Apply `policy` to the current process.  Seatbelt entry cannot be undone.
pub(super) fn enter_current_process(policy: &SandboxProjection) -> Result<(), String> {
    let profile = build_profile(policy)?;
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
    // No SBPL parameters: a lone null terminator.
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

/// Policy-independent SBPL preamble: `(version 1)`, `(deny default)`, and the
/// Apple-required carve-outs.  A sibling file so the rules read as SBPL rather
/// than `format!()` strings.
const BASE_PROFILE: &str = include_str!("macos-base.sbpl");

pub(super) fn build_profile(policy: &SandboxProjection) -> Result<String, String> {
    let mut lines: Vec<String> = vec![BASE_PROFILE.to_string()];
    let bind_spec = policy.bind_spec();
    let deny_paths = match &policy.fs {
        FsProjection::Restricted(_) => emit_fs_restricted(&mut lines, &bind_spec)?,
        FsProjection::Unrestricted => {
            // Pass fs through, so an exec-only grant can enter the sandbox
            // for exec gating without clamping the agent's cwd or HOME.
            lines.push("(allow file-read*)".to_string());
            lines.push("(allow file-write*)".to_string());
            Vec::new()
        }
    };

    emit_exec_rules(&mut lines, &policy.exec)?;

    // After the broad allows: Seatbelt is last-match-wins.  `subpath` so a
    // denied directory covers everything under it; `file-link` (Seatbelt
    // has no `file-link*`) blocks `link(2)` against the source, closing the
    // hole where a second name elsewhere would let writes bypass the deny.
    for path in &deny_paths {
        let escaped = escape_path(path);
        lines.push(format!("(deny file-read* (subpath \"{escaped}\"))"));
        lines.push(format!("(deny file-write* (subpath \"{escaped}\"))"));
        lines.push(format!("(deny file-link (subpath \"{escaped}\"))"));
    }
    // A pinned dir keeps its entries mutable — only its own name-in-parent is
    // frozen — so `literal`, never `subpath`, which would also block
    // unlinking every entry inside it.  Unconditional on existence, like the
    // deny paths above: an ancestor absent now can be created later under the
    // write prefix that covers it.
    for dir in match_variants_list(&bind_spec.pinned_dirs)? {
        lines.push(format!(
            "(deny file-write-unlink (literal \"{}\"))",
            escape_path(&dir)
        ));
    }
    if policy.net {
        lines.push("(allow network*)".to_string());
    }

    Ok(lines.join("\n"))
}

/// Emit the allow rules and ancestor-metadata carve-outs for a restricted fs
/// bind spec, returning the expanded `deny_paths` for the caller to layer
/// after every allow (Seatbelt is last-match-wins).
///
/// `Err` when a prefix's firmlink expansion is not valid UTF-8, which
/// [`crate::path::match_variants_list`] refuses rather than approximates.
fn emit_fs_restricted(
    lines: &mut Vec<String>,
    bind_spec: &SandboxBindSpec,
) -> Result<Vec<String>, String> {
    let read_prefixes = match_variants_list(&bind_spec.read_prefixes)?;
    let write_prefixes = match_variants_list(&bind_spec.write_prefixes)?;
    let deny_paths = match_variants_list(&bind_spec.deny_paths)?;
    let system_read_paths = existing_system_read_paths()?;
    emit_ancestor_metadata(lines, system_read_paths.iter().map(String::as_str));
    emit_read_subpaths(lines, system_read_paths.iter().map(String::as_str));
    // Seatbelt checks parent metadata during lookup; without these, traversal
    // and posix_spawn report ENOENT even where the final subpath is allowed.
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
    Ok(deny_paths)
}

/// Render the `process-exec` rules.  `Unrestricted` emits a wildcard so an
/// fs-only `grant [fs: …]` block does not attenuate exec at the OS layer.
/// `Restricted` folds the resolved `[exec]` literals and `allow_dirs` into one
/// `file-read* process-exec` allow: Seatbelt requires both operations to spawn
/// a binary, and the reads granted by `system_paths` miss user-installed
/// toolchain dirs like `~/.rustup/.../bin`.
///
/// `(subpath …)` admits every binary under an admitted dir, so this layer is
/// deliberately coarser than the in-ral gate — its job is to close the
/// interpreter-bypass class (`sh -c`, `env`, `xargs`, `find -exec`) by denying
/// what lies *outside* the granted dirs.  `deny_basenames` still vetoes a name
/// inside one, so a denied command cannot be re-execed through the covering
/// subpath by an interpreter the gate never sees.
fn emit_exec_rules(lines: &mut Vec<String>, exec: &ExecProjection) -> Result<(), String> {
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
            // The platform exec base folds in alongside the user's admits:
            // Apple's real binaries live under CommandLineTools / Xcode, so a
            // chain like `gcc → cc1 → as → ld` would die at the first
            // descendant exec when `[exec]` names only `/usr/bin/`.
            let user_dirs = match_variants_list(allow_dirs)?;
            let system_dirs = existing_system_exec_paths()?;
            let deny_dirs = match_variants_list(deny_dirs)?;
            // Bundled tools dispatch through `--ral-bundled-tool`, which
            // re-execs the running binary.  Admitting our own path
            // unconditionally spares every policy from naming wherever the
            // embedding binary lives; the per-tool admission gate is `vet` in
            // `core/src/runtime/command/vet.rs`.
            let self_exec = super::reexec::self_exec_path_string();
            let mut clauses = String::new();
            for path in allow_paths.iter().chain(self_exec.as_ref()) {
                let _ = write!(clauses, "\n  (literal \"{}\")", escape_path(path));
            }
            for dir in user_dirs.iter().chain(system_dirs.iter()) {
                let _ = write!(clauses, "\n  (subpath \"{}\")", escape_path(dir));
            }
            // An operand-less `(allow file-read* process-exec)` is an
            // unconditional allow under SBPL, so an empty admit set must
            // emit nothing and leave deny-default in force.
            if !clauses.is_empty() {
                lines.push(format!("(allow file-read* process-exec{clauses})"));
            }
            // posix_spawn walks the parent directories and Seatbelt gates
            // each lookup independently of the allow on the binary itself.
            emit_ancestor_metadata(
                lines,
                allow_paths
                    .iter()
                    .map(String::as_str)
                    .chain(self_exec.as_deref()),
            );
            // After the broad allow: last-match-wins.  Both ops are denied —
            // read alone would let the exec through to fail later, exec
            // alone would leave the binary readable.
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
            // A bare-name deny vetoes the command wherever it resolves, hence
            // a final-component regex rather than one literal.  Exec only:
            // denying reads of every same-named file would over-reach past
            // the veto the gate carries.
            for name in deny_basenames {
                let pattern = format!("/{}$", escape_regex(name));
                lines.push(format!("(deny process-exec (regex #\"{pattern}\"))"));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SystemAccess {
    Read,
    Exec, // implies read; emitted in the folded `file-read* process-exec` rule
}

/// Baseline system paths the runtime needs regardless of user grant.  User
/// temp and workspace paths are deliberately absent — they must arrive via
/// the active fs grant.
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
        // Wholesale rather than cherry-picked: tools read whatever they read
        // (gitconfig, paths.d, zshenv, nix.conf, …) and omitting one breaks
        // them mysteriously.  Nothing user-secret lives here — master.passwd
        // is 0600, and Seatbelt enforces inode permissions atop the profile.
        ("/private/etc", Read),
        // xcode-select state: the CommandLineTools shims read
        // /var/select/developer_dir, libtool and make probe /var/select/sh.
        // Denied, build drivers misreport the EPERM as a broken install.
        ("/private/var/select", Read),
        // /etc/resolv.conf symlinks to /var/run/resolv.conf and
        // mDNSResponder's socket lives at /var/run/mDNSResponder, so DNS
        // resolution goes through here.
        ("/private/var/run", Read),
    ]
}

/// Host-existing system paths admitted for read — every entry, since `Exec`
/// implies read.  Each expands to its firmlink-equivalent forms (`/private/etc`
/// → `[/etc, /private/etc]`), matching whichever spelling Seatbelt presents.
fn existing_system_read_paths() -> Result<Vec<String>, String> {
    match_variants_list(&filter_existing(system_paths().iter().map(|(p, _)| *p)))
}

/// The `Exec`-tagged subset, folded into the combined exec rule alongside
/// the user's admits when exec is `Restricted`.
fn existing_system_exec_paths() -> Result<Vec<String>, String> {
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

/// Escape a command name for an SBPL `(regex …)` pattern, so a name like
/// `c++` or `python3.11` matches itself instead of acting as a pattern.
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
        let profile = build_profile(&SandboxProjection::default()).unwrap();
        assert!(profile.contains("(allow process-exec)"));
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
        let profile = build_profile(&policy).unwrap();
        assert!(
            profile.contains("(allow file-read* process-exec"),
            "missing combined read+exec rule:\n{profile}"
        );
        assert!(profile.contains("(literal \"/usr/bin/git\")"));
        assert!(profile.contains("(subpath \"/usr/bin\")"));
        assert!(profile.contains("(subpath \"/opt/homebrew/bin\")"));
        assert!(
            !profile.contains("(allow process-exec)\n"),
            "wildcard process-exec leaked into restricted profile"
        );
    }

    /// Apple spawns its real binaries from `CommandLineTools` (or Xcode.app),
    /// so those dirs must fold into the combined rule: otherwise `gcc → cc1 →
    /// as → ld` dies at the first descendant exec even with `/usr/bin/` in
    /// `[exec]`.
    #[test]
    fn mac_profile_folds_toolchain_into_combined_exec_rule_when_restricted() {
        if !crate::path::exists("/Library/Developer/CommandLineTools") {
            return; // No toolchain on this host.
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
        let profile = build_profile(&policy).unwrap();
        let combined = profile
            .find("(allow file-read* process-exec")
            .expect("missing combined rule");
        let user = profile[combined..]
            .find("(subpath \"/usr/bin\")")
            .expect("user admit missing from combined rule");
        let toolchain = profile[combined..]
            .find("(subpath \"/Library/Developer/CommandLineTools\")")
            .expect("toolchain not folded into combined rule");
        // Both fall inside the combined rule; the toolchain gets no allow of
        // its own.
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
        let profile = build_profile(&policy).unwrap();
        // An empty exec map still admits the platform base, just as an empty
        // fs grant still admits libc and dyld.
        assert!(profile.contains("(allow file-read* process-exec"));
        assert!(!profile.contains("(allow process-exec)\n"));
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
        let profile = build_profile(&policy).unwrap();
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
        assert!(allow_idx < deny_read_idx, "deny read must follow allow");
        assert!(allow_idx < deny_exec_idx, "deny exec must follow allow");
        assert!(allow_idx < deny_git_idx, "literal deny must follow allow");
    }

    /// A bare-name deny renders as a `/name$` regex after the broad allow, so
    /// the name is exec-denied wherever it resolves under an admitted dir,
    /// with metacharacters escaped so `c++` matches itself.
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
        let profile = build_profile(&policy).unwrap();
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
        })
        .unwrap();
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn mac_profile_allows_common_dev_writes() {
        let profile = build_profile(&SandboxProjection::default()).unwrap();
        for path in ["/dev/null", "/dev/zero", "/dev/dtracehelper", "/dev/tty"] {
            assert!(
                profile.contains(&format!("(allow file-write* (literal \"{path}\"))")),
                "missing write allowance for {path}"
            );
        }
    }

    #[test]
    fn mac_profile_leaves_tty_ioctl_available_for_tui_children() {
        let profile = build_profile(&SandboxProjection::default()).unwrap();
        assert!(profile.contains("(allow file-ioctl)"));
        assert!(
            !profile.contains("(deny file-ioctl (literal \"/dev/tty\"))"),
            "sandboxed full-screen TUI children need termios/window-size ioctls"
        );
    }

    #[test]
    fn mac_profile_names_notification_center_as_posix_shm() {
        let profile = build_profile(&SandboxProjection::default()).unwrap();
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
        // System read paths are emitted explicitly only when fs is
        // Restricted; otherwise the wildcard `(allow file-read*)` covers them.
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy::default()),
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy).unwrap();
        assert!(
            profile
                .contains("(allow file-read* (subpath \"/Library/Developer/CommandLineTools\"))")
        );
        assert!(profile.contains("(allow file-read-metadata (literal \"/Library\"))"));
        assert!(profile.contains("(allow file-read-metadata (literal \"/Library/Developer\"))"));
    }

    #[test]
    fn mac_profile_does_not_grant_tmp_as_system_read_path() {
        let profile = build_profile(&SandboxProjection::default()).unwrap();
        assert!(!profile.contains("(allow file-read* (subpath \"/tmp\"))"));
        assert!(!profile.contains("(allow file-read* (subpath \"/private/tmp\"))"));
    }

    #[test]
    fn mac_profile_emits_deny_rules_for_deny_paths() {
        use crate::types::FsPolicy;
        // /tmp firmlinks to /private/tmp, so both spellings must appear, and
        // each of the three denies must follow its covering allow.
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: Vec::new(),
                write_prefixes: vec![crate::path::NormalizedPrefix::from_surface("/tmp/work")],
                deny_paths: vec![crate::path::NormalizedPrefix::from_surface(
                    "/tmp/work/.exarch.toml",
                )],
            }),
            net: true,
            exec: ExecProjection::default(),
        };
        let profile = build_profile(&policy).unwrap();
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

    /// The write prefix root and the intermediate `.ssh` directory both get
    /// pinned against rename/unlink, each in both firmlink spellings, after
    /// the write allow that covers them — the fix for the `mv /repo/.ssh
    /// /repo/x` and `mv /repo /scratch/r` escapes.
    #[test]
    fn mac_profile_emits_pin_rules_for_deny_ancestors() {
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: Vec::new(),
                write_prefixes: vec![crate::path::NormalizedPrefix::from_surface("/tmp/work")],
                deny_paths: vec![crate::path::NormalizedPrefix::from_surface(
                    "/tmp/work/.ssh/id_rsa",
                )],
            }),
            net: true,
            exec: ExecProjection::default(),
        };
        let profile = build_profile(&policy).unwrap();
        for form in ["/tmp/work", "/private/tmp/work"] {
            let allow_idx = profile
                .find(&format!("(allow file-write* (subpath \"{form}\"))"))
                .unwrap_or_else(|| panic!("write allow for {form} missing"));
            for dir in [form.to_string(), format!("{form}/.ssh")] {
                let pin_idx = profile
                    .find(&format!("(deny file-write-unlink (literal \"{dir}\"))"))
                    .unwrap_or_else(|| panic!("pin for {dir} missing:\n{profile}"));
                assert!(
                    allow_idx < pin_idx,
                    "pin for {dir} must follow the write allow"
                );
            }
        }
        assert!(
            !profile.contains("(deny file-write-unlink (subpath"),
            "pins must be literal, not subpath — subpath would also block \
             unlinking every entry inside the pinned directory"
        );
    }
}
