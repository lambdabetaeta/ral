// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

//! Persistent capability-ACE stamping for the `AppContainer` sandbox backend.
//!
//! Authority here is path-keyed, not session-keyed. Every `(path, kind)` a
//! projection grants names a capability SID derived from the canonical path
//! ([`fs_capability_name`]); [`ensure_fs_grant`] stamps that SID's ACE onto
//! the prefix, and a child reaches the path only if `session` put the
//! capability in its `LowBox` token. The stamp is the expensive part — one
//! security write (`SetNamedSecurityInfoW`, or `SetSecurityInfo` through a
//! held handle), whose inheritable ACE Windows propagates into every existing
//! descendant before returning, seconds on a build tree — so it is paid once
//! per `(path, kind)` *ever*, across sessions and projections, and never
//! reverted. Leaving it is safe: a capability SID is
//! matched only in the `AppContainer` pass of the access check, whose result
//! intersects the normal pass, so an ACE no live token names is inert and can
//! never widen any process's reach beyond the owning user's own.
//!
//! The capability ACE alone does not admit *creating* an entry: Windows runs
//! the mandatory-integrity check before the `AppContainer` pass, and an
//! unlabeled directory refuses every `Low`-IL `LowBox` child's create
//! regardless of the DACL. A read-write grant on a directory therefore also
//! stamps a `SYSTEM_MANDATORY_LABEL_ACE` at `Low` ([`ensure_low_integrity_label`])
//! — its own permanent mutation, witnessed the same way under its own
//! stamp-key namespace. The label is container-inherit only, never
//! object-inherit: Windows caps a process at the label on its image file, so a
//! `Low` label reaching files would run every host-built executable under the
//! tree at `Low`. Files stay unlabeled, and the capability ACE alone admits
//! writing an existing one.
//!
//! Two witnesses must agree before a stamp is skipped: the stamp store (a
//! completion set beside the ledgers) says the propagation ran to completion,
//! and a probe of the root's own DACL says the tree it completed on is still
//! this one. A crash mid-propagation leaves no store entry, so the next grant
//! re-stamps; a granted root deleted and recreated fails the probe, same
//! outcome; and in either interim the child fails closed.
//!
//! The honest limit of the primitive: an ACE lives on the NTFS *object*, so
//! authority is prefix-shaped at stamp time and object-sticky after. A file
//! renamed into a granted tree (same volume) keeps its old descriptor and
//! stays dark — fail-closed; one renamed *out* carries the inherited grant
//! with it — fail-open, for exactly the tokens that already held the tree.
//! Both drifts exist under session-scoped stamping too, since Windows never
//! re-inherits on rename; persistence extends the second in time rather than
//! creating it. The trade is recorded, with the mitigations weighed, in
//! `docs/ral-wiki/decisions/260730_path-derived-capability-sids.md`.
//!
//! What still needs undoing is the profile. `DaclManager` ledgers
//! `AppContainer` profile registrations before the OS-level create, and
//! [`recover_orphaned_state`] sweeps a crashed session's ledger at the next
//! boot, deleting its registered profiles.

use crate::path::shown;
use crate::process::cancel::{CancelCause, Scope};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};

mod acl;
mod deny;
#[cfg(test)]
pub(super) mod deny_tests;
mod frozen;
mod ledger;
mod mutex;
mod stamp;
#[cfg(test)]
mod tests;

use acl::{
    AceType, OICI_ACE_FLAGS, Target, apply_explicit_ace, apply_mandatory_label_ace,
    low_integrity_label_present, scan_explicit_aces_for_sid,
};
pub(crate) use frozen::{FrozenPin, needing_grant, pin_all};
pub(crate) use ledger::{DaclManager, recover_orphaned_state};
use mutex::PathMutexGuard;
use stamp::{label_stamp_key, record_stamp, stamp_key, stamp_recorded, update_stamp};

/// Rights stamped on a read-write prefix. `FILE_GENERIC_EXECUTE` is there for
/// `FILE_TRAVERSE`, without which the child cannot `SetCurrentDirectoryW` in;
/// NTFS cannot grant the one without the other, so inheritance carries
/// `FILE_EXECUTE` down to every file — accepted, since the `AppContainer`
/// already sandboxes code execution.
pub(crate) const RW_MASK: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

/// Rights stamped on a read-only prefix; execute is there for the same
/// `FILE_TRAVERSE` reason as [`RW_MASK`].
pub(crate) const RO_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// Rights a deny stamp subtracts: the whole `FILE_ALL_ACCESS` surface
/// (`STANDARD_RIGHTS_REQUIRED | SYNCHRONIZE | 0x1FF`). Explicit deny precedes
/// inherited allow in canonical ACL order, so a deny nested inside a granted
/// prefix beats the enclosing grant.
pub(crate) const DENY_MASK: u32 = 0x001F_01FF;

