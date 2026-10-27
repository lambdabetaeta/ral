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
//! ([`fs_capability_name`]); [`ensure_frozen_grant`] stamps that SID's ACE onto
//! the prefix, and a child reaches the path only if `session` put the
//! capability in its `LowBox` token. The stamp is the expensive part — one
//! security write (`SetSecurityInfo` through a held handle), whose inheritable ACE Windows propagates into every existing
//! descendant before returning, seconds on a build tree — so it is paid once
//! per `(path, kind)` *ever*, across sessions and projections, and never
//! reverted. Leaving it is safe: a capability SID is
//! matched only in the `AppContainer` pass of the access check, whose result
//! intersects the normal pass, so an ACE no live token names is inert and can
//! never widen any process's reach beyond the owning user's own.
//!
//! No ACE alone admits *creating* an entry: Windows runs the
//! mandatory-integrity check before the `AppContainer` pass, and an unlabeled
//! directory refuses every `Low`-IL `LowBox` child's create regardless of the
//! DACL. A read-write grant on a directory therefore always stamps a
//! `SYSTEM_MANDATORY_LABEL_ACE` at `Low` ([`ensure_low_integrity_label`]) — its
//! own permanent mutation, witnessed the same way under its own stamp-key
//! namespace — even where its capability ACE is not needed. The label is
//! container-inherit only, never object-inherit: Windows caps a process at the
//! label on its image file, so a `Low` label reaching files would run every
//! host-built executable under the tree at `Low`. Files stay unlabeled, and the
//! capability ACE alone admits writing an existing one.
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
//! A deny stamps nothing. The `AppContainer` pass of the access check matches
//! an allow ACE against the token's package and capability SIDs but a deny ACE
//! against its ordinary groups alone, so a deny ACE naming a capability is
//! inert and no ACE can take back what a capability's ACE admits. A deny is
//! enforced by absence instead: `session` carves it out of the allowances
//! around it (`authority::sandbox::carve`), granting their other entries and each carved
//! directory only its own listing ([`GrantKind::List`]), refuses one on or
//! around an allowance, and [`ensure_unreachable`] proves, per launch, that the
//! token reaches nothing beneath it. See `decisions/260730_path-derived-capability-sids.md`,
//! amendment of 2026-10-20.
//!
//! What still needs undoing is the profile. `DaclManager` ledgers
//! `AppContainer` profile registrations before the OS-level create, and
//! [`recover_orphaned_state`] sweeps a crashed session's ledger at the next
//! boot, deleting its registered profiles.

use crate::own::OwnError;
use crate::path::lex::is_volume_guid;
use crate::path::shown;
use crate::process::cancel::{CancelCause, Scope};
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};

mod acl;
mod deny;
mod frozen;
mod ledger;
mod mutex;
mod stamp;
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;

use acl::{
    AceType, OICI_ACE_FLAGS, Target, appcontainer_already_grants, apply_explicit_ace,
    apply_mandatory_label_ace, low_integrity_label_present, scan_explicit_aces_for_sid,
};
pub(crate) use deny::{ContainerAccess, ensure_unreachable};
pub(crate) use frozen::{FrozenPin, pin_all};
pub(crate) use ledger::{DaclManager, recover_orphaned_state};
use mutex::PathMutexGuard;
use stamp::{label_stamp_key, record_stamp, stamp_key, stamp_recorded, update_stamp};

/// Rights stamped on a read-write prefix. `FILE_GENERIC_EXECUTE` is there for
/// `FILE_TRAVERSE`, without which the child cannot `SetCurrentDirectoryW` in;
/// NTFS cannot grant the one without the other, so inheritance carries
/// `FILE_EXECUTE` down to every file — accepted, since the `AppContainer`
/// already sandboxes code execution.
const RW_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

/// Rights stamped on a read-only prefix; execute is there for the same
/// `FILE_TRAVERSE` reason as [`RW_MASK`].
const RO_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// The three access shapes a grant stamps. Each keys its own capability name
/// and its own stamp-store entry, so read-only coverage never satisfies a
/// read-write request, and a carved directory's listing never stands for a
/// grant of what is in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GrantKind {
    ReadWrite,
    ReadOnly,
    /// A directory carved around an exclusion (`authority::sandbox::carve`): read-only
    /// rights on the directory object alone, by an ACE nothing inherits, so
    /// the child may traverse it, enter it and list its names, and reaches no
    /// entry through it — an entry made there later included.
    List,
}

