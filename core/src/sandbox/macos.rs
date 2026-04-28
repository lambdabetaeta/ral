//! macOS sandbox using the Seatbelt (sandbox_init) API.
//!
//! Single mode of operation: a ral subprocess spawned by
//! `eval_grant_sandboxed` enters the Seatbelt profile once at startup via
//! `enter_current_process`, then evaluates the grant body in-process with
//! every external it spawns inheriting the confinement.  `process-exec` is
//! allowed without restriction; file and network access are limited by the
//! policy.
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
//! support per-address rules.  `SandboxPolicy::net` is therefore a boolean
//! allow/deny bit, not an endpoint list.

use crate::types::SandboxPolicy;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

pub(super) fn apply_current_process_policy(policy: &SandboxPolicy) -> std::io::Result<()> {
    let profile = build_profile(policy);
    apply_profile(&profile, std::iter::empty::<(&str, &str)>())
}

/// Apply `policy` to the current process and mark the sandbox as active so
/// children inherit the flag and skip re-entry.
pub(super) fn enter_current_process(
    policy: &SandboxPolicy,
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

pub(super) fn build_profile(policy: &SandboxPolicy) -> String {
    let bind_spec = policy.bind_spec();
    let mut lines = vec![
        "(version 1)".to_string(),
        "(deny default)".to_string(),
        "(allow signal (target self))".to_string(),
        "(allow sysctl-read)".to_string(),
        // dyld talks to launchd over Mach to locate the dyld shared
        // cache; without it `(allow process-exec)` is not enough — the
        // child aborts before main().
        "(allow mach-lookup)".to_string(),
        // process-exec without process-fork yields EPERM at spawn time:
        // Seatbelt gates fork() separately from execve().
        "(allow process-fork)".to_string(),
        // Path resolution requires reading the root inode itself, even
        // when every interesting subpath is allowed via (subpath ...).
        // Without this dyld aborts with SIGABRT before main().
        "(allow file-read* (literal \"/\"))".to_string(),
        // Shell-mode entry: every external the sandboxed child spawns
        // inherits this profile, so process-exec is unrestricted; the
        // exec capability check happens in-ral before spawn.
        "(allow process-exec)".to_string(),
    ];

    let system_read_paths = existing_system_read_paths();
    emit_ancestor_metadata(&mut lines, system_read_paths.iter().copied());
    emit_read_subpaths(&mut lines, system_read_paths.iter().copied());
    // Shell redirections and common libc / tooling paths open these device
    // nodes for write.  Without explicit literal allows, `2>/dev/null`
    // and similar patterns fail under Seatbelt even though `/dev` is
    // readable.
    for path in ["/dev/null", "/dev/zero", "/dev/dtracehelper", "/dev/tty"] {
        lines.push(format!(
            "(allow file-write* (literal \"{}\"))",
            escape_path(path)
        ));
    }
    // For each grant prefix, also allow file-read-metadata on its
    // ancestors.  Seatbelt checks parent metadata during lookup; without
    // these, path traversal and posix_spawn can report ENOENT even when
    // the final subpath is allowed.
    emit_ancestor_metadata(
        &mut lines,
        bind_spec
            .read_prefixes
            .iter()
            .chain(bind_spec.write_prefixes.iter())
            .map(String::as_str),
    );

    for prefix in &bind_spec.read_prefixes {
        lines.push(format!(
            "(allow file-read* (subpath \"{}\"))",
            escape_path(prefix)
        ));
    }
    for prefix in &bind_spec.write_prefixes {
        let escaped = escape_path(prefix);
        lines.push(format!("(allow file-read* (subpath \"{escaped}\"))"));
        lines.push(format!("(allow file-write* (subpath \"{escaped}\"))"));
    }
    // Per-file deny rules.  Emitted *after* the broad write allows so
    // Seatbelt's last-match-wins semantics let the deny override.  The
    // `file-link*` deny closes the hardlink/rename hole: writes via a
    // hardlink elsewhere would otherwise hit the same inode.
    for path in &bind_spec.deny_paths {
        let escaped = escape_path(path);
        lines.push(format!("(deny file-write* (literal \"{escaped}\"))"));
        lines.push(format!("(deny file-link* (literal \"{escaped}\"))"));
    }
    if policy.net {
        lines.push("(allow network*)".to_string());
    }

    lines.join("\n")
}

/// Always-readable system paths.  These are not grant policy: they are
/// enough of the platform runtime to let already-authorised executables
/// start, dynamically link, resolve users/hosts, and for C toolchains,
/// spawn their internal tools (`clang` -> `ld`) under Seatbelt.
///
/// User temp/workspace paths are deliberately absent here; they must
/// arrive via the active fs grant.
fn system_read_paths() -> &'static [&'static str] {
    &[
        "/bin",
        "/usr",
        "/lib",
        "/System",
        "/dev",
        "/Library/Apple/usr",
        "/Library/Developer/CommandLineTools",
        "/Applications/Xcode.app/Contents/Developer",
        "/opt/homebrew",
        "/private/var/db/dyld",
        "/var/db/dyld",
        "/private/etc/resolv.conf",
        "/private/etc/hosts",
        "/private/etc/ssl",
        "/private/etc/openssl",
        // getpwuid / getgrgid sit in libc startup paths; without these,
        // many programs can't even resolve $HOME.
        "/private/etc/passwd",
        "/private/etc/group",
        "/private/etc/services",
        "/private/etc/protocols",
        "/private/etc/nsswitch.conf",
    ]
}

