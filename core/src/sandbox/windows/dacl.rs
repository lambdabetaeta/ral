// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// This module is a close port of `wxc_common/src/filesystem_dacl.rs` from
// github.com/microsoft/mxc @ 0e7c3dd (the `DaclManager` apply/restore
// engine), adapted to ral's naming, error types, and `windows-sys` binding.
// Each ported unit carries a `// after mxc filesystem_dacl.rs::<fn>`
// breadcrumb naming its upstream counterpart, for future diffing against
// upstream (see `dev/docs/260712_windows_port.md`, W1).

//! Crash-safe grant-ACE apply/restore engine for the session-scoped
//! AppContainer sandbox.
//!
//! [`DaclManager`] stamps allow-ACEs for an AppContainer SID onto host
//! filesystem prefixes so a LowBox-token child can read or read-write them,
//! and reverts every stamp it made — on explicit [`DaclManager::restore`] or
//! on [`Drop`]. The lifecycle is **session-scoped, not per-spawn**:
//! [`crate::sandbox::windows::session`] creates exactly one `DaclManager`
//! per shell session and holds it in session-global state for the
//! session's whole lifetime, accumulating every confined command's grants
//! into it; [`crate::sandbox::windows::session::teardown`] is what finally
//! calls [`restore`](DaclManager::restore), and only runs at session end.
//!
//! **Consequence worth stating plainly:** because the profile SID and its
//! stamped ACEs are shared across every command the session confines, and
//! nothing revokes a grant between commands, a session's confinement
//! *widens monotonically* over its lifetime — a child spawned by command 2
//! can open any path command 1's grant stamped, even if command 2's own
//! declared projection is narrower, because the OS access check at
//! open-time sees the union of ACEs ever stamped for the session SID, not
//! just the ones the current command's projection asked for. The same
//! persistence cuts the other way for denies: an explicit deny-ACE stamped
//! for command 1's `deny_path` canonically precedes any allow a later
//! command stamps, so command 2 stays blocked on that path even where its
//! own projection grants it — attenuation-safe, but cross-command
//! interference all the same. Both directions are a deliberate consequence
//! of the session-scoped-profile decision
//! (`dev/docs/260712_windows_port.md`, W1), not an oversight — narrowing
//! per command would require a fresh AppContainer SID (and profile
//! create/delete round trip) on every spawn.
//!
//! # Design
//!
//! - **Inheritable ACEs.** Directories get `OBJECT_INHERIT_ACE |
//!   CONTAINER_INHERIT_ACE`; `SetNamedSecurityInfoW`'s automatic propagation
//!   handles both the add and the remove, so there is no manual descendant
//!   walk.
//! - **ACE-merge resilience.** `SetEntriesInAclW(GRANT)` coalesces rights for
//!   the same trustee into a single ACE, so a second grant to a SID that
//!   already has an explicit ACE silently *merges* rather than adds a
//!   second entry. Before every apply we capture *every* explicit
//!   (non-inherited) ACE already on the target for our SID — allow or deny —
//!   into [`AppliedAce::prior_state`]. Restore then issues a full rebuild
//!   (drop our SID's explicit ACEs, replay `prior_state` verbatim) rather
//!   than trusting `REVOKE_ACCESS` to undo only our contribution.
//!
//! # Crash-safety protocol
//!
//! The ordering the whole engine exists to protect, per grant:
//!
//! 1. Acquire the per-path named mutex ([`PathMutexGuard`]) so no other ral
//!    session touches the same path's DACL concurrently.
//! 2. Scan the target's current DACL for explicit ACEs already held by our
//!    SID ([`scan_explicit_aces_for_sid`]) — this is the state a crash must
//!    restore.
//! 3. **Write the ledger to disk before touching the DACL.** The in-memory
//!    entry is appended and [`DaclManager::persist_ledger`] durably commits
//!    the whole applied-list (stage to `.tmp`, `fsync`, rename over the
//!    destination) *before* any Win32 apply call runs. If the process dies
//!    between steps 3 and 4, the ledger already names everything needed to
//!    undo — nothing is lost.
//! 4. Apply the ACE via `SetEntriesInAclW` + `SetNamedSecurityInfoW`.
//! 5. Release the mutex (guard drop).
//!
//! Restore reverses a single entry by re-acquiring its path's mutex and
//! rebuilding the DACL from the captured `prior_state` — never a bare
//! `REVOKE_ACCESS` (see [`replace_explicit_aces_for_sid`] for why). On
//! success the entry is dropped from the ledger; on failure it is *retained*
//! on disk and in memory so a later `restore()` call, or the next session's
//! [`recover_orphaned_state`] sweep, can retry — a transient failure never
//! silently loses track of a host ACL mutation.
//!
//! [`recover_orphaned_state`] is the orphan sweep: it inspects every ledger
//! file under the state directory, classifies its owning process as
//! live-or-dead (PID reuse is defeated by comparing the recorded process
//! *creation time* against the live process's, via `GetProcessTimes`; see
//! [`process_alive_with_image`]), and restores the DACLs a dead owner left
//! stamped. W1c calls this once at session boot, before any grant is
//! applied; it is not wired to anything from this module.
//!
//! # Ledger directory
//!
//! Ledgers live under the per-user ral state directory (the `state` XDG
//! kind — see [`crate::path::basedir`] — joined with `ral/sandbox-dacl`),
//! not a hardcoded `%LOCALAPPDATA%` path, so overriding `XDG_STATE_HOME`
//! (as tests do) relocates it. Three file species can appear there:
//!
//! - `<run-id>.json` — a fully written, owner-active ledger.
//! - `<run-id>.json.tmp` — an in-progress atomic write; harmless leftover
//!   from a crash mid-write, cleaned up at the start of the next write to
//!   the same path.
//! - `<run-id>.json.corrupt` — quarantine of a ledger that failed to parse
//!   during recovery, kept for inspection rather than silently discarded.
//!
//! # Caveats carried over from upstream
//!
//! - **The container id must be per-session.** Two concurrent stampers
//!   sharing one AppContainer SID clobber each other's grants — the
//!   merge-then-restore dance above only defends against *sequential*
//!   overlapping grants on one path, not concurrent ones from two SIDs that
//!   happen to be the same string. Callers must derive a fresh SID per
//!   session.
//! - **Apply/restore is O(prefixes) syscalls.** Each prefix is its own
//!   mutex acquisition, DACL scan, and `SetNamedSecurityInfoW` round trip;
//!   there is no batching across prefixes.
//!
//! Note: the module-level `#[cfg(windows)]` lives on the `mod dacl;`
//! declaration in `sandbox/windows.rs` (inherited from `sandbox.rs`'s
//! `#[cfg(windows)] mod windows;`). Repeating it here as an inner
//! `#![cfg(windows)]` would trip clippy's `duplicated_attributes` lint.

use std::ffi::c_void;
use std::fs;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_SUCCESS, FILETIME, HANDLE, WAIT_ABANDONED, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
    NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
    AddAccessAllowedAceEx, AddAccessDeniedAceEx, AddAce, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetLengthSid, INHERITED_ACE,
    InitializeAcl, IsValidSid, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, ReleaseMutex,
    WaitForSingleObject,
};

// -------------------------------------------------------------------------
// Access masks
// -------------------------------------------------------------------------

/// Access mask granted on a read-write prefix: read + write + execute +
/// delete. `FILE_GENERIC_EXECUTE` is required so the AppContainer child can
/// `SetCurrentDirectoryW` into the granted directory — the API opens the
/// target with `FILE_TRAVERSE`, which is the same bit (`0x20`) as
/// `FILE_EXECUTE` for files.
///
/// after mxc filesystem_dacl.rs::RW_MASK (0e7c3dd)
///
/// Deliberate side-effect: because the ACE is inheritable, the same bit
/// propagates as `FILE_EXECUTE` to every file descendant. Accepted for the
/// same reasons upstream accepts it: workloads routinely need to execute
/// helper binaries under a granted scratch tree; NTFS has no primitive for
/// "traverse but never execute" (the two rights share a bit, distinguished
/// only by the kernel's per-object-type interpretation); and the
/// AppContainer is already a code-execution sandbox, so `FILE_EXECUTE` on a
/// host file grants nothing a compromised child couldn't already do
/// in-memory.
pub(crate) const RW_MASK: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

/// Access mask granted on a read-only prefix: read + execute, for the same
/// `chdir`-needs-`FILE_TRAVERSE` reason as [`RW_MASK`].
///
/// after mxc filesystem_dacl.rs::RO_MASK (0e7c3dd)
pub(crate) const RO_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

const _: () = {
    assert!(RW_MASK == FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE);
    assert!(RO_MASK == FILE_GENERIC_READ | FILE_GENERIC_EXECUTE);
    assert!(
        (RW_MASK & RO_MASK) == RO_MASK,
        "RW must be a superset of RO"
    );
    // FILE_TRAVERSE == FILE_EXECUTE == 0x20 is part of FILE_GENERIC_EXECUTE.
    // Both masks must carry it so `chdir` into a granted directory works.
    assert!(RW_MASK & 0x20 == 0x20, "RW must grant FILE_TRAVERSE");
    assert!(RO_MASK & 0x20 == 0x20, "RO must grant FILE_TRAVERSE");
};

// -------------------------------------------------------------------------
// Public error type
// -------------------------------------------------------------------------

/// Errors returned by [`DaclManager`] and [`recover_orphaned_state`].
///
/// after mxc filesystem_dacl.rs::DaclError (0e7c3dd)
#[derive(Debug)]
pub enum DaclError {
    /// Caller passed a UNC network path; only local paths are supported.
    NetworkPathRejected(PathBuf),
    /// The path could not be resolved (does not exist).
    PathNotFound(PathBuf),
    /// `WRITE_DAC` denied on the target.
    WriteDacDenied { path: PathBuf, reason: String },
    /// Any other Win32 failure.
    Win32 { path: PathBuf, reason: String },
    /// Ledger file I/O error.
    LedgerIo(io::Error),
    /// Ledger file failed to parse.
    LedgerParse(String),
    /// SID string could not be parsed.
    InvalidSid(String),
    /// Timed out waiting on the per-path serialization mutex.
    MutexTimeout { path: PathBuf, timeout_ms: u32 },
}