impl GrantKind {
    fn mask(self) -> u32 {
        match self {
            Self::ReadWrite => RW_MASK,
            Self::ReadOnly | Self::List => RO_MASK,
        }
    }

    /// Whether the ACE reaches what lies beneath a directory.
    fn inherits(self) -> bool {
        match self {
            Self::ReadWrite | Self::ReadOnly => true,
            Self::List => false,
        }
    }

    /// The capability-name segment, doubling as the stamp-store key prefix.
    fn tag(self) -> &'static str {
        match self {
            Self::ReadWrite => "rw",
            Self::ReadOnly => "ro",
            Self::List => "ls",
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
    /// A frozen allowance on a share, a removable drive or a volume that has
    /// no GUID path: no DACL a stamp may key.
    NotLocal(PathBuf),
    /// A frozen allowance whose path is not Unicode: a Windows grant names its
    /// object by text, so it is refused where it is pinned.
    NotUnicode(PathBuf),
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
    /// `at`, inside the exclusion `denied` (or `denied` itself), is one the
    /// launch's token could still open.
    Reachable {
        denied: PathBuf,
        at: PathBuf,
    },
    /// The access check standing in for the launch's token could not be run.
    AccessCheck {
        op: &'static str,
        error: io::Error,
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

impl std::fmt::Display for DaclError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLocal(p) => write!(
                f,
                "'{}' is not on a local fixed volume (a network share, a removable \
                 drive and an unnamed volume cannot be granted); grant a folder on a local disk",
                shown(p)
            ),
            Self::NotUnicode(p) => write!(
                f,
                "'{}' is not valid Unicode, and a Windows grant names its object by text: \
                 a grant that cannot be expressed faithfully is refused, not approximated",
                shown(p)
            ),
            Self::Redirected {
                granted,
                at,
                target: Some(t),
            } if is_volume_guid(t) => write!(
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
                write!(f, "'{}' was granted by its real path, but ", shown(granted))?;
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
            Self::Reachable { denied, at } => {
                write!(f, "'{}' is excluded", shown(at))?;
                if at != denied {
                    write!(f, " (it lies inside the excluded '{}')", shown(denied))?;
                }
                f.write_str(
                    ", but a sandboxed program could still open it: its own permissions \
                     admit every Windows sandbox (as `ALL APPLICATION PACKAGES` does) or carry \
                     a folder this grant allows (as a hard link does), and Windows cannot take \
                     such access away. Remove that access from it, or drop the exclusion",
                )
            }
            Self::AccessCheck { op, error } => {
                write!(
                    f,
                    "cannot check what the sandbox could open ({op}): {error}"
                )
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

impl From<OwnError> for DaclError {
    /// JSON that is not a ledger is a parse error; anything else is the I/O's.
    fn from(e: OwnError) -> Self {
        match e {
            OwnError::Json { .. } => Self::LedgerParse(e.to_string()),
            e => Self::LedgerIo(e.into()),
        }
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

/// The capability name whose derived SID carries one `(path, kind)` grant.
/// Deterministic across sessions and machines — that is what lets a stamp be
/// paid once and matched forever — and collision-resistant, since two paths
/// sharing a name would silently merge their authority: the name embeds 128
/// bits of the canonical path's SHA-256, short enough to stay well inside any
/// capability-name length the OS might enforce. The canonical spelling, as the
/// text [`FrozenPin::name`] holds, is hashed as-is, not case-folded:
/// canonicalisation already yields one spelling per object, and folding would
/// merge two genuinely distinct names in an NTFS case-sensitive directory into
/// one authority.
pub(crate) fn fs_capability_name(name: &str, kind: GrantKind) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(name.as_bytes());
    let mut capability = format!("ral.fs.{}.", kind.tag());
    for b in &digest[..16] {
        let _ = write!(capability, "{b:02x}");
    }
    capability
}

/// Ensure the persistent ACE for a frozen allowance's `kind` under `sid_str` —
/// the SID [`fs_capability_name`] derives to — applied through the handle
/// [`FrozenPin`] holds, so the object checked is the object stamped; and, for
/// a read-write grant, the mandatory label alongside it when the target is a
/// directory (see [`ensure_low_integrity_label`]).
///
/// A read-write grant is two mutations, so each answers for itself and neither
/// may speak for the other: the label is asked *before* anything about the ACE,
/// since a witness that only knows the DACL would otherwise wave through as
/// complete a grant some earlier ral stamped when the label half did not yet
/// exist — and, nothing here reverting, that path would stay unwritable for
/// good with no witness able to say why. The ACE is skipped where the
/// well-known `AppContainer` SIDs already admit `kind`, or where both of its
/// witnesses hold: the stamp store records a completed propagation, and the
/// root's own DACL still carries it. A stamp already recorded needs no
/// `WRITE_DAC`: only the write that would be new fails, as `WriteDacDenied`.
/// `cancel` is polled before the apply; Win32's own propagation is not
/// interruptible.
pub(crate) fn ensure_frozen_grant(
    pin: &FrozenPin,
    sid_str: &str,
    kind: GrantKind,
    cancel: &Scope,
) -> Result<(), DaclError> {
    cancelled(cancel)?;
    let target = pin.target();
    let canonical = target.path;
    let inheritable = kind.inherits()
        && target.is_dir().map_err(|e| DaclError::Win32 {
            path: canonical.to_path_buf(),
            reason: format!("metadata: {e}"),
        })?;
    // No ACE alone admits a create: the mandatory-integrity check runs first,
    // and a `Low`-IL child clears it only if the directory's own label is
    // `Low` or below. A read-only grant needs no label — `Low` already reads
    // `Medium` under the default no-write-up-only policy — and neither does a
    // file, whose write the capability ACE alone admits.
    if kind == GrantKind::ReadWrite && inheritable {
        ensure_low_integrity_label(target)?;
    }
    // The ACE, never the label, is skipped where every sandbox already has its
    // rights: else `System32` fails a non-admin session on `WRITE_DAC` (after
    // mxc `dispatcher.rs::filter_paths_needing_grant`).
    if appcontainer_already_grants(target, kind.mask()) {
        return Ok(());
    }
    let key = stamp_key(pin.name(), kind);
    // Serialize the witness with the mutation, including an interrupted retry.
    let _guard = PathMutexGuard::acquire(canonical)?;
    if stamp_recorded(&key) && capability_ace_present(target, sid_str, kind, inheritable) {
        return Ok(());
    }
    // A recreated root must not resurrect its old completion witness mid-apply.
    update_stamp(&key, false)?;
    apply_explicit_ace(target, sid_str, kind.mask(), inheritable)?;
    // Apply, then record: dying between the two leaves no witness, and the
    // next grant re-runs a propagation that is idempotent by construction.
    record_stamp(&key)
}

/// The mandatory-label half of a read-write grant, stamped and witnessed the
/// same way [`ensure_frozen_grant`] treats the capability ACE — its own stamp-key
/// namespace, so the two mutations' idempotency never entangles, and its own
/// probe, so a label removed or never stamped is seen whatever the DACL says.
fn ensure_low_integrity_label(target: Target<'_>) -> Result<(), DaclError> {
    let key = label_stamp_key(target.name);
    if stamp_recorded(&key) && low_integrity_label_present(target) {
        return Ok(());
    }
    apply_mandatory_label_ace(target)?;
    record_stamp(&key)
}

/// Whether `canonical`'s own DACL already carries the stamp: an explicit allow
/// ACE for `sid_str` covering `kind`'s whole mask, inheriting exactly when
/// `inheritable`. An unreadable DACL answers `false` —
/// attempt the real grant and let it fail loudly, the same posture as
/// [`appcontainer_already_grants`].
fn capability_ace_present(
    target: Target<'_>,
    sid_str: &str,
    kind: GrantKind,
    inheritable: bool,
) -> bool {
    match scan_explicit_aces_for_sid(target, sid_str) {
        Ok(aces) => aces.iter().any(|a| {
            a.ace_type == AceType::Allow
                && (a.access_mask & kind.mask()) == kind.mask()
                && (a.inherit_flags & OICI_ACE_FLAGS)
                    == if inheritable { OICI_ACE_FLAGS } else { 0 }
        }),
        Err(_) => false,
    }
}
