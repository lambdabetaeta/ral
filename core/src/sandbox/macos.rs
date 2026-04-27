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

    // Always-readable system paths.  /private/etc is intentionally excluded;
    // only specific networking-related files are allowed below.  /private/var
    // is needed for the dyld shared cache (/private/var/db/dyld on older macOS)
    // and per-user temp directories (/private/var/folders).
    for path in [
        "/bin",
        "/usr",
        "/lib",
        "/System",
        "/dev",
        "/tmp",
        "/private/tmp",
        "/var",
        "/private/var",
        "/private/etc/resolv.conf",
        "/private/etc/hosts",
        "/private/etc/ssl",
        "/private/etc/openssl",
    ] {
        lines.push(format!("(allow file-read* (subpath \"{path}\"))"));
    }
    // For each grant prefix, also allow file-read-metadata on its ancestor
    // directories so that path-traversal tools (e.g. the `glob` crate) can
    // stat intermediate path components without being granted full read access
    // to their contents.  Without this, `glob "/a/b/c/*"` silently returns []
    // when Seatbelt blocks `stat("/a")` even though `(subpath "/a/b/c")` is
    // allowed.
    let all_prefixes = bind_spec.read_prefixes.iter()
        .chain(bind_spec.write_prefixes.iter());
    let mut ancestor_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for prefix in all_prefixes {
        for ancestor in std::path::Path::new(prefix).ancestors().skip(1) {
            if ancestor == std::path::Path::new("/") || ancestor.as_os_str().is_empty() {
                break;
            }
            ancestor_set.insert(ancestor.to_string_lossy().into_owned());
        }
    }
    for ancestor in &ancestor_set {
        lines.push(format!(
            "(allow file-read-metadata (literal \"{}\"))",
            escape_path(ancestor)
        ));
    }

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
    if policy.net {
        lines.push("(allow network*)".to_string());
    }

    lines.join("\n")
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
    use super::build_profile;
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
}