/// The three access shapes a grant stamps. Each keys its own capability name
/// and its own stamp-store entry, so read-only coverage never satisfies a
/// read-write request, and a deny is opted into per token rather than shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GrantKind {
    ReadWrite,
    ReadOnly,
    Deny,
}

impl GrantKind {
    fn mask(self) -> u32 {
        match self {
            Self::ReadWrite => RW_MASK,
            Self::ReadOnly => RO_MASK,
            Self::Deny => DENY_MASK,
        }
    }

    fn ace_type(self) -> AceType {
        match self {
            Self::ReadWrite | Self::ReadOnly => AceType::Allow,
            Self::Deny => AceType::Deny,
        }
    }

    /// The capability-name segment, doubling as the stamp-store key prefix.
    fn tag(self) -> &'static str {
        match self {
            Self::ReadWrite => "rw",
            Self::ReadOnly => "ro",
            Self::Deny => "deny",
        }
    }
}

const _: () = {
    assert!(RW_MASK == FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE);
    assert!(RO_MASK == FILE_GENERIC_READ | FILE_GENERIC_EXECUTE);
    assert!(
        (RW_MASK & RO_MASK) == RO_MASK,
        "RW must be a superset of RO"
    );
    // FILE_TRAVERSE and FILE_EXECUTE are both 0x20, inside FILE_GENERIC_EXECUTE.
    assert!(RW_MASK & 0x20 == 0x20, "RW must grant FILE_TRAVERSE");
    assert!(RO_MASK & 0x20 == 0x20, "RO must grant FILE_TRAVERSE");
};

/// Errors returned by [`DaclManager`] and [`recover_orphaned_state`].
#[derive(Debug)]
pub(crate) enum DaclError {
    NetworkPathRejected(PathBuf),
    PathNotFound(PathBuf),
    /// A frozen allowance whose path now meets a reparse point at `at`,
    /// which is `granted` itself or an ancestor of it.
    Redirected {
        granted: PathBuf,
        at: PathBuf,
        target: Option<PathBuf>,
    },
    WriteDacDenied {
        path: PathBuf,
        reason: String,
    },
    Win32 {
        path: PathBuf,
        reason: String,
    },
    LedgerIo(io::Error),
    LedgerParse(String),
    InvalidSid(String),
    MutexTimeout {
        path: PathBuf,
        timeout_ms: u32,
    },
    /// A cancel landed between two paths' stamps.  Not a failure: stamps
    /// already applied are persistent by design, and one interrupted before
    /// its apply left no witness, so the next grant simply re-runs it.
    Cancelled(CancelCause),
}

/// Whether a reparse target is a volume GUID path, as a mount point's is.
fn volume_mount(target: &Path) -> bool {
    let t = target.to_string_lossy();
    ["\\??\\", "\\\\?\\"].iter().any(|p| {
        t.strip_prefix(p)
            .and_then(|r| r.get(..7))
            .is_some_and(|v| v.eq_ignore_ascii_case("Volume{"))
    })
}

impl std::fmt::Display for DaclError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NetworkPathRejected(p) => write!(
                f,
                "path is not local (network/UNC paths not supported): {}",
                shown(p)
            ),
            Self::PathNotFound(p) => write!(f, "path does not exist: {}", shown(p)),
            Self::Redirected {
                granted,
                at,
                target: Some(t),
            } if volume_mount(t) => write!(
                f,
                "'{}' was granted by its real path, but '{}' is where a volume is mounted; \
                 a volume mounted in a folder cannot be granted on Windows (it cannot be \
                 told from a junction). Would giving the volume a drive letter and \
                 granting that do?",
                shown(granted),
                shown(at)
            ),
            Self::Redirected {
                granted,
                at,
                target,
            } => {
                write!(
                    f,
                    "'{}' was granted by its real path, but ",
                    shown(granted)
                )?;
                match target {
                    Some(t) => write!(f, "'{}' is now a link to '{}'", shown(at), shown(t))?,
                    None => write!(f, "'{}' is now a reparse point", shown(at))?,
                }
                f.write_str(
                    "; an allowance names what it named when granted. To allow the new \
                     target, grant it where the ceiling is set (in exarch, restart with a \
                     grant that names it)",
                )
            }
            Self::WriteDacDenied { path, reason } => {
                write!(f, "WRITE_DAC denied on {}: {reason}", shown(path))
            }
            Self::Win32 { path, reason } => {
                write!(f, "Win32 error on {}: {reason}", shown(path))
            }
            Self::LedgerIo(e) => write!(f, "DACL ledger I/O error: {e}"),
            Self::LedgerParse(s) => write!(f, "DACL ledger parse error: {s}"),
            Self::InvalidSid(s) => write!(f, "invalid SID string: {s}"),
            Self::MutexTimeout { path, timeout_ms } => write!(
                f,
                "timed out acquiring DACL mutex on {} after {timeout_ms} ms",
                shown(path)
            ),
            Self::Cancelled(cause) => write!(f, "{}", cause.message()),
        }
    }
}