impl std::fmt::Display for DaclError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NetworkPathRejected(p) => write!(
                f,
                "path is not local (network/UNC paths not supported): {}",
                p.display()
            ),
            Self::PathNotFound(p) => write!(f, "path does not exist: {}", p.display()),
            Self::WriteDacDenied { path, reason } => {
                write!(f, "WRITE_DAC denied on {}: {reason}", path.display())
            }
            Self::Win32 { path, reason } => {
                write!(f, "Win32 error on {}: {reason}", path.display())
            }
            Self::LedgerIo(e) => write!(f, "DACL ledger I/O error: {e}"),
            Self::LedgerParse(s) => write!(f, "DACL ledger parse error: {s}"),
            Self::InvalidSid(s) => write!(f, "invalid SID string: {s}"),
            Self::MutexTimeout { path, timeout_ms } => write!(
                f,
                "timed out acquiring DACL mutex on {} after {timeout_ms} ms",
                path.display()
            ),
        }
    }
}

impl std::error::Error for DaclError {}

impl From<io::Error> for DaclError {
    fn from(e: io::Error) -> Self {
        Self::LedgerIo(e)
    }
}

// -------------------------------------------------------------------------
// Ledger types
// -------------------------------------------------------------------------

/// Distinguishes allow vs deny ACEs captured as prior state.  Grants applied
/// by this module are always [`Self::Allow`]; [`Self::Deny`] exists so a
/// pre-existing explicit deny ACE left by another tool (`icacls`, a prior
/// session) is captured and replayed verbatim on restore rather than
/// dropped.
///
/// after mxc filesystem_dacl.rs::AceType (0e7c3dd)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum AceType {
    Allow,
    Deny,
}

/// One explicit (non-inherited) ACE that existed on the target *before* we
/// applied — captured so restore can reconstruct the exact pre-apply DACL,
/// defeating `SetEntriesInAclW`'s rights-coalescing for the same trustee.
///
/// after mxc filesystem_dacl.rs::PriorAce (0e7c3dd)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct PriorAce {
    ace_type: AceType,
    access_mask: u32,
    /// Raw `AceFlags` byte with `INHERITED_ACE` masked off (only explicit
    /// ACEs are ever captured here).
    inherit_flags: u8,
}

/// Persisted record of one applied ACE.
///
/// after mxc filesystem_dacl.rs::AppliedAce (0e7c3dd)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppliedAce {
    canonical_path: PathBuf,
    sid_string: String,
    access_mask: u32,
    ace_type: AceType,
    /// Whether `OI|CI` were set (directories only).
    inheritable: bool,
    /// Every explicit ACE for our SID that existed before we applied,
    /// regardless of type.  Empty means "nothing to restore to but a bare
    /// revoke".
    prior_state: Vec<PriorAce>,
}

/// Ledger written before each ACE is applied, so a crash between the apply
/// and the next checkpoint still leaves enough on disk to undo.
///
/// after mxc filesystem_dacl.rs::StateFile (0e7c3dd)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ledger {
    run_id: String,
    pid: u32,
    image_name: String,
    /// Owning process *creation* time as a Windows FILETIME, captured once
    /// at [`DaclManager::new`].  Recovery compares this against the live
    /// process's creation time before classifying the ledger as active,
    /// defeating PID reuse (a crashed owner's PID later recycled by an
    /// unrelated process).
    started_at_filetime: u64,
    applied: Vec<AppliedAce>,
}

/// Aggregated outcome of [`recover_orphaned_state`].
#[derive(Debug, Default)]
pub struct RecoveryReport {
    /// Number of ledger files inspected.
    pub files_processed: usize,
    /// Total ACEs successfully removed across all orphaned ledgers.
    pub aces_restored: usize,
    /// ACEs pruned because their target path no longer exists — there is
    /// nothing to restore on a deleted file, so the entry is dropped rather
    /// than retained-and-retried forever.
    pub aces_pruned_missing: usize,
    /// Per-file or per-path errors, formatted for logging.
    pub errors: Vec<String>,
}

// -------------------------------------------------------------------------
// DaclManager: the guard
// -------------------------------------------------------------------------

/// Crash-safe guard for filesystem DACL grants.
///
/// Apply grants via [`grant_appcontainer_access`](Self::grant_appcontainer_access);
/// call [`restore`](Self::restore) to undo explicitly, or simply drop the
/// guard — [`Drop`] restores best-effort. [`crate::sandbox::windows::session`]
/// holds exactly one of these per shell session (not one per spawn): create
/// it once at session start, grant into it on every command that confines
/// one, and drop (or `restore`) it once at session end — see the
/// session-scoped-lifecycle note at the top of this module for the
/// consequence that accumulation across commands carries.
///
/// after mxc filesystem_dacl.rs::DaclManager (0e7c3dd)
#[derive(Debug)]
pub struct DaclManager {
    run_id: String,
    ledger_path: PathBuf,
    applied: Vec<AppliedAce>,
    warnings: Vec<String>,
    process_start_filetime: u64,
}

impl DaclManager {
    /// Create a new manager.  The ledger directory is created if missing; a
    /// fresh run id is generated and the (empty) ledger is not written until
    /// the first ACE is applied.
    ///
    /// after mxc filesystem_dacl.rs::DaclManager::new (0e7c3dd)
    pub fn new() -> Result<Self, DaclError> {
        let dir = ensure_ledger_dir()?;
        let run_id = generate_run_id();
        let ledger_path = dir.join(format!("{run_id}.json"));
        let process_start_filetime = process_creation_filetime()?;
        Ok(Self {
            run_id,
            ledger_path,
            applied: Vec::new(),
            warnings: Vec::new(),
            process_start_filetime,
        })
    }

    /// Warnings accumulated during apply/restore (non-fatal issues).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Grant the AppContainer SID read-write access on `readwrite` prefixes
    /// and read-only access on `readonly` prefixes, stamping an allow-ACE
    /// for each and capturing its prior ACE state first.  `sid_str` is the
    /// per-session AppContainer SID (see the module-level caveat: it must
    /// not be shared by two concurrent stampers).
    ///
    /// after mxc filesystem_dacl.rs::DaclManager::grant_appcontainer_access (0e7c3dd)
    pub fn grant_appcontainer_access(
        &mut self,
        sid_str: &str,
        readwrite: &[PathBuf],
        readonly: &[PathBuf],
    ) -> Result<(), DaclError> {
        for p in readwrite {
            self.apply_one(sid_str, p, RW_MASK, AceType::Allow)?;
        }
        for p in readonly {
            self.apply_one(sid_str, p, RO_MASK, AceType::Allow)?;
        }
        Ok(())
    }

    /// Stamp an explicit **deny**-ACE for `sid_str` on each path in `denied`,
    /// denying the full `FILE_ALL_ACCESS` surface.  Callers use this for a
    /// `deny_path` nested inside a granted read/write prefix: the enclosing
    /// allow-ACE is inheritable, so it would otherwise propagate down and
    /// expose the nested path; an explicit deny stamped here precedes the
    /// inherited allow in canonical ACL order (`SetEntriesInAclW` places
    /// explicit-deny before inherited-allow), so the deny wins.  A
    /// `deny_path` *not* inside any granted prefix needs no stamp — the
    /// AppContainer is deny-by-default, so it is already unreachable.
    ///
    /// Denies go through the same [`apply_one`](Self::apply_one) lifecycle as
    /// grants — per-path mutex, prior-state capture, ledger-before-apply — so
    /// [`restore`](Self::restore) and [`recover_orphaned_state`] revert them
    /// identically.
    ///
    /// after mxc filesystem_dacl.rs::DaclManager::add_deny_aces (0e7c3dd)
    pub fn add_deny_aces(&mut self, sid_str: &str, denied: &[PathBuf]) -> Result<(), DaclError> {
        // FILE_ALL_ACCESS = STANDARD_RIGHTS_REQUIRED | SYNCHRONIZE | 0x1FF.
        let deny_mask: u32 = 0x001F_01FF;
        for p in denied {
            self.apply_one(sid_str, p, deny_mask, AceType::Deny)?;
        }
        Ok(())
    }

    /// Idempotently remove every ACE this manager has applied.  Failures are
    /// per-entry: a transient error on one path does not block the rest and
    /// is recorded into [`warnings`](Self::warnings); the failed entry is
    /// retained (in memory and on the ledger) so a future `restore()` call,
    /// or [`recover_orphaned_state`] on the next session's boot, can retry.
    /// Only fatal ledger I/O surfaces as `Err`.
    ///
    /// after mxc filesystem_dacl.rs::DaclManager::restore (0e7c3dd)
    pub fn restore(&mut self) -> Result<(), DaclError> {
        // Tail-first (LIFO): the last ACE applied is the first removed.
        let mut remaining: Vec<AppliedAce> = Vec::new();
        while let Some(entry) = self.applied.pop() {
            match restore_one(&entry) {
                Ok(()) => {}
                Err(e) => {
                    self.warnings.push(format!(
                        "restore failed for {} (entry retained for retry): {e}",
                        entry.canonical_path.display(),
                    ));
                    remaining.push(entry);
                }
            }
        }
        // `remaining` was pushed in pop order (newest first); reverse to
        // restore the original apply order so a future retry again
        // processes tail-first.
        remaining.reverse();
        self.applied = remaining;
        if self.applied.is_empty() {
            remove_ledger_best_effort(&self.ledger_path);
        } else {
            self.persist_ledger()?;
        }
        Ok(())
    }