fn existing_system_read_paths() -> Vec<&'static str> {
    system_read_paths()
        .iter()
        .copied()
        .filter(|p| std::path::Path::new(p).exists())
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
    for ancestor in metadata_ancestors(paths) {
        lines.push(format!(
            "(allow file-read-metadata (literal \"{}\"))",
            escape_path(&ancestor)
        ));
    }
}

fn metadata_ancestors<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for path in paths {
        for ancestor in std::path::Path::new(path).ancestors().skip(1) {
            if ancestor == std::path::Path::new("/") || ancestor.as_os_str().is_empty() {
                break;
            }
            out.insert(ancestor.to_string_lossy().into_owned());
        }
    }
    out.into_iter().collect()
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
    use crate::types::SandboxPolicy;

    #[test]
    fn mac_shell_profile_allows_general_exec() {
        let profile = build_profile(&SandboxPolicy::default());
        assert!(profile.contains("(allow process-exec)"));
    }

    #[test]
    fn mac_profile_denies_network_when_disabled() {
        let profile = build_profile(&SandboxPolicy {
            net: false,
            ..SandboxPolicy::default()
        });
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn mac_profile_allows_common_dev_writes() {
        let profile = build_profile(&SandboxPolicy::default());
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
        let profile = build_profile(&SandboxPolicy::default());
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
        let profile = build_profile(&SandboxPolicy::default());
        assert!(!profile.contains("(allow file-read* (subpath \"/tmp\"))"));
        assert!(!profile.contains("(allow file-read* (subpath \"/private/tmp\"))"));
    }

    #[test]
    fn mac_profile_emits_deny_rules_for_deny_paths() {
        use crate::types::FsPolicy;
        let policy = SandboxPolicy {
            fs: FsPolicy {
                read_prefixes: Vec::new(),
                write_prefixes: vec!["/tmp/work".into()],
                deny_paths: vec!["/tmp/work/.exarch.toml".into()],
            },
            net: true,
        };
        let profile = build_profile(&policy);
        let allow_idx = profile
            .find("(allow file-write* (subpath \"/tmp/work\"))")
            .expect("write allow rendered");
        let deny_w_idx = profile
            .find("(deny file-write* (literal \"/tmp/work/.exarch.toml\"))")
            .expect("write deny rendered");
        let deny_l_idx = profile
            .find("(deny file-link* (literal \"/tmp/work/.exarch.toml\"))")
            .expect("link deny rendered");
        assert!(allow_idx < deny_w_idx, "deny must come after allow for last-match-wins");
        assert!(allow_idx < deny_l_idx);
    }
}