impl std::error::Error for DaclError {}

impl From<io::Error> for DaclError {
    fn from(e: io::Error) -> Self {
        Self::LedgerIo(e)
    }
}

/// The stamp loops' poll point: a chain of atomic loads, so it costs nothing
/// against the security write it guards.
fn cancelled(cancel: &Scope) -> Result<(), DaclError> {
    match cancel.cause() {
        Some(cause) => Err(DaclError::Cancelled(cause)),
        None => Ok(()),
    }
}

/// Canonicalize a grant target the way stamps and capability names key it:
/// `canonicalise_strict`, then local-only — a UNC path has no DACL we may
/// write and no volume the marker could witness.
pub(crate) fn canonicalize_grant_target(path: &Path) -> Result<PathBuf, DaclError> {
    canonicalize_local(path)
}

/// The capability name whose derived SID carries one `(path, kind)` grant.
/// Deterministic across sessions and machines — that is what lets a stamp be
/// paid once and matched forever — and collision-resistant, since two paths
/// sharing a name would silently merge their authority: the name embeds 128
/// bits of the canonical path's SHA-256, short enough to stay well inside any
/// capability-name length the OS might enforce. The canonical spelling is
/// hashed as-is, not case-folded: canonicalisation already yields one
/// spelling per object, and folding would merge two genuinely distinct names
/// in an NTFS case-sensitive directory into one authority.
pub(crate) fn fs_capability_name(canonical: &Path, kind: GrantKind) -> String {
    let key = canonical.to_string_lossy();
    let digest = <sha2::Sha256 as sha2::Digest>::digest(key.as_bytes());
    let mut name = format!("ral.fs.{}.", kind.tag());
    for b in &digest[..16] {
        let _ = write!(name, "{b:02x}");
    }
    name
}

/// Ensure the persistent ACE for `(canonical, kind)` under `sid_str` — the
/// SID [`fs_capability_name`] derives to — and, for a read-write grant, the
/// mandatory label alongside it when the target is a directory (see
/// [`ensure_low_integrity_label`]).
///
/// A read-write grant is two mutations, so each answers for itself and
/// neither may speak for the other: the label is asked *before* the ACE's
/// witness is consulted, since a witness that only knows the DACL would
/// otherwise wave through as complete a grant some earlier ral stamped when
/// the label half did not yet exist — and, nothing here reverting, that path
/// would stay unwritable for good with no witness able to say why. The ACE's
/// own skip stands when both of its witnesses hold: the stamp store records a
/// completed propagation, and the root's own DACL still carries it. `cancel` is
/// polled between descendants of a deny walk; Win32's own propagation is
/// not interruptible. A deny's witness includes that walk, since inheritance
/// cannot cross protected DACLs or outrank explicit entries.
pub(crate) fn ensure_fs_grant(
    canonical: &Path,
    sid_str: &str,
    kind: GrantKind,
    cancel: &Scope,
) -> Result<(), DaclError> {
    ensure_grant(Target::Named(canonical), sid_str, kind, cancel)
}

/// [`ensure_fs_grant`] for a frozen allowance, applied through the handle
/// [`FrozenPin`] holds: the object checked is the object stamped. Allows only; a
/// deny is re-resolved at every launch. A stamp already recorded needs no
/// `WRITE_DAC`: only the write that would be new fails, as `WriteDacDenied`.
pub(crate) fn ensure_frozen_grant(
    pin: &FrozenPin,
    sid_str: &str,
    kind: GrantKind,
    cancel: &Scope,
) -> Result<(), DaclError> {
    debug_assert!(kind != GrantKind::Deny, "a deny is stamped by name");
    ensure_grant(pin.target(), sid_str, kind, cancel)
}