    /// after mxc filesystem_dacl.rs::DaclManager::apply_one (0e7c3dd)
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:dacl-apply] Stats the grant target to choose OI|CI inheritance before stamping the allow-ACE. Sandbox grant-application infrastructure, not model data I/O — raises no surface card."
    )]
    fn apply_one(
        &mut self,
        sid_str: &str,
        path: &Path,
        mask: u32,
        ace_type: AceType,
    ) -> Result<(), DaclError> {
        let canonical = canonicalize_local(path)?;
        let inheritable = fs::metadata(&canonical)
            .map_err(|e| DaclError::Win32 {
                path: canonical.clone(),
                reason: format!("metadata: {e}"),
            })?
            .is_dir();

        // Hold the per-path mutex for the whole scan-persist-apply sequence
        // so no other ral session's stamper can interleave.
        let _guard = PathMutexGuard::acquire(&canonical)?;

        let prior_state = scan_explicit_aces_for_sid(&canonical, sid_str)?;

        let entry = AppliedAce {
            canonical_path: canonical,
            sid_string: sid_str.to_string(),
            access_mask: mask,
            ace_type,
            inheritable,
            prior_state,
        };

        // Ledger before apply (the crash-safety ordering the whole engine
        // exists for): if the process dies right after this, recovery has
        // everything needed to undo an apply that may or may not have
        // reached the Win32 call.
        self.applied.push(entry.clone());
        if let Err(e) = self.persist_ledger() {
            self.applied.pop();
            return Err(e);
        }

        apply_explicit_ace(
            &entry.canonical_path,
            &entry.sid_string,
            entry.access_mask,
            entry.inheritable,
            entry.ace_type,
        )
    }

    fn persist_ledger(&self) -> Result<(), DaclError> {
        let ledger = Ledger {
            run_id: self.run_id.clone(),
            pid: std::process::id(),
            image_name: current_image_basename(),
            started_at_filetime: self.process_start_filetime,
            applied: self.applied.clone(),
        };
        write_ledger(&self.ledger_path, &ledger)
    }
}

impl Drop for DaclManager {
    /// after mxc filesystem_dacl.rs::DaclManager::drop (0e7c3dd)
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            crate::diagnostic::shell_warning(&format!(
                "sandbox DACL guard: restore on drop failed: {e}"
            ));
        }
    }
}

/// Scan the ledger directory and reap every ledger whose owning process is
/// no longer alive, restoring the DACL state it recorded.  W1c calls this
/// once at session boot, before any grant is applied; nothing in this
/// module wires it up.
///
/// after mxc filesystem_dacl.rs::recover_orphaned_state (0e7c3dd)
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:dacl-ledger-sweep] Startup orphan sweep: lists the ledger directory, quarantines unparseable ledgers by rename, and removes fully-recovered ones. Sandbox crash-recovery infrastructure, not model data I/O."
)]
pub fn recover_orphaned_state() -> Result<RecoveryReport, DaclError> {
    let mut report = RecoveryReport::default();
    let dir = ledger_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(DaclError::LedgerIo(e)),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        report.files_processed += 1;
        let ledger = match read_ledger(&path) {
            Ok(s) => s,
            Err(e) => {
                report.errors.push(format!("parse {}: {e}", path.display()));
                let corrupt = path.with_extension("json.corrupt");
                if let Err(e2) = fs::rename(&path, &corrupt) {
                    report.errors.push(format!(
                        "quarantine {} -> {}: {e2}",
                        path.display(),
                        corrupt.display(),
                    ));
                }
                continue;
            }
        };
        if process_alive_with_image(
            ledger.pid,
            &ledger.image_name,
            Some(ledger.started_at_filetime),
        ) {
            continue;
        }
        let mut remaining: Vec<AppliedAce> = Vec::new();
        for ace in ledger.applied.iter().rev() {
            if matches!(ace.canonical_path.try_exists(), Ok(false)) {
                report.aces_pruned_missing += 1;
                continue;
            }
            match restore_one(ace) {
                Ok(()) => report.aces_restored += 1,
                Err(e) => {
                    if matches!(ace.canonical_path.try_exists(), Ok(false)) {
                        report.aces_pruned_missing += 1;
                    } else {
                        report.errors.push(format!(
                            "restore {} (pid {}): {e}",
                            ace.canonical_path.display(),
                            ledger.pid,
                        ));
                        remaining.push(ace.clone());
                    }
                }
            }
        }
        if remaining.is_empty() {
            if let Err(e) = fs::remove_file(&path) {
                report
                    .errors
                    .push(format!("remove {}: {e}", path.display()));
            }
        } else {
            remaining.reverse();
            let pending = Ledger {
                run_id: ledger.run_id,
                pid: ledger.pid,
                image_name: ledger.image_name,
                started_at_filetime: ledger.started_at_filetime,
                applied: remaining,
            };
            if let Err(e) = write_ledger(&path, &pending) {
                report
                    .errors
                    .push(format!("rewrite {}: {e}", path.display()));
            }
        }
    }
    Ok(report)
}

// -------------------------------------------------------------------------
// Ledger directory and file I/O
// -------------------------------------------------------------------------

/// The per-user ledger directory: the XDG `state` base joined with
/// `ral/sandbox-dacl` (see [`crate::path::basedir`]).
fn ledger_dir() -> PathBuf {
    crate::path::basedir::resolve_xdg(
        crate::path::basedir::XdgKind::State,
        &crate::path::home_from_env(),
    )
    .join("ral")
    .join("sandbox-dacl")
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:dacl-state-dir] Ensures the per-user DACL ledger directory exists before the first ledger write. Sandbox crash-safety infrastructure, not model data I/O."
)]
fn ensure_ledger_dir() -> Result<PathBuf, DaclError> {
    let dir = ledger_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Crash-safe write: stage to `<path>.tmp`, `fsync`, then atomically replace
/// the destination via rename (`MoveFileExW(..., MOVEFILE_REPLACE_EXISTING)`
/// on Windows). Recovery on the next boot therefore observes either the
/// prior complete ledger or the new one, never a half-written file.
///
/// after mxc filesystem_dacl.rs::write_state_file (0e7c3dd)
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:dacl-ledger-write] Atomic ledger write for the DACL crash-safety protocol: stage to <path>.tmp, fsync, rename over the destination. Sandbox infrastructure, not model data I/O."
)]
fn write_ledger(path: &Path, ledger: &Ledger) -> Result<(), DaclError> {
    let json = serde_json::to_vec_pretty(ledger)
        .map_err(|e| DaclError::LedgerParse(format!("serialize: {e}")))?;
    let tmp = tmp_path_for(path);
    // Best-effort: clear any leftover tmp from a previous crashed write so
    // `create_new` below doesn't spuriously fail with ERROR_FILE_EXISTS.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => crate::dbg_trace!(
            "sandbox-win-dacl",
            "pre-write cleanup of {} failed ({e}); a leftover tmp may obstruct the write",
            tmp.display()
        ),
    }
    {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(DaclError::LedgerIo(e));
    }
    Ok(())
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

fn remove_ledger_best_effort(path: &Path) {
    let _ = remove_ledger(path);
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:dacl-ledger-remove] Removes a fully-restored ledger file once every grant it recorded has been undone. Sandbox infrastructure, not model data I/O."
)]
fn remove_ledger(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

/// Retries on `ERROR_SHARING_VIOLATION`/`ERROR_LOCK_VIOLATION`: a concurrent
/// writer mid `<path>.tmp` -> `<path>` rename briefly holds the destination
/// open exclusively, and antivirus on-access scanners can surface the same
/// transient errors. Without the retry a perfectly-good ledger would be
/// quarantined as corrupt.
///
/// after mxc filesystem_dacl.rs::read_state_file (0e7c3dd)
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:dacl-ledger-read] Reads back a persisted ledger for orphan recovery / crash-safety bookkeeping. Sandbox infrastructure, not model data I/O."
)]
fn read_ledger(path: &Path) -> Result<Ledger, DaclError> {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    const ATTEMPTS: u32 = 3;
    let mut last_err: Option<io::Error> = None;
    for i in 0..ATTEMPTS {
        match fs::read(path) {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes)
                    .map_err(|e| DaclError::LedgerParse(format!("{}: {e}", path.display())));
            }
            Err(e) => {
                let transient = matches!(
                    e.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION)
                );
                if !transient {
                    return Err(e.into());
                }
                last_err = Some(e);
                if i + 1 < ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(20u64 << i));
                }
            }
        }
    }
    Err(DaclError::LedgerIo(last_err.unwrap_or_else(|| {
        io::Error::other("read_ledger: retries exhausted on transient error")
    })))
}

fn generate_run_id() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in pid.to_le_bytes().iter().chain(nanos.to_le_bytes().iter()) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("pid-{pid}-{h:016x}")
}

fn current_image_basename() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().and_then(|s| s.to_str()).map(str::to_string))
        .unwrap_or_else(|| "ral.exe".to_string())
}

// -------------------------------------------------------------------------
// Path canonicalization
// -------------------------------------------------------------------------

/// Classify whether an already-canonical path refers to a local object.
/// `canonicalise_strict` on Windows emits `\\?\X:\...` for local drive
/// paths and `\\?\UNC\server\share\...` for network shares; a caller may
/// also pass a `\\.\Volume{GUID}\...` DOS-device-namespace path directly,
/// which is local too even though canonicalisation does not normally
/// produce it.
///
/// after mxc filesystem_dacl.rs::ensure_local_canonical_prefix (0e7c3dd)
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

/// after mxc filesystem_dacl.rs::canonicalize_local (0e7c3dd)
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

// -------------------------------------------------------------------------
// Win32: SID parsing (RAII)
// -------------------------------------------------------------------------

/// Owned PSID returned by `ConvertStringSidToSidW`; frees via `LocalFree` on
/// drop.
///
/// after mxc filesystem_dacl.rs::OwnedSid (0e7c3dd)
struct OwnedSid(PSID);

// SAFETY: a `PSID` from `ConvertStringSidToSidW` is a private, immutable
// `LocalAlloc`'d buffer — nothing else in the process holds or mutates it.
// Sharing the pointer value across threads (as [`well_known_ac_sids`]'s
// process-wide cache does) is sound because every use is a read-only Win32
// call (`EqualSid`, `AddAccessAllowedAceEx`, …); ownership for the eventual
// `LocalFree` still belongs to whichever `OwnedSid` value is dropped, same
// as upstream (filesystem_dacl.rs::OwnedSid, 0e7c3dd, which asserts the
// same two impls).
unsafe impl Send for OwnedSid {}
unsafe impl Sync for OwnedSid {}

/// The longest well-formed SID the Win32 SID grammar can produce is well
/// under 200 characters (15 sub-authorities x ~10 digits, plus the `S-1-`
/// prefix and separators); cap generously at 256 to reject obviously
/// malformed input before engaging the Win32 parser.
const MAX_SID_STRING_LEN: usize = 256;

