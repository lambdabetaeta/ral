//! Windows restricted-token profile-dump advisory.
//!
//! The OS-level confined spawner that this module once hosted has been
//! removed; what remains is the audit-only profile dump that
//! [`dump_profile_for_windows`] renders for `RAL_DUMP_SANDBOX_PROFILE`.
//!
//! **Policy mapping note:** `policy.fs.{read,write,deny}_paths` are
//! *advisory* under this backend — they are surfaced in
//! `dump_profile_for_windows` for audit only.  Network is different: a
//! net-restricting projection (`net: false`) is rejected fail-closed by
//! [`crate::sandbox::projection_enforceable`] before dispatch, so only a
//! `net: allow` projection ever reaches the dump.

use crate::types::SandboxProjection;

/// Render a human-readable dump of the planned restricted-token + Low IL
/// + scratch-dir plan for `RAL_DUMP_SANDBOX_PROFILE`.  Mirrors the
/// Seatbelt SBPL / bwrap argv dumps but for the Win32 path.
pub fn dump_profile_for_windows(policy: &SandboxProjection) -> String {
    let mut out = String::new();
    out.push_str("Restricted-token plan:\n");
    out.push_str("  integrity level: Low (S-1-16-4096)\n");
    out.push_str("  restricting SIDs: [S-1-5-12 RESTRICTED]\n");
    out.push_str(
        "  scratch dir: per-spawn under std::env::temp_dir() as \
ral-sandbox-<pid>-<ns>; stamped Low via SACL\n",
    );
    out.push_str("  mitigation flags:\n");
    out.push_str("    DEP_ENABLE\n");
    out.push_str("    BOTTOM_UP_ASLR_ALWAYS_ON\n");
    out.push_str("    HEAP_TERMINATE_ALWAYS_ON\n");
    out.push_str("    BLOCK_NON_MICROSOFT_BINARIES_ALWAYS_ON\n");
    out.push_str(&format!(
        "  net: {} -- no kernel network enforcement on this backend; a \
net-restricting projection is rejected fail-closed before dispatch (see \
projection_enforceable), so only net: allow reaches here\n",
        if policy.net { "allow" } else { "deny" }
    ));
    out.push_str("  ignored (advisory under this backend):\n");
    if let Some(fs) = policy.fs.as_policy() {
        out.push_str("    fs.read_prefixes (logged only, not enforced as allow-list):\n");
        for p in &fs.read_prefixes {
            out.push_str(&format!("      {}\n", p.as_str()));
        }
        out.push_str(
            "    fs.write_prefixes (logged only, not enforced; only scratch dir writable):\n",
        );
        for p in &fs.write_prefixes {
            out.push_str(&format!("      {}\n", p.as_str()));
        }
        out.push_str("    fs.deny_paths (logged only, not enforced):\n");
        for p in &fs.deny_paths {
            out.push_str(&format!("      {}\n", p.as_str()));
        }
    } else {
        out.push_str("    fs: unrestricted (Low IL still blocks writes to Medium+ objects)\n");
    }
    out
}