fn ensure_grant(
    target: Target<'_>,
    sid_str: &str,
    kind: GrantKind,
    cancel: &Scope,
) -> Result<(), DaclError> {
    cancelled(cancel)?;
    let canonical = target.path();
    let inheritable = target.is_dir().map_err(|e| DaclError::Win32 {
        path: canonical.to_path_buf(),
        reason: format!("metadata: {e}"),
    })?;
    // A capability ACE alone cannot admit a create: the mandatory-integrity
    // check runs first, and a `Low`-IL child clears it only if the directory's
    // own label is `Low` or below. Read-only and deny grants need no label —
    // `Low` already reads `Medium` under the default no-write-up-only policy —
    // and neither does a file, whose write the capability ACE alone admits.
    if kind == GrantKind::ReadWrite && inheritable {
        ensure_low_integrity_label(target)?;
    }
    let key = stamp_key(canonical, kind);
    // Serialize the witness with the mutation, including an interrupted retry.
    let _guard = PathMutexGuard::acquire(canonical)?;
    if stamp_recorded(&key) && capability_ace_present(target, sid_str, kind, inheritable) {
        return Ok(());
    }
    // A recreated root must not resurrect its old completion witness mid-apply.
    update_stamp(&key, false)?;
    let _deny_root = if kind == GrantKind::Deny && inheritable {
        Some(deny::descendants(canonical, sid_str, cancel)?)
    } else {
        None
    };
    apply_explicit_ace(target, sid_str, kind.mask(), inheritable, kind.ace_type())?;
    // Apply, then record: dying between the two leaves no witness, and the
    // next grant re-runs a propagation that is idempotent by construction.
    record_stamp(&key)
}

/// The mandatory-label half of a read-write grant, stamped and witnessed the
/// same way [`ensure_fs_grant`] treats the capability ACE — its own stamp-key
/// namespace, so the two mutations' idempotency never entangles, and its own
/// probe, so a label removed or never stamped is seen whatever the DACL says.
fn ensure_low_integrity_label(target: Target<'_>) -> Result<(), DaclError> {
    let key = label_stamp_key(target.path());
    if stamp_recorded(&key) && low_integrity_label_present(target) {
        return Ok(());
    }
    apply_mandatory_label_ace(target)?;
    record_stamp(&key)
}

/// Whether `canonical`'s own DACL already carries the stamp: an explicit ACE
/// for `sid_str` of `kind`'s type, covering `kind`'s whole mask, inheritable
/// when the target is a directory. An unreadable DACL answers `false` —
/// attempt the real grant and let it fail loudly, the same posture as
/// [`appcontainer_already_grants`].
fn capability_ace_present<'a>(
    target: impl Into<Target<'a>>,
    sid_str: &str,
    kind: GrantKind,
    inheritable: bool,
) -> bool {
    match scan_explicit_aces_for_sid(target.into(), sid_str) {
        Ok(aces) => aces.iter().any(|a| {
            a.ace_type == kind.ace_type()
                && (a.access_mask & kind.mask()) == kind.mask()
                && (!inheritable || (a.inherit_flags & OICI_ACE_FLAGS) == OICI_ACE_FLAGS)
        }),
        Err(_) => false,
    }
}

/// `canonicalise_strict` emits `\\?\X:\...` for a local drive and
/// `\\?\UNC\server\share\...` for a share; a caller may also hand us a
/// `\\.\Volume{GUID}\...` DOS-device path, local too though canonicalisation
/// never produces it.
fn ensure_local_canonical_prefix(canonical: &Path) -> Result<(), DaclError> {
    let s = canonical.to_string_lossy();
    let bytes = s.as_bytes();
    if bytes.len() >= 8
        && &bytes[..4] == b"\\\\?\\"
        && bytes[4].eq_ignore_ascii_case(&b'U')
        && bytes[5].eq_ignore_ascii_case(&b'N')
        && bytes[6].eq_ignore_ascii_case(&b'C')
        && bytes[7] == b'\\'
    {
        return Err(DaclError::NetworkPathRejected(canonical.to_path_buf()));
    }
    if s.starts_with(r"\\?\") || s.starts_with(r"\\.\") {
        return Ok(());
    }
    if s.starts_with(r"\\") {
        return Err(DaclError::NetworkPathRejected(canonical.to_path_buf()));
    }
    Ok(())
}

fn canonicalize_local(path: &Path) -> Result<PathBuf, DaclError> {
    let canonical = match crate::path::canon::canonicalise_strict(path) {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(DaclError::PathNotFound(path.to_path_buf()));
        }
        Err(e) => {
            return Err(DaclError::Win32 {
                path: path.to_path_buf(),
                reason: format!("canonicalize: {e}"),
            });
        }
    };
    ensure_local_canonical_prefix(&canonical)?;
    Ok(canonical)
}