impl OwnedSid {
    fn parse(s: &str) -> Result<Self, DaclError> {
        if s.is_empty() {
            return Err(DaclError::InvalidSid("(empty)".to_string()));
        }
        if s.len() > MAX_SID_STRING_LEN {
            return Err(DaclError::InvalidSid(format!(
                "SID string too long ({} bytes, max {MAX_SID_STRING_LEN})",
                s.len(),
            )));
        }
        let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psid: PSID = std::ptr::null_mut();
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer kept alive for the
        // call; `psid` is an out-param the callee fills on success.
        let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut psid) };
        if ok == 0 {
            return Err(DaclError::InvalidSid(format!(
                "{s}: {}",
                io::Error::last_os_error()
            )));
        }
        // SAFETY: `psid` was just filled by a successful ConvertStringSidToSidW.
        if psid.is_null() || unsafe { IsValidSid(psid) } == 0 {
            if !psid.is_null() {
                unsafe {
                    windows_sys::Win32::Foundation::LocalFree(psid);
                }
            }
            return Err(DaclError::InvalidSid(s.to_string()));
        }
        Ok(Self(psid))
    }

    fn as_psid(&self) -> PSID {
        self.0
    }
}

impl Drop for OwnedSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was allocated by `ConvertStringSidToSidW`
            // (which uses `LocalAlloc` internally) and is freed exactly once.
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.0);
            }
        }
    }
}

// -------------------------------------------------------------------------
// Win32: per-path mutex (RAII)
// -------------------------------------------------------------------------

/// FNV-1a 64-bit hash.  Sufficient for mutex-name uniqueness — not a
/// security boundary.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// after mxc filesystem_dacl.rs::mutex_name_for (0e7c3dd)
fn mutex_name_for(canonical: &Path) -> String {
    let key = canonical.to_string_lossy().to_lowercase();
    let h = fnv1a64(&key);
    format!("Local\\ral.sandbox.dacl.{h:016x}")
}

/// after mxc filesystem_dacl.rs::PathMutexGuard (0e7c3dd)
struct PathMutexGuard {
    handle: HANDLE,
    acquired: bool,
}

/// Concurrent ral sessions applying ACEs to the same path are expected to
/// serialize on the order of seconds at most; 30s is a generous upper bound
/// that still surfaces an actionable error rather than hanging indefinitely
/// if a peer has wedged.
const PATH_MUTEX_WAIT_MS: u32 = 30_000;

impl PathMutexGuard {
    fn acquire(canonical: &Path) -> Result<Self, DaclError> {
        let name = mutex_name_for(canonical);
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and kept alive for the call; no
        // security attributes, not initially owned.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(DaclError::Win32 {
                path: canonical.to_path_buf(),
                reason: format!("CreateMutexW: {}", io::Error::last_os_error()),
            });
        }
        // SAFETY: `handle` is the mutex just created above.
        let wait = unsafe { WaitForSingleObject(handle, PATH_MUTEX_WAIT_MS) };
        if wait == WAIT_OBJECT_0 {
            return Ok(Self {
                handle,
                acquired: true,
            });
        }
        if wait == WAIT_ABANDONED {
            // The previous holder died without releasing; we still own the
            // mutex, and orphan recovery reconciles any DACL state it left
            // behind, so proceed rather than fail.
            crate::dbg_trace!(
                "sandbox-win-dacl",
                "acquired abandoned mutex for {}: previous holder terminated without releasing",
                canonical.display()
            );
            return Ok(Self {
                handle,
                acquired: true,
            });
        }
        let err = if wait == WAIT_TIMEOUT {
            DaclError::MutexTimeout {
                path: canonical.to_path_buf(),
                timeout_ms: PATH_MUTEX_WAIT_MS,
            }
        } else if wait == WAIT_FAILED {
            DaclError::Win32 {
                path: canonical.to_path_buf(),
                reason: format!("WaitForSingleObject: {}", io::Error::last_os_error()),
            }
        } else {
            DaclError::Win32 {
                path: canonical.to_path_buf(),
                reason: format!("WaitForSingleObject: unexpected result {wait}"),
            }
        };
        // SAFETY: `handle` is a valid handle owned by this call.
        unsafe {
            CloseHandle(handle);
        }
        Err(err)
    }
}

impl Drop for PathMutexGuard {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is a valid mutex handle owned by this guard;
        // released only if actually acquired, then always closed.
        unsafe {
            if self.acquired {
                ReleaseMutex(self.handle);
            }
            CloseHandle(self.handle);
        }
    }
}

// -------------------------------------------------------------------------
// Win32: apply / scan / restore a single ACE
// -------------------------------------------------------------------------

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn trustee_for(sid: &OwnedSid) -> TRUSTEE_W {
    TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        // The trustee-by-SID form reinterprets this field as the PSID
        // pointer per the Win32 contract; both are raw `*mut _` under
        // windows-sys so the cast is a bit-preserving reinterpret.
        ptstrName: sid.as_psid() as *mut u16,
    }
}

fn win32_err(path: &Path, op: &str, rc: u32) -> DaclError {
    DaclError::Win32 {
        path: path.to_path_buf(),
        reason: format!("{op}: {}", io::Error::from_raw_os_error(rc as i32)),
    }
}

fn win32_err_str(path: &Path, msg: &str) -> DaclError {
    DaclError::Win32 {
        path: path.to_path_buf(),
        reason: msg.to_string(),
    }
}

/// Apply a single explicit allow-or-deny ACE to `path`'s DACL via
/// `SetEntriesInAclW` + `SetNamedSecurityInfoW`.  `SetEntriesInAclW` merges
/// the new ACE into the existing DACL in canonical order — explicit-deny
/// before explicit-allow before inherited — so a deny stamped on a path that
/// inherits an allow from an enclosing grant wins.  The caller
/// ([`DaclManager::apply_one`]) holds the per-path mutex for the whole
/// scan-persist-apply sequence.
///
/// after mxc filesystem_dacl.rs::apply_explicit_ace (0e7c3dd)
fn apply_explicit_ace(
    path: &Path,
    sid_str: &str,
    access_mask: u32,
    inheritable: bool,
    ace_type: AceType,
) -> Result<(), DaclError> {
    let sid = OwnedSid::parse(sid_str)?;
    let inheritance: u32 = if inheritable {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    let access_mode = match ace_type {
        AceType::Allow => GRANT_ACCESS,
        AceType::Deny => DENY_ACCESS,
    };
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_mask,
        grfAccessMode: access_mode,
        grfInheritance: inheritance,
        Trustee: trustee_for(&sid),
    };

    let path_w = wide(path);
    let mut existing_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: all pointers are either NUL-terminated wide buffers kept
    // alive for the call, or out-params owned by this stack frame.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut existing_dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(path, "GetNamedSecurityInfoW", rc));
    }

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `ea` outlives the call; `existing_dacl` came from the query
    // above (may be null, which SetEntriesInAclW accepts as "no prior DACL").
    let rc = unsafe { SetEntriesInAclW(1, &ea, existing_dacl, &mut new_dacl) };
    if rc != ERROR_SUCCESS {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(sd);
        }
        return Err(win32_err(path, "SetEntriesInAclW", rc));
    }

    // SAFETY: `new_dacl` was just built by SetEntriesInAclW.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    unsafe {
        if !new_dacl.is_null() {
            windows_sys::Win32::Foundation::LocalFree(new_dacl as *mut c_void);
        }
        windows_sys::Win32::Foundation::LocalFree(sd);
    }

    if rc != ERROR_SUCCESS {
        if rc == ERROR_ACCESS_DENIED {
            return Err(DaclError::WriteDacDenied {
                path: path.to_path_buf(),
                reason: format!(
                    "SetNamedSecurityInfoW: {}",
                    io::Error::from_raw_os_error(rc as i32)
                ),
            });
        }
        return Err(win32_err(path, "SetNamedSecurityInfoW", rc));
    }
    Ok(())
}

/// Restore the target's DACL to its pre-apply state: acquire the per-path
/// mutex, then delegate to [`replace_explicit_aces_for_sid`].
///
/// after mxc filesystem_dacl.rs::restore_one (0e7c3dd)
fn restore_one(entry: &AppliedAce) -> Result<(), DaclError> {
    let _guard = PathMutexGuard::acquire(&entry.canonical_path)?;
    replace_explicit_aces_for_sid(&entry.canonical_path, &entry.sid_string, &entry.prior_state)
}

/// Scan the DACL on `canonical` and return every explicit (non-inherited)
/// ACE attached to `sid_str`, allow or deny.  Called under the per-path
/// mutex so the captured state is consistent with the apply that follows.
///
/// after mxc filesystem_dacl.rs::scan_explicit_aces_for_sid (0e7c3dd)
fn scan_explicit_aces_for_sid(canonical: &Path, sid_str: &str) -> Result<Vec<PriorAce>, DaclError> {
    let sid = OwnedSid::parse(sid_str)?;
    let path_w = wide(canonical);

    let mut existing_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: see apply_explicit_ace — same query shape.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut existing_dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(canonical, "GetNamedSecurityInfoW", rc));
    }
    if existing_dacl.is_null() {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(sd);
        }
        return Ok(Vec::new());
    }

    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `info` is a stack out-param sized exactly to the class asked for.
    let ok = unsafe {
        GetAclInformation(
            existing_dacl,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        let err = io::Error::last_os_error();
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(sd);
        }
        return Err(win32_err_str(
            canonical,
            &format!("GetAclInformation: {err}"),
        ));
    }

    let mut prior: Vec<PriorAce> = Vec::new();
    let inherited_bit = INHERITED_ACE as u8;
    for i in 0..info.AceCount {
        let mut ace_ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: `i` is within `info.AceCount`, which GetAclInformation
        // just reported for this exact ACL.
        if unsafe { GetAce(existing_dacl, i, &mut ace_ptr) } == 0 {
            continue;
        }
        // SAFETY: `ace_ptr` was filled by a successful GetAce; every ACE
        // begins with an ACE_HEADER.
        let header = unsafe { &*(ace_ptr as *const ACE_HEADER) };
        if (header.AceFlags & inherited_bit) != 0 {
            continue;
        }
        let ace_type = match header.AceType {
            0x00 => AceType::Allow,
            0x01 => AceType::Deny,
            _ => continue, // object/compound/audit ACEs are not our concern
        };
        // ACCESS_ALLOWED_ACE and ACCESS_DENIED_ACE share layout up to and
        // including the inline SidStart dword.
        let mask_and_sid = ace_ptr as *const ACCESS_ALLOWED_ACE;
        // SAFETY: same ACE, reinterpreted per the shared prefix layout.
        let ace_mask = unsafe { (*mask_and_sid).Mask };
        // SAFETY: `SidStart` is the first dword of the inline SID; its
        // address is exactly where the ACE's SID bytes begin.
        let ace_sid = (unsafe { &raw const (*mask_and_sid).SidStart }) as PSID;
        // SAFETY: both pointers reference valid SID buffers for the
        // duration of this call.
        if unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0 {
            prior.push(PriorAce {
                ace_type,
                access_mask: ace_mask,
                inherit_flags: header.AceFlags & !inherited_bit,
            });
        }
    }
    unsafe {
        windows_sys::Win32::Foundation::LocalFree(sd);
    }
    Ok(prior)
}

// -------------------------------------------------------------------------
// Effective-access filter: skip grants the well-known AC SIDs already cover
// -------------------------------------------------------------------------

/// SIDs every AppContainer process token implicitly belongs to. A grant to
/// any of these is observed by every AppContainer the OS launches, so a
/// per-session grant that only restates it is redundant — and, on a system
/// path this session's account does not own, would fail `WRITE_DAC` for no
/// gain.
///
/// - `S-1-15-2-1` — `APPLICATION PACKAGE AUTHORITY\ALL APPLICATION PACKAGES`.
/// - `S-1-15-2-2` — `APPLICATION PACKAGE AUTHORITY\ALL RESTRICTED APPLICATION PACKAGES`.
/// - `S-1-1-0`    — `Everyone`. AppContainer tokens DO retain Everyone; they
///   strip `Authenticated Users` and `Users`, so those are deliberately
///   omitted here.
///
/// after mxc filesystem_dacl.rs::WELL_KNOWN_AC_SIDS (0e7c3dd)
const WELL_KNOWN_AC_SIDS: &[&str] = &["S-1-15-2-1", "S-1-15-2-2", "S-1-1-0"];

fn well_known_ac_sids() -> &'static [OwnedSid] {
    static CACHE: std::sync::OnceLock<Vec<OwnedSid>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            WELL_KNOWN_AC_SIDS
                .iter()
                .map(|s| OwnedSid::parse(s).expect("well-known AC SID must parse"))
                .collect()
        })
        .as_slice()
}

/// Walk the effective DACL on `path` and compute the access mask granted to
/// a process whose only relevant identities are the well-known
/// AppContainer-membership SIDs ([`WELL_KNOWN_AC_SIDS`]). Inherited ACEs are
/// included; explicit grants to a *specific* AppContainer SID are not — the
/// caller is presumably deciding whether such a grant is needed.
///
/// Walking is canonical: a `DENY` ACE matching one of these SIDs marks bits
/// as denied, and a later `ALLOW` ACE can only add bits that have not
/// already been denied — matching Windows' own access-check order. Returns
/// 0 when the DACL is empty or NULL (treated as "grants nothing", not
/// "grants everything" — the caller falls back to attempting the real
/// grant, which may then fail `WRITE_DAC`).
///
/// after mxc filesystem_dacl.rs::compute_appcontainer_effective_access (0e7c3dd)
fn compute_appcontainer_effective_access(path: &Path) -> Result<u32, DaclError> {
    let well_known = well_known_ac_sids();
    let path_w = wide(path);

    let mut existing_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: see apply_explicit_ace — same query shape.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut existing_dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(path, "GetNamedSecurityInfoW", rc));
    }
    if existing_dacl.is_null() {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(sd);
        }
        return Ok(0);
    }

    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `info` is a stack out-param sized exactly to the class asked for.
    let ok = unsafe {
        GetAclInformation(
            existing_dacl,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        let err = io::Error::last_os_error();
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(sd);
        }
        return Err(win32_err_str(path, &format!("GetAclInformation: {err}")));
    }

    let mut allowed: u32 = 0;
    let mut denied: u32 = 0;
    for i in 0..info.AceCount {
        let mut ace_ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: `i` is within `info.AceCount`, which GetAclInformation
        // just reported for this exact ACL.
        if unsafe { GetAce(existing_dacl, i, &mut ace_ptr) } == 0 {
            continue;
        }
        // SAFETY: filled by a successful GetAce; every ACE begins with an
        // ACE_HEADER.
        let header = unsafe { &*(ace_ptr as *const ACE_HEADER) };
        let ace_type = match header.AceType {
            0x00 => AceType::Allow,
            0x01 => AceType::Deny,
            _ => continue,
        };
        let mask_and_sid = ace_ptr as *const ACCESS_ALLOWED_ACE;
        // SAFETY: same shared-prefix reasoning as scan_explicit_aces_for_sid.
        let ace_mask = unsafe { (*mask_and_sid).Mask };
        let ace_sid = (unsafe { &raw const (*mask_and_sid).SidStart }) as PSID;
        // SAFETY: both pointers reference valid SID buffers for this call.
        let matches = well_known
            .iter()
            .any(|s| unsafe { EqualSid(ace_sid, s.as_psid()) } != 0);
        if !matches {
            continue;
        }
        match ace_type {
            AceType::Deny => denied |= ace_mask & !allowed,
            AceType::Allow => allowed |= ace_mask & !denied,
        }
    }
    unsafe {
        windows_sys::Win32::Foundation::LocalFree(sd);
    }
    Ok(allowed)
}

/// Whether the well-known AppContainer-membership SIDs already grant
/// `needed_mask` on `path`, without any per-session ACE. `false` on any
/// error reading the DACL — the caller then attempts (and may fail) the
/// real grant rather than silently assuming a coverage it could not verify.
///
/// after mxc fallback_detector.rs::appcontainer_already_grants (0e7c3dd)
fn appcontainer_already_grants(path: &Path, needed_mask: u32) -> bool {
    match compute_appcontainer_effective_access(path) {
        Ok(effective) => (effective & needed_mask) == needed_mask,
        Err(_) => false,
    }
}

/// Drop every path in `paths` that the well-known AppContainer SIDs already
/// grant `needed_mask` on — a per-session ACE restating that access would
/// be redundant, and on a system path this session's account does not own
/// would fail `WRITE_DAC` for no gain. This is the gap that would otherwise
/// block W2: stamping a `system:`-sigil root (e.g. `System32`, which
/// `ALL APPLICATION PACKAGES` already reads system-wide) would fail closed
/// for a non-admin session even though nothing needed stamping.
///
/// Only grant paths are filtered here. `deny_paths` are never filtered —
/// denying is about *subtracting* access, which a well-known-group grant
/// cannot do, so every deny is still attempted (see
/// [`crate::sandbox::windows::session::confine`]).
///
/// after mxc dispatcher.rs::filter_paths_needing_grant (0e7c3dd)
pub(crate) fn filter_paths_needing_grant(paths: Vec<PathBuf>, needed_mask: u32) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|p| !appcontainer_already_grants(p, needed_mask))
        .collect()
}

/// Canonical-order bucket for an ACE (per Microsoft's documented "order of
/// ACEs in a DACL"): explicit deny, then explicit allow, then explicit
/// other, then inherited (any type, original order preserved within each
/// bucket).  Smaller bucket sorts earlier.
///
/// after mxc filesystem_dacl.rs::canonical_bucket (0e7c3dd)
fn canonical_bucket(ace_type: u8, inherited: bool) -> u8 {
    if inherited {
        3
    } else {
        match ace_type {
            0x01 => 0,
            0x00 => 1,
            _ => 2,
        }
    }
}

/// Rebuild `path`'s DACL by dropping every explicit ACE whose trustee is
/// `sid_str`, then re-appending `replay` ACEs (also for `sid_str`) in
/// canonical order.  Inherited ACEs and explicit ACEs for other trustees
/// are preserved verbatim.
///
/// `SetEntriesInAclW(REVOKE_ACCESS)` is not used for restore: on some
/// Windows builds it fails to remove explicit `ACCESS_DENIED` ACEs, leaving
/// residue after what should be a full revoke.  Manual ACL surgery here
/// gives deterministic control over what survives.
///
/// after mxc filesystem_dacl.rs::replace_explicit_aces_for_sid (0e7c3dd)
fn replace_explicit_aces_for_sid(
    path: &Path,
    sid_str: &str,
    replay: &[PriorAce],
) -> Result<(), DaclError> {
    let sid = OwnedSid::parse(sid_str)?;
    let path_w = wide(path);

    let mut existing_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: see apply_explicit_ace — same query shape.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut existing_dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(path, "GetNamedSecurityInfoW", rc));
    }

    let result = replace_explicit_aces_for_sid_inner(path, &sid, existing_dacl, replay);
    unsafe {
        windows_sys::Win32::Foundation::LocalFree(sd);
    }
    let new_acl_dwords = result?;

    let new_acl_ptr = new_acl_dwords.as_ptr() as *const ACL;
    // SAFETY: `new_acl_ptr` points at a freshly built, well-formed ACL.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_acl_ptr,
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        if rc == ERROR_ACCESS_DENIED {
            return Err(DaclError::WriteDacDenied {
                path: path.to_path_buf(),
                reason: format!(
                    "SetNamedSecurityInfoW: {}",
                    io::Error::from_raw_os_error(rc as i32)
                ),
            });
        }
        return Err(win32_err(path, "SetNamedSecurityInfoW", rc));
    }
    Ok(())
}

/// The pure-rebuild half of [`replace_explicit_aces_for_sid`]: walks the
/// existing DACL, filters explicit ACEs for our SID, adds replay ACEs, and
/// returns a `Vec<u32>` whose bytes are the new ACL (`Vec<u32>` guarantees
/// 4-byte alignment, which `InitializeAcl` requires).
///
/// after mxc filesystem_dacl.rs::replace_explicit_aces_for_sid_inner (0e7c3dd)
fn replace_explicit_aces_for_sid_inner(
    path: &Path,
    sid: &OwnedSid,
    existing_dacl: *mut ACL,
    replay: &[PriorAce],
) -> Result<Vec<u32>, DaclError> {
    enum Entry<'a> {
        Kept {
            ptr: *mut c_void,
            size: u32,
            bucket: u8,
            order: u32,
        },
        Replay {
            prior: &'a PriorAce,
            bucket: u8,
            order: u32,
        },
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut keeps_bytes: u32 = 0;
    let mut next_order: u32 = 0;

    if !existing_dacl.is_null() {
        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: `existing_dacl` is non-null and came from a successful
        // GetNamedSecurityInfoW query.
        let ok = unsafe {
            GetAclInformation(
                existing_dacl,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        // A failure here must not silently degrade to "no kept entries":
        // that would rebuild the target's DACL as if every other
        // trustee's explicit ACE (allow or deny) had never existed,
        // dropping them from a user directory. Propagate instead, matching
        // upstream (filesystem_dacl.rs::replace_explicit_aces_for_sid_inner,
        // 0e7c3dd) and this file's own `scan_explicit_aces_for_sid`, which
        // treats the same call's failure as fatal.
        if ok == 0 {
            return Err(win32_err_str(
                path,
                &format!("GetAclInformation: {}", io::Error::last_os_error()),
            ));
        }
        let inherited_bit = INHERITED_ACE as u8;
        for i in 0..info.AceCount {
            let mut ace_ptr: *mut c_void = std::ptr::null_mut();
            // SAFETY: `i` is within `info.AceCount` for this ACL.
            if unsafe { GetAce(existing_dacl, i, &mut ace_ptr) } == 0 {
                continue;
            }
            // SAFETY: filled by a successful GetAce.
            let header = unsafe { &*(ace_ptr as *const ACE_HEADER) };
            let inherited = (header.AceFlags & inherited_bit) != 0;
            let mut drop_it = false;
            if !inherited && (header.AceType == 0x00 || header.AceType == 0x01) {
                let ace_struct = ace_ptr as *const ACCESS_ALLOWED_ACE;
                // SAFETY: same shared-prefix reasoning as scan_explicit_aces_for_sid.
                let ace_sid = (unsafe { &raw const (*ace_struct).SidStart }) as PSID;
                // SAFETY: both are valid SID buffers for this call.
                if unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0 {
                    drop_it = true;
                }
            }
            if !drop_it {
                entries.push(Entry::Kept {
                    ptr: ace_ptr,
                    size: header.AceSize as u32,
                    bucket: canonical_bucket(header.AceType, inherited),
                    order: next_order,
                });
                next_order += 1;
                keeps_bytes += header.AceSize as u32;
            }
        }
    }

    for prior in replay {
        let ace_type_byte = match prior.ace_type {
            AceType::Allow => 0x00u8,
            AceType::Deny => 0x01u8,
        };
        entries.push(Entry::Replay {
            prior,
            bucket: canonical_bucket(ace_type_byte, false),
            order: next_order,
        });
        next_order += 1;
    }

    // Stable sort by (bucket, original order): preserves the original DACL
    // layout inside each bucket so unrelated ACEs never shuffle.
    entries.sort_by_key(|e| match e {
        Entry::Kept { bucket, order, .. } | Entry::Replay { bucket, order, .. } => {
            (*bucket, *order)
        }
    });

    // Per-ACE byte size: ACE_HEADER (4) + ACCESS_MASK (4) + SID bytes.
    // SAFETY: `sid` owns a valid PSID for the duration of this call.
    let sid_len: u32 = unsafe { GetLengthSid(sid.as_psid()) };
    let per_replay_size: u32 = 8 + sid_len;
    let replay_bytes: u32 = per_replay_size.saturating_mul(replay.len() as u32);

    let mut new_acl_size: u32 = std::mem::size_of::<ACL>() as u32 + keeps_bytes + replay_bytes;
    new_acl_size = (new_acl_size + 3) & !3; // DWORD-align, per ACL_SIZE_INFORMATION's units
    let min_acl_size = std::mem::size_of::<ACL>() as u32;
    if new_acl_size < min_acl_size {
        new_acl_size = min_acl_size;
    }

    let dwords = (new_acl_size as usize).div_ceil(4);
    let mut new_acl_buf: Vec<u32> = vec![0u32; dwords];
    let new_acl_ptr = new_acl_buf.as_mut_ptr() as *mut ACL;

    // SAFETY: `new_acl_buf` is a freshly allocated, zeroed, 4-byte-aligned
    // buffer at least `new_acl_size` bytes long.
    if unsafe { InitializeAcl(new_acl_ptr, new_acl_size, ACL_REVISION) } == 0 {
        return Err(win32_err_str(
            path,
            &format!("InitializeAcl: {}", io::Error::last_os_error()),
        ));
    }

    // Both `AddAce` and the typed `AddAccess{Allowed,Denied}AceEx` family
    // append at the tail of the ACL despite the latter's name — neither
    // canonicalizes on insert — so emission order here drives the final
    // canonical order directly.
    for entry in &entries {
        match entry {
            Entry::Kept { ptr, size, .. } => {
                // SAFETY: `new_acl_ptr` was just initialized above; `ptr`
                // points at a live ACE of exactly `size` bytes from the
                // still-valid `existing_dacl` query.
                if unsafe { AddAce(new_acl_ptr, ACL_REVISION, u32::MAX, *ptr, *size) } == 0 {
                    return Err(win32_err_str(
                        path,
                        &format!("AddAce(keep): {}", io::Error::last_os_error()),
                    ));
                }
            }
            Entry::Replay { prior, .. } => {
                let flags = prior.inherit_flags as u32;
                // SAFETY: `new_acl_ptr` is valid and has room (sized above
                // to include every replay entry); `sid` is a valid PSID.
                let ok = unsafe {
                    match prior.ace_type {
                        AceType::Allow => AddAccessAllowedAceEx(
                            new_acl_ptr,
                            ACL_REVISION,
                            flags,
                            prior.access_mask,
                            sid.as_psid(),
                        ),
                        AceType::Deny => AddAccessDeniedAceEx(
                            new_acl_ptr,
                            ACL_REVISION,
                            flags,
                            prior.access_mask,
                            sid.as_psid(),
                        ),
                    }
                };
                if ok == 0 {
                    let op = match prior.ace_type {
                        AceType::Allow => "AddAccessAllowedAceEx",
                        AceType::Deny => "AddAccessDeniedAceEx",
                    };
                    return Err(win32_err_str(
                        path,
                        &format!("{op}: {}", io::Error::last_os_error()),
                    ));
                }
            }
        }
    }

    Ok(new_acl_buf)
}

// -------------------------------------------------------------------------
// Win32: process-alive check (orphan recovery)
// -------------------------------------------------------------------------

/// RAII wrapper around a process `HANDLE`.
struct ProcessHandleGuard(HANDLE);

impl Drop for ProcessHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a valid handle opened by `OpenProcess`.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Liveness probe for orphan recovery.  Returns `true` if a process with
/// `pid` is running, has the expected image basename, and (when
/// `expected_start_filetime` is `Some` and non-zero) has a kernel-recorded
/// creation time exactly equal to the recorded value — `GetProcessTimes`
/// returns a fixed timestamp for a process's whole lifetime, so exact
/// equality is the right test; any deviation means a different process now
/// holds the recorded PID (PID reuse).  `None`/`Some(0)` preserves
/// PID-and-image-only liveness for ledgers written before this field
/// existed.
///
/// after mxc filesystem_dacl.rs::process_alive_with_image (0e7c3dd)
fn process_alive_with_image(
    pid: u32,
    expected_image: &str,
    expected_start_filetime: Option<u64>,
) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: no preconditions beyond a valid pid, which the OS validates.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let handle = ProcessHandleGuard(handle);

    let mut buf = [0u16; 1024];
    let mut sz: u32 = buf.len() as u32;
    // SAFETY: `handle.0` is valid (checked above); `buf`/`sz` are stack
    // locals sized to the buffer.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_FORMAT::default(),
            buf.as_mut_ptr(),
            &mut sz,
        )
    };
    if ok == 0 || sz == 0 {
        return false;
    }
    let full = String::from_utf16_lossy(&buf[..sz as usize]);
    let basename = Path::new(&full)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !basename.eq_ignore_ascii_case(expected_image) {
        return false;
    }
    let recorded = match expected_start_filetime {
        Some(0) | None => return true,
        Some(v) => v,
    };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all four out-params are stack locals of the right type.
    let gpt =
        unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) };
    if gpt == 0 {
        return false;
    }
    let live = ((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64);
    live == recorded
}

/// Process creation time of the *current* process as a Windows FILETIME
/// (100-ns intervals since 1601-01-01 UTC).  `GetCurrentProcess` returns a
/// pseudo-handle that does not need closing.
///
/// after mxc filesystem_dacl.rs::process_creation_filetime (0e7c3dd)
fn process_creation_filetime() -> Result<u64, DaclError> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle needing no close;
    // the four out-params are stack locals of the right type.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return Err(DaclError::Win32 {
            path: PathBuf::new(),
            reason: format!(
                "GetProcessTimes(GetCurrentProcess): {}",
                io::Error::last_os_error()
            ),
        });
    }
    Ok(((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64))
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    #[test]
    fn ledger_roundtrip() {
        let l = Ledger {
            run_id: "pid-42-deadbeef".into(),
            pid: 42,
            image_name: "ral.exe".into(),
            started_at_filetime: 132_000_000_000_000_000,
            applied: vec![AppliedAce {
                canonical_path: PathBuf::from(r"\\?\C:\tmp\foo"),
                sid_string: "S-1-15-2-1-2-3-4-5-6-7".into(),
                access_mask: 0x12_3456,
                ace_type: AceType::Allow,
                inheritable: true,
                prior_state: vec![PriorAce {
                    ace_type: AceType::Allow,
                    access_mask: 0x01FF,
                    inherit_flags: (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8,
                }],
            }],
        };
        let bytes = serde_json::to_vec(&l).unwrap();
        let parsed: Ledger = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.run_id, l.run_id);
        assert_eq!(parsed.pid, l.pid);
        assert_eq!(parsed.started_at_filetime, l.started_at_filetime);
        assert_eq!(parsed.applied.len(), 1);
        assert_eq!(parsed.applied[0].access_mask, 0x12_3456);
        assert_eq!(parsed.applied[0].ace_type, AceType::Allow);
        assert_eq!(parsed.applied[0].prior_state.len(), 1);
        assert_eq!(parsed.applied[0].prior_state[0].access_mask, 0x01FF);
    }

    #[test]
    fn ledger_back_compat_no_prior_state_field() {
        let json = br#"{
            "run_id": "pid-1-old",
            "pid": 1,
            "image_name": "ral.exe",
            "started_at_filetime": 132000000000000000,
            "applied": [{
                "canonical_path": "\\\\?\\C:\\tmp\\foo",
                "sid_string": "S-1-1-0",
                "access_mask": 1,
                "ace_type": "Allow",
                "inheritable": false
            }]
        }"#;
        let parsed: Ledger = serde_json::from_slice(json).expect("legacy ledger must parse");
        assert!(parsed.applied[0].prior_state.is_empty());
    }

    // The RW_MASK/RO_MASK shape invariants (superset relationship,
    // FILE_TRAVERSE presence) are compile-time `const _: () = assert!(...)`
    // checks at the mask declarations above, not a runtime test: a value
    // drift fails the build itself rather than silently passing on a host
    // where the test suite is filtered.

    #[test]
    fn canonical_bucket_orders_deny_before_allow_before_other_before_inherited() {
        assert!(canonical_bucket(0x01, false) < canonical_bucket(0x00, false));
        assert!(canonical_bucket(0x00, false) < canonical_bucket(0x02, false));
        assert!(canonical_bucket(0x02, false) < canonical_bucket(0x00, true));
        assert!(canonical_bucket(0x01, true) == canonical_bucket(0x00, true));
    }

    #[test]
    fn ensure_local_canonical_prefix_accepts_local() {
        assert!(ensure_local_canonical_prefix(Path::new(r"\\?\C:\tmp\foo")).is_ok());
        assert!(
            ensure_local_canonical_prefix(Path::new(
                r"\\.\Volume{12345678-1234-1234-1234-123456789abc}\foo"
            ))
            .is_ok()
        );
        assert!(ensure_local_canonical_prefix(Path::new(r"\\.\C:\tmp\foo")).is_ok());
        assert!(ensure_local_canonical_prefix(Path::new(r"C:\tmp\foo")).is_ok());
    }

    #[test]
    fn ensure_local_canonical_prefix_rejects_unc_namespace() {
        let err = ensure_local_canonical_prefix(Path::new(r"\\?\UNC\server\share\foo"));
        assert!(matches!(err, Err(DaclError::NetworkPathRejected(_))));
        let err = ensure_local_canonical_prefix(Path::new(r"\\?\unc\server\share\foo"));
        assert!(matches!(err, Err(DaclError::NetworkPathRejected(_))));
    }

    #[test]
    fn ensure_local_canonical_prefix_rejects_raw_unc() {
        let err = ensure_local_canonical_prefix(Path::new(r"\\server\share\foo"));
        assert!(matches!(err, Err(DaclError::NetworkPathRejected(_))));
    }

    #[test]
    fn mutex_name_is_deterministic_and_local_prefix() {
        let p = PathBuf::from(r"C:\tmp\foo");
        let n1 = mutex_name_for(&p);
        let n2 = mutex_name_for(&p);
        assert_eq!(n1, n2);
        assert!(n1.starts_with("Local\\ral.sandbox.dacl."));
        assert_eq!(n1.len(), "Local\\ral.sandbox.dacl.".len() + 16);
    }

    #[test]
    fn mutex_name_case_insensitive() {
        let a = mutex_name_for(&PathBuf::from(r"C:\Tmp\Foo"));
        let b = mutex_name_for(&PathBuf::from(r"c:\tmp\foo"));
        assert_eq!(a, b);
    }

    #[test]
    fn run_id_format() {
        let id = generate_run_id();
        assert!(id.starts_with("pid-"));
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2].len(), 16);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sid_parse_valid_and_invalid() {
        let ok = OwnedSid::parse("S-1-1-0");
        assert!(ok.is_ok(), "S-1-1-0 should parse: {:?}", ok.err());
        drop(ok);
        let err = OwnedSid::parse("not-a-sid");
        assert!(matches!(err, Err(DaclError::InvalidSid(_))));
    }

    #[test]
    fn sid_parse_rejects_empty_and_oversized() {
        let empty = OwnedSid::parse("");
        assert!(matches!(empty, Err(DaclError::InvalidSid(_))));
        let huge = "S-1-".to_string() + &"1".repeat(MAX_SID_STRING_LEN);
        let err = OwnedSid::parse(&huge);
        assert!(matches!(err, Err(DaclError::InvalidSid(_))));
    }

    #[test]
    fn process_creation_time_is_after_unix_epoch_and_not_in_the_future() {
        let t = process_creation_filetime().expect("GetProcessTimes should succeed");
        const UNIX_EPOCH_AS_FILETIME: u64 = 11_644_473_600 * 10_000_000;
        assert!(
            t > UNIX_EPOCH_AS_FILETIME,
            "process_creation_filetime ({t}) should be > Unix epoch ({UNIX_EPOCH_AS_FILETIME})"
        );
        // Pure arithmetic "now" in FILETIME units (no extra Win32 feature
        // needed just for this bound check).
        let now_ft = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(u64::MAX, |d| {
                UNIX_EPOCH_AS_FILETIME + d.as_nanos() as u64 / 100
            });
        const TEN_SECONDS_TICKS: u64 = 10 * 10_000_000;
        assert!(
            t <= now_ft.saturating_add(TEN_SECONDS_TICKS),
            "process_creation_filetime ({t}) must not be after now ({now_ft})"
        );
    }

    #[test]
    fn recovery_ignores_active_pid() {
        let pid = std::process::id();
        let img = current_image_basename();
        let start = process_creation_filetime().unwrap();
        assert!(process_alive_with_image(pid, &img, Some(start)));
        assert!(process_alive_with_image(pid, &img, None));
        assert!(process_alive_with_image(pid, &img, Some(0)));
    }

    #[test]
    fn recovery_detects_dead_pid() {
        assert!(!process_alive_with_image(0, "anything.exe", None));
        assert!(!process_alive_with_image(0x7FFF_FFFE, "ral.exe", None));
    }

    #[test]
    fn recovery_detects_pid_reuse_via_creation_time() {
        let pid = std::process::id();
        let img = current_image_basename();
        const Y2K_FILETIME: u64 = 125_911_584_000_000_000;
        assert!(!process_alive_with_image(pid, &img, Some(Y2K_FILETIME)));
    }

    #[test]
    fn recovery_detects_pid_reuse_off_by_one_tick() {
        let pid = std::process::id();
        let img = current_image_basename();
        let real = process_creation_filetime().unwrap();
        assert!(!process_alive_with_image(
            pid,
            &img,
            Some(real.saturating_add(1))
        ));
        assert!(!process_alive_with_image(
            pid,
            &img,
            Some(real.saturating_sub(1))
        ));
    }

    // ---------------- integration tests -----------------
    //
    // These mutate real filesystem ACLs under a tempdir and run only on
    // Windows CI (this whole module is cfg(windows)-gated).  Each scopes
    // XDG_STATE_HOME to a fresh tempdir, serialized by `ENV_LOCK` so
    // `RUST_TEST_THREADS > 1` (`.cargo/config.toml`) can't interleave two
    // tests' env mutations.  `crate::test_env` is Unix-only (its only
    // consumers today are `cfg(unix)` tests), so this module keeps its own
    // copy of the same pattern rather than lifting that gate.

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_scoped_state_dir<R>(f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let scratch = tempfile::tempdir().unwrap();
        let state_home = scratch.path().to_string_lossy().into_owned();
        let prev = std::env::var_os("XDG_STATE_HOME");
        // SAFETY: serialized by ENV_LOCK above; no other thread in this
        // process reads/writes XDG_STATE_HOME while the guard is held.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", &state_home);
        }
        let out = f();
        // SAFETY: same guard, restoring the pre-scope value.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
        out
    }

    #[test]
    fn state_dir_honors_xdg_override() {
        with_scoped_state_dir(|| {
            let dir = ledger_dir();
            let env = std::env::var_os("XDG_STATE_HOME").unwrap();
            assert!(dir.starts_with(PathBuf::from(env)));
            assert!(dir.ends_with("ral/sandbox-dacl") || dir.ends_with(r"ral\sandbox-dacl"));
        });
    }

    #[test]
    fn apply_allow_then_restore_temp_dir() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let mut m = DaclManager::new().unwrap();
            m.grant_appcontainer_access("S-1-1-0", &[td.path().to_path_buf()], &[])
                .unwrap();
            m.restore().unwrap();
            assert!(m.applied.is_empty());
        });
    }

    #[test]
    fn apply_overlapping_then_restore_does_not_leak_rights() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let target = td.path().to_path_buf();
            let everyone = "S-1-1-0";

            // First apply: RO. Second apply via a separate manager: RW.
            // The two grants merge in the DACL (same trustee), so restoring
            // the second manager must use its captured prior_state (the
            // first grant's ACE) to unwind correctly.
            let mut outer = DaclManager::new().unwrap();
            outer
                .grant_appcontainer_access(everyone, &[], std::slice::from_ref(&target))
                .unwrap();

            let mut inner = DaclManager::new().unwrap();
            inner
                .grant_appcontainer_access(everyone, std::slice::from_ref(&target), &[])
                .unwrap();

            let captured = &inner.applied.last().unwrap().prior_state;
            assert!(
                !captured.is_empty(),
                "inner.prior_state should contain outer's ACE: {captured:?}"
            );

            inner.restore().unwrap();
            outer.restore().unwrap();
        });
    }

    #[test]
    fn inheritance_propagation_applies_without_error() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let sub = td.path().join("sub");
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("file.txt"), b"x").unwrap();
            let mut m = DaclManager::new().unwrap();
            m.grant_appcontainer_access("S-1-1-0", &[td.path().to_path_buf()], &[])
                .unwrap();
            m.restore().unwrap();
        });
    }

    #[test]
    fn drop_calls_restore() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            {
                let mut m = DaclManager::new().unwrap();
                m.grant_appcontainer_access("S-1-1-0", &[td.path().to_path_buf()], &[])
                    .unwrap();
                // No explicit restore — Drop should clean up.
            }
        });
    }

    #[test]
    fn nonexistent_path_errors_cleanly() {
        with_scoped_state_dir(|| {
            let mut m = DaclManager::new().unwrap();
            let err = m.grant_appcontainer_access(
                "S-1-1-0",
                &[PathBuf::from(r"C:\__definitely_not_a_real_path__\xyzzy")],
                &[],
            );
            assert!(matches!(err, Err(DaclError::PathNotFound(_))));
        });
    }

    #[test]
    fn network_path_rejected_e2e() {
        with_scoped_state_dir(|| {
            let mut m = DaclManager::new().unwrap();
            let err = m.grant_appcontainer_access(
                "S-1-1-0",
                &[PathBuf::from(r"\\someserver\share\foo")],
                &[],
            );
            // The exact variant depends on how canonicalisation resolves an
            // unreachable server name on the test host; all three are
            // "never silently succeeds on a UNC path".
            // `ensure_local_canonical_prefix`'s own unit tests above cover
            // the classification deterministically.
            match &err {
                Err(DaclError::NetworkPathRejected(_))
                | Err(DaclError::PathNotFound(_))
                | Err(DaclError::Win32 { .. }) => {}
                other => panic!(
                    "expected NetworkPathRejected | PathNotFound | Win32 for UNC path, got: {other:?}"
                ),
            }
        });
    }

    #[test]
    fn crash_recovery_synthetic_dead_pid() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let target = td.path().join("victim");
            std::fs::create_dir(&target).unwrap();
            {
                let mut m = DaclManager::new().unwrap();
                m.grant_appcontainer_access("S-1-1-0", std::slice::from_ref(&target), &[])
                    .unwrap();
                // Forge a ledger with a dead PID so recovery picks it up,
                // then tell our manager not to also restore (isolating the
                // recovery path).
                let dir = ensure_ledger_dir().unwrap();
                let synthetic = dir.join("pid-2147483646-orphan.json");
                let l = Ledger {
                    run_id: "pid-2147483646-orphan".into(),
                    pid: 0x7FFF_FFFE,
                    image_name: "ral.exe".into(),
                    started_at_filetime: 0,
                    applied: m.applied.clone(),
                };
                write_ledger(&synthetic, &l).unwrap();
                m.applied.clear();
            }
            let report = recover_orphaned_state().unwrap();
            assert!(report.files_processed >= 1);
        });
    }

    #[test]
    fn recovery_prunes_ace_whose_target_is_gone() {
        with_scoped_state_dir(|| {
            let dir = ensure_ledger_dir().unwrap();
            let missing = dir.join("does-not-exist-victim");
            assert!(matches!(missing.try_exists(), Ok(false)));

            let synthetic = dir.join("pid-2147483646-missing.json");
            let l = Ledger {
                run_id: "pid-2147483646-missing".into(),
                pid: 0x7FFF_FFFE,
                image_name: "ral.exe".into(),
                started_at_filetime: 0,
                applied: vec![AppliedAce {
                    canonical_path: missing,
                    sid_string: "S-1-1-0".into(),
                    access_mask: RW_MASK,
                    ace_type: AceType::Allow,
                    inheritable: false,
                    prior_state: Vec::new(),
                }],
            };
            write_ledger(&synthetic, &l).unwrap();

            let report = recover_orphaned_state().unwrap();
            assert!(
                report.aces_pruned_missing >= 1,
                "missing-target ACE should be pruned"
            );
            assert!(
                report.errors.is_empty(),
                "a pruned entry must not surface as an error: {:?}",
                report.errors
            );
            assert!(matches!(synthetic.try_exists(), Ok(false)));
        });
    }

    /// Full DACL walk (inherited ACEs included, unlike
    /// [`scan_explicit_aces_for_sid`]) returning, in ACL order, the
    /// `(AceType byte, inherited?)` of every allow/deny ACE for `sid_str` —
    /// so a test can assert canonical ordering.
    fn sid_ace_sequence(path: &Path, sid_str: &str) -> Vec<(u8, bool)> {
        let sid = OwnedSid::parse(sid_str).unwrap();
        let path_w = wide(path);
        let mut existing_dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = unsafe {
            GetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut existing_dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "GetNamedSecurityInfoW failed");
        let mut out = Vec::new();
        if !existing_dacl.is_null() {
            let mut info = ACL_SIZE_INFORMATION::default();
            unsafe {
                GetAclInformation(
                    existing_dacl,
                    &mut info as *mut _ as *mut c_void,
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                );
            }
            let inherited_bit = INHERITED_ACE as u8;
            for i in 0..info.AceCount {
                let mut ace_ptr: *mut c_void = std::ptr::null_mut();
                if unsafe { GetAce(existing_dacl, i, &mut ace_ptr) } == 0 {
                    continue;
                }
                let header = unsafe { &*(ace_ptr as *const ACE_HEADER) };
                if header.AceType != 0x00 && header.AceType != 0x01 {
                    continue;
                }
                let ace_struct = ace_ptr as *const ACCESS_ALLOWED_ACE;
                let ace_sid = (unsafe { &raw const (*ace_struct).SidStart }) as PSID;
                if unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0 {
                    out.push((header.AceType, (header.AceFlags & inherited_bit) != 0));
                }
            }
        }
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(sd);
        }
        out
    }

    /// A deny stamped on a path nested inside a granted (inheritable) prefix
    /// produces an explicit deny-ACE that precedes the inherited allow in
    /// canonical order — so the deny wins — and is fully reverted on restore.
    #[test]
    fn deny_nested_in_grant_overrides_inherited_allow_and_reverts() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let parent = td.path().to_path_buf();
            let child = parent.join("cred");
            std::fs::create_dir(&child).unwrap();
            let sid = "S-1-1-0";

            let mut m = DaclManager::new().unwrap();
            m.grant_appcontainer_access(sid, std::slice::from_ref(&parent), &[])
                .unwrap();
            m.add_deny_aces(sid, std::slice::from_ref(&child)).unwrap();

            // Exactly one explicit (non-inherited) ACE for the SID on the
            // child, and it is a deny.
            let explicit = scan_explicit_aces_for_sid(&child, sid).unwrap();
            assert_eq!(explicit.len(), 1, "one explicit ACE expected: {explicit:?}");
            assert_eq!(explicit[0].ace_type, AceType::Deny);

            // Canonical order: the explicit deny precedes the inherited allow.
            let seq = sid_ace_sequence(&child, sid);
            let deny_idx = seq
                .iter()
                .position(|(t, inh)| *t == 0x01 && !*inh)
                .expect("explicit deny present in DACL");
            if let Some(allow_idx) = seq.iter().position(|(t, _)| *t == 0x00) {
                assert!(deny_idx < allow_idx, "deny must precede allow: {seq:?}");
            }

            m.restore().unwrap();
            assert!(m.applied.is_empty());
            let after = scan_explicit_aces_for_sid(&child, sid).unwrap();
            assert!(
                after.is_empty(),
                "restore must remove our explicit deny: {after:?}"
            );
        });
    }

    /// The deny stamp goes through the same ledger as a grant: recorded in
    /// memory and persisted to disk with `AceType::Deny`.
    #[test]
    fn deny_ace_recorded_in_ledger() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let parent = td.path().to_path_buf();
            let child = parent.join("secret");
            std::fs::create_dir(&child).unwrap();
            let sid = "S-1-1-0";

            let mut m = DaclManager::new().unwrap();
            m.grant_appcontainer_access(sid, std::slice::from_ref(&parent), &[])
                .unwrap();
            m.add_deny_aces(sid, std::slice::from_ref(&child)).unwrap();

            assert!(
                m.applied.iter().any(|a| a.ace_type == AceType::Deny),
                "in-memory ledger must record the deny stamp"
            );
            let persisted = read_ledger(&m.ledger_path).unwrap();
            assert!(
                persisted.applied.iter().any(|a| a.ace_type == AceType::Deny),
                "on-disk ledger must record the deny stamp"
            );

            m.restore().unwrap();
        });
    }

    /// A fresh temp dir grants the well-known AC SIDs nothing, so the
    /// filter keeps it; once `Everyone` is explicitly granted the needed
    /// mask, the filter drops it as redundant. Mirrors upstream's
    /// `filter_paths_needing_grant_drops_well_known_grant`
    /// (dispatcher.rs, 0e7c3dd).
    #[test]
    fn filter_paths_needing_grant_drops_well_known_grant() {
        with_scoped_state_dir(|| {
            let td_grant = tempfile::tempdir().unwrap();
            let td_no_grant = tempfile::tempdir().unwrap();

            let mut mgr = DaclManager::new().unwrap();
            mgr.grant_appcontainer_access(
                "S-1-1-0",
                std::slice::from_ref(&td_grant.path().to_path_buf()),
                &[],
            )
            .unwrap();

            let input = vec![
                td_grant.path().to_path_buf(),
                td_no_grant.path().to_path_buf(),
            ];
            let kept = filter_paths_needing_grant(input, RW_MASK);
            assert!(
                !kept.iter().any(|p| p == td_grant.path()),
                "already-granted path should be filtered out: kept={kept:?}"
            );
            assert!(
                kept.iter().any(|p| p == td_no_grant.path()),
                "non-granted path should survive the filter: kept={kept:?}"
            );

            mgr.restore().unwrap();
        });
    }

    /// `compute_appcontainer_effective_access` returns 0 on a fresh temp
    /// dir (no well-known-SID grant present).
    #[test]
    fn effective_access_is_zero_on_fresh_dir() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let access = compute_appcontainer_effective_access(td.path()).unwrap();
            assert_eq!(access, 0);
        });
    }
}
