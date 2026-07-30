// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings. Names match upstream's unless a comment says
// otherwise, so a unit here diffs against the one it was taken from.

//! Crash-safe grant-ACE apply/restore for the `AppContainer` sandbox backend.
//!
//! `DaclManager` stamps allow-ACEs for an `AppContainer` SID onto host
//! filesystem prefixes so a LowBox-token child can reach them, and reverts
//! every stamp it made. `session` holds exactly one manager per shell session
//! and its `teardown` is what finally restores; authority, though, is
//! projection-keyed — one SID per distinct fs projection, so a child's
//! kernel-enforced reach is its own projection, never the union of what the
//! session stamped. ACEs consequently outlive the grant frame that asked for
//! them: a detached worker keeps the authority it was born with, and a SID no
//! live child holds is inert, since only this session's spawns can mint one.
//!
//! Per grant the ordering is: take the path's named mutex, scan the explicit
//! ACEs our SID already holds, **persist the ledger, then** apply. A process
//! that dies mid-sequence leaves in the XDG state directory everything needed
//! to undo an apply that may or may not have landed; `recover_orphaned_state`
//! sweeps those ledgers at the next session's boot. A restore that fails keeps
//! its entry, in memory and on disk, so a later attempt retries it.
//!
//! Two concurrent stampers must never share an `AppContainer` SID — the
//! merge-then-restore dance below defends only against *sequential*
//! overlapping grants on one path. `session` guarantees it by naming profiles
//! from its pid plus a per-session counter, and serializing all stamping under
//! its lock.

use std::ffi::c_void;
use std::fs;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::process::cancel::{CancelCause, CancelScope};

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

/// `ACL_SIZE_INFORMATION`'s byte count, as the `u32` [`GetAclInformation`]
/// wants for `nAclInformationLength`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a fixed three-DWORD Win32 layout; the size is a compile-time constant of 12, nowhere near u32::MAX"
)]
const ACL_SIZE_INFORMATION_CB: u32 = std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32;

/// An empty `ACL` header's byte count — the floor every ACL built here is
/// sized up from, as the `u32` [`InitializeAcl`] wants.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a fixed eight-byte Win32 header; the size is a compile-time constant nowhere near u32::MAX"
)]
const ACL_CB: u32 = std::mem::size_of::<ACL>() as u32;

/// [`INHERITED_ACE`] as the `u8` an `ACE_HEADER`'s `AceFlags` field is:
/// windows-sys types the flag constants `u32`, but the field is one byte.
#[allow(
    clippy::cast_possible_truncation,
    reason = "INHERITED_ACE is 0x10, and AceFlags — the field it is tested against — is a single byte"
)]
const INHERITED_ACE_FLAG: u8 = INHERITED_ACE as u8;

/// Errors returned by [`DaclManager`] and [`recover_orphaned_state`].
#[derive(Debug)]
pub enum DaclError {
    NetworkPathRejected(PathBuf),
    PathNotFound(PathBuf),
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
    /// A cancel landed between two paths' stamps.  Not a failure: the ACEs
    /// already applied are on the ledger, so `restore` — or the next boot's
    /// sweep — takes them off exactly as it would after a completed grant.
    Cancelled(CancelCause),
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
/// against the `SetNamedSecurityInfoW` it guards.
fn cancelled(cancel: &CancelScope) -> Result<(), DaclError> {
    match cancel.cause() {
        Some(cause) => Err(DaclError::Cancelled(cause)),
        None => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum AceType {
    Allow,
    Deny,
}

/// One explicit (non-inherited) ACE the target carried before we applied,
/// including a deny some other tool left. `SetEntriesInAclW` coalesces rights
/// per trustee, so our stamp merges into any ACE our SID already had and no
/// revoke can pick it back apart; restore rebuilds from these instead.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct PriorAce {
    ace_type: AceType,
    access_mask: u32,
    /// Raw `AceFlags` with `INHERITED_ACE` masked off.
    inherit_flags: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppliedAce {
    canonical_path: PathBuf,
    sid_string: String,
    access_mask: u32,
    ace_type: AceType,
    /// Whether `OI|CI` were set (directories only).
    inheritable: bool,
    /// Empty means restore has nothing to replay and simply drops our SID's
    /// explicit ACEs; `default` so a ledger written without the field reads
    /// that way too.
    #[serde(default)]
    prior_state: Vec<PriorAce>,
}

/// Upstream calls this `StateFile`, and its I/O `write_state_file` /
/// `read_state_file`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ledger {
    run_id: String,
    pid: u32,
    image_name: String,
    /// The owner's creation time as a Windows FILETIME. Recovery matches it
    /// against the live process's, so a recycled PID never looks alive.
    started_at_filetime: u64,
    applied: Vec<AppliedAce>,
    /// `AppContainer` profile names, recorded before the OS-level create so
    /// the orphan sweep deletes a crashed session's profiles alongside its
    /// ACEs. `default` so a ledger written without the field still parses.
    #[serde(default)]
    profiles: Vec<String>,
}

/// Aggregated outcome of [`recover_orphaned_state`].
#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub files_processed: usize,
    pub aces_restored: usize,
    pub profiles_deleted: usize,
    /// ACEs dropped because their target no longer exists: nothing to restore
    /// on a deleted file, so retrying it every boot would be forever.
    pub aces_pruned_missing: usize,
    pub errors: Vec<String>,
}

/// Crash-safe guard for filesystem DACL grants: grant into it under each
/// projection's own SID, then [`restore`](Self::restore) or just drop it.
/// `session` keeps one per shell session, never one per spawn, and ledgers
/// its `AppContainer` profile registrations here as well.
#[derive(Debug)]
pub struct DaclManager {
    run_id: String,
    ledger_path: PathBuf,
    applied: Vec<AppliedAce>,
    profiles: Vec<String>,
    warnings: Vec<String>,
    process_start_filetime: u64,
}

impl DaclManager {
    /// Creates the ledger directory if missing; the ledger file itself waits
    /// until there is something to record.
    pub fn new() -> Result<Self, DaclError> {
        let dir = ensure_ledger_dir()?;
        let run_id = generate_run_id();
        let ledger_path = dir.join(format!("{run_id}.json"));
        let process_start_filetime = process_creation_filetime()?;
        Ok(Self {
            run_id,
            ledger_path,
            applied: Vec::new(),
            profiles: Vec::new(),
            warnings: Vec::new(),
            process_start_filetime,
        })
    }

    /// Durably record a profile name *before* the caller registers it with
    /// the OS — the same ledger-before-mutation ordering ACEs get. A crash in
    /// between leaves recovery trying to delete a profile that never existed,
    /// which is the harmless direction to fail in.
    pub fn record_profile(&mut self, name: &str) -> Result<(), DaclError> {
        self.profiles.push(name.to_string());
        if let Err(e) = self.persist_ledger() {
            self.profiles.pop();
            return Err(e);
        }
        Ok(())
    }

    /// Call once the OS-level profile is gone. The ledger file disappears
    /// outright when nothing — no ACE, no profile — is left recorded.
    pub fn forget_profile(&mut self, name: &str) -> Result<(), DaclError> {
        self.profiles.retain(|p| p != name);
        self.checkpoint()
    }

    fn checkpoint(&self) -> Result<(), DaclError> {
        if self.applied.is_empty() && self.profiles.is_empty() {
            remove_ledger_best_effort(&self.ledger_path);
            Ok(())
        } else {
            self.persist_ledger()
        }
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Stamp an allow-ACE for `sid_str` on every prefix given.
    ///
    /// `cancel` is polled between paths, which is as fine-grained as this gets:
    /// one path is a single `SetNamedSecurityInfoW`, which propagates the
    /// inheritable ACE into every existing descendant before returning —
    /// seconds on a build tree, and not interruptible from here.
    /// `TreeSetNamedSecurityInfoW` takes a progress callback that *can* abort
    /// mid-walk, and is the way to make this promptly preemptible.
    pub fn grant_appcontainer_access(
        &mut self,
        sid_str: &str,
        readwrite: &[PathBuf],
        readonly: &[PathBuf],
        cancel: &CancelScope,
    ) -> Result<(), DaclError> {
        for p in readwrite {
            cancelled(cancel)?;
            self.apply_one(sid_str, p, RW_MASK, AceType::Allow)?;
        }
        for p in readonly {
            cancelled(cancel)?;
            self.apply_one(sid_str, p, RO_MASK, AceType::Allow)?;
        }
        Ok(())
    }

    /// Stamp an explicit deny-ACE for `sid_str` over the whole
    /// `FILE_ALL_ACCESS` surface. Explicit deny precedes inherited allow in
    /// canonical ACL order, so a deny nested inside a granted prefix beats the
    /// enclosing grant. `session::confine` stamps every declared deny path,
    /// including ones outside every grant: an `AppContainer` token still
    /// carries `Everyone` and the system-wide `ALL APPLICATION PACKAGES`
    /// grants, so "outside our grants" is not the same as unreachable.
    pub fn add_deny_aces(
        &mut self,
        sid_str: &str,
        denied: &[PathBuf],
        cancel: &CancelScope,
    ) -> Result<(), DaclError> {
        // FILE_ALL_ACCESS = STANDARD_RIGHTS_REQUIRED | SYNCHRONIZE | 0x1FF.
        let deny_mask: u32 = 0x001F_01FF;
        for p in denied {
            cancelled(cancel)?;
            self.apply_one(sid_str, p, deny_mask, AceType::Deny)?;
        }
        Ok(())
    }

    /// Idempotently remove every ACE this manager applied. A failure on one
    /// path blocks none of the others: it lands in
    /// [`warnings`](Self::warnings) and its entry is kept, in memory and on
    /// the ledger, for a later `restore` or the next boot's sweep to retry.
    /// Only ledger I/O is fatal enough to surface as `Err`.
    pub fn restore(&mut self) -> Result<(), DaclError> {
        // Tail-first: the last ACE applied is the first removed.
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
        // Pushed newest-first; reverse back to apply order so a retry again
        // goes tail-first.
        remaining.reverse();
        self.applied = remaining;
        self.checkpoint()
    }

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

        // Held for the whole scan-persist-apply sequence, so no other ral
        // session's stamper interleaves with it.
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

        // Ledger before apply: dying right here must still leave recovery
        // able to undo a Win32 call that may or may not have run.
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
            profiles: self.profiles.clone(),
        };
        write_ledger(&self.ledger_path, &ledger)
    }
}

impl Drop for DaclManager {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            crate::diagnostic::shell_warning(&format!(
                "sandbox DACL guard: restore on drop failed: {e}"
            ));
        }
    }
}

/// Reap every ledger whose owning process is gone, restoring the DACL state
/// it recorded and deleting the profiles it registered. `session::boot_recover`
/// runs this once at session start, before any grant is applied.
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
        // Never retained for retry. The ledger precedes the OS-level create,
        // so a recorded profile may never have existed; and an undeletable one
        // is inert, since only a spawn from this codebase can acquire its SID.
        for name in &ledger.profiles {
            match super::appcontainer::delete_profile_by_name(name) {
                Ok(()) => report.profiles_deleted += 1,
                Err(e) => report.errors.push(format!("delete profile {name}: {e}")),
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
                profiles: Vec::new(),
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

/// Under the XDG `state` base rather than a hardcoded `%LOCALAPPDATA%`, so
/// `XDG_STATE_HOME` relocates it — which is how the tests get a private one.
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

/// Stage to `<path>.tmp`, `fsync`, then rename over the destination, so the
/// next boot's sweep sees either the previous complete ledger or this one,
/// never a half-written file.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:dacl-ledger-write] Atomic ledger write for the DACL crash-safety protocol: stage to <path>.tmp, fsync, rename over the destination. Sandbox infrastructure, not model data I/O."
)]
fn write_ledger(path: &Path, ledger: &Ledger) -> Result<(), DaclError> {
    let json = serde_json::to_vec_pretty(ledger)
        .map_err(|e| DaclError::LedgerParse(format!("serialize: {e}")))?;
    let tmp = tmp_path_for(path);
    // A tmp left by a crashed write would fail `create_new` below with
    // ERROR_FILE_EXISTS; clearing it is best-effort.
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

/// Retries `ERROR_SHARING_VIOLATION`/`ERROR_LOCK_VIOLATION`: a concurrent
/// writer mid-rename, or an on-access virus scanner, holds the file for a
/// moment, and without the retry a good ledger gets quarantined as corrupt.
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
                    Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
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
        h ^= u64::from(*b);
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

/// Owned PSID from `ConvertStringSidToSidW`, freed with `LocalFree`.
struct OwnedSid(PSID);

// SAFETY: the buffer is a private, immutable `LocalAlloc` — nothing else in
// the process holds or mutates it — and every use is a read-only Win32 call
// (`EqualSid`, `AddAccessAllowedAceEx`, …), so `well_known_ac_sids`'s
// process-wide cache may share the pointer across threads. The `LocalFree`
// still belongs to whichever `OwnedSid` is dropped.
unsafe impl Send for OwnedSid {}
unsafe impl Sync for OwnedSid {}

/// The SID grammar tops out well under 200 characters (15 sub-authorities of
/// ~10 digits); 256 turns away junk before the Win32 parser sees it.
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
        // SAFETY: `wide` is NUL-terminated and outlives the call; `psid` is an
        // out-param the callee fills on success.
        let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &raw mut psid) };
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
            // SAFETY: `LocalAlloc`'d by `ConvertStringSidToSidW`, freed once.
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.0);
            }
        }
    }
}

/// FNV-1a. Enough to keep mutex names apart; not a security boundary.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn mutex_name_for(canonical: &Path) -> String {
    let key = canonical.to_string_lossy().to_lowercase();
    let h = fnv1a64(&key);
    format!("Local\\ral.sandbox.dacl.{h:016x}")
}

struct PathMutexGuard {
    handle: HANDLE,
    acquired: bool,
}

/// Two ral sessions stamping the same path should serialize in seconds at
/// worst; the cap exists so a wedged peer surfaces an error instead of
/// hanging us.
const PATH_MUTEX_WAIT_MS: u32 = 30_000;

impl PathMutexGuard {
    fn acquire(canonical: &Path) -> Result<Self, DaclError> {
        let name = mutex_name_for(canonical);
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; no security
        // attributes, not initially owned.
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
            // The previous holder died without releasing, but we own the
            // mutex all the same, and the orphan sweep reconciles whatever
            // DACL state it left — so proceed rather than fail.
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
        // SAFETY: a valid mutex handle owned by this guard; released only if
        // actually acquired, then always closed.
        unsafe {
            if self.acquired {
                ReleaseMutex(self.handle);
            }
            CloseHandle(self.handle);
        }
    }
}

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
        // The trustee-by-SID form reinterprets this field as the PSID; both
        // are raw `*mut _` here, so the cast preserves the bits.
        ptstrName: sid.as_psid().cast::<u16>(),
    }
}

fn win32_err(path: &Path, op: &str, rc: u32) -> DaclError {
    DaclError::Win32 {
        path: path.to_path_buf(),
        reason: format!("{op}: {}", io::Error::from_raw_os_error(rc.cast_signed())),
    }
}

fn win32_err_str(path: &Path, msg: &str) -> DaclError {
    DaclError::Win32 {
        path: path.to_path_buf(),
        reason: msg.to_string(),
    }
}

/// Merge one explicit allow-or-deny ACE into `path`'s DACL, under the path
/// mutex the caller holds across scan, ledger, and this. `SetEntriesInAclW`
/// inserts in canonical order — explicit deny, explicit allow, inherited — so
/// a deny beats an allow inherited from an enclosing grant; and a directory
/// gets `OI|CI`, whose propagation Win32 handles in both directions, which is
/// why nothing here walks descendants.
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
    // SAFETY: every pointer is either a NUL-terminated wide buffer that
    // outlives the call or an out-param owned by this stack frame.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut existing_dacl,
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(path, "GetNamedSecurityInfoW", rc));
    }

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `ea` outlives the call; `existing_dacl` came from the query
    // above, and null there means "no prior DACL", which the callee accepts.
    let rc = unsafe { SetEntriesInAclW(1, &raw const ea, existing_dacl, &raw mut new_dacl) };
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
            windows_sys::Win32::Foundation::LocalFree(new_dacl.cast::<c_void>());
        }
        windows_sys::Win32::Foundation::LocalFree(sd);
    }

    if rc != ERROR_SUCCESS {
        if rc == ERROR_ACCESS_DENIED {
            return Err(DaclError::WriteDacDenied {
                path: path.to_path_buf(),
                reason: format!(
                    "SetNamedSecurityInfoW: {}",
                    io::Error::from_raw_os_error(rc.cast_signed())
                ),
            });
        }
        return Err(win32_err(path, "SetNamedSecurityInfoW", rc));
    }
    Ok(())
}

/// Put one target's DACL back the way the entry found it, under the same
/// per-path mutex the apply took.
fn restore_one(entry: &AppliedAce) -> Result<(), DaclError> {
    let _guard = PathMutexGuard::acquire(&entry.canonical_path)?;
    replace_explicit_aces_for_sid(&entry.canonical_path, &entry.sid_string, &entry.prior_state)
}

/// Every explicit (non-inherited) ACE on `canonical` for `sid_str`. Runs under
/// the path mutex, so what it sees still holds when the apply lands.
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
            &raw mut existing_dacl,
            std::ptr::null_mut(),
            &raw mut sd,
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
    // SAFETY: `info` is a stack out-param sized to the class asked for.
    let ok = unsafe {
        GetAclInformation(
            existing_dacl,
            (&raw mut info).cast::<c_void>(),
            ACL_SIZE_INFORMATION_CB,
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
    for i in 0..info.AceCount {
        let mut ace_ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: `i` is within the count just reported for this ACL.
        if unsafe { GetAce(existing_dacl, i, &raw mut ace_ptr) } == 0 {
            continue;
        }
        // SAFETY: filled by a successful GetAce; every ACE opens with an
        // ACE_HEADER.
        let header = unsafe { &*(ace_ptr as *const ACE_HEADER) };
        if (header.AceFlags & INHERITED_ACE_FLAG) != 0 {
            continue;
        }
        let ace_type = match header.AceType {
            0x00 => AceType::Allow,
            0x01 => AceType::Deny,
            _ => continue, // object/compound/audit ACEs are not our concern
        };
        // ACCESS_ALLOWED_ACE and ACCESS_DENIED_ACE share their layout up to
        // and including the inline SidStart dword.
        let mask_and_sid = ace_ptr as *const ACCESS_ALLOWED_ACE;
        // SAFETY: same ACE, read through that shared prefix.
        let ace_mask = unsafe { (*mask_and_sid).Mask };
        // SAFETY: `SidStart` is the first dword of the inline SID, so its
        // address is where the SID bytes begin.
        let ace_sid = (unsafe { &raw const (*mask_and_sid).SidStart }) as PSID;
        // SAFETY: both point at valid SID buffers for this call.
        if unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0 {
            prior.push(PriorAce {
                ace_type,
                access_mask: ace_mask,
                inherit_flags: header.AceFlags & !INHERITED_ACE_FLAG,
            });
        }
    }
    unsafe {
        windows_sys::Win32::Foundation::LocalFree(sd);
    }
    Ok(prior)
}

/// SIDs every `AppContainer` token implicitly carries, so a grant restating
/// one of them buys nothing — and on a system path this account does not own
/// would fail `WRITE_DAC` for nothing.
///
/// - `S-1-15-2-1` — `ALL APPLICATION PACKAGES`.
/// - `S-1-15-2-2` — `ALL RESTRICTED APPLICATION PACKAGES`.
/// - `S-1-1-0` — `Everyone`, which an `AppContainer` token does keep; it
///   strips `Authenticated Users` and `Users`, so those are absent by design.
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

/// What [`WELL_KNOWN_AC_SIDS`] alone grant on `path`: inherited ACEs count,
/// explicit grants to a specific `AppContainer` SID do not, since the caller
/// is deciding whether such a grant is still needed. The walk follows
/// Windows' own access check — a matching deny marks bits off, and a later
/// allow can only add bits not already denied. A NULL or empty DACL yields 0,
/// "grants nothing", so the caller tries the real grant instead of assuming.
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
            &raw mut existing_dacl,
            std::ptr::null_mut(),
            &raw mut sd,
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
    // SAFETY: `info` is a stack out-param sized to the class asked for.
    let ok = unsafe {
        GetAclInformation(
            existing_dacl,
            (&raw mut info).cast::<c_void>(),
            ACL_SIZE_INFORMATION_CB,
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
        // SAFETY: `i` is within the count just reported for this ACL.
        if unsafe { GetAce(existing_dacl, i, &raw mut ace_ptr) } == 0 {
            continue;
        }
        // SAFETY: filled by a successful GetAce; every ACE opens with an
        // ACE_HEADER.
        let header = unsafe { &*(ace_ptr as *const ACE_HEADER) };
        let ace_type = match header.AceType {
            0x00 => AceType::Allow,
            0x01 => AceType::Deny,
            _ => continue,
        };
        let mask_and_sid = ace_ptr as *const ACCESS_ALLOWED_ACE;
        // SAFETY: the shared-prefix layout again, as in scan_explicit_aces_for_sid.
        let ace_mask = unsafe { (*mask_and_sid).Mask };
        let ace_sid = (unsafe { &raw const (*mask_and_sid).SidStart }) as PSID;
        // SAFETY: both point at valid SID buffers for this call.
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

/// Whether `needed_mask` is already covered without any per-session ACE.
/// A DACL that cannot be read answers `false`: better to attempt the grant
/// and fail than to assume a coverage nothing verified.
///
/// after mxc `fallback_detector.rs::appcontainer_already_grants`
fn appcontainer_already_grants(path: &Path, needed_mask: u32) -> bool {
    match compute_appcontainer_effective_access(path) {
        Ok(effective) => (effective & needed_mask) == needed_mask,
        Err(_) => false,
    }
}

/// Drop the paths already covered by the well-known SIDs. Without this a
/// non-admin session stamping a system root — `System32`, which
/// `ALL APPLICATION PACKAGES` reads system-wide anyway — would fail closed on
/// `WRITE_DAC` for a grant nobody needed. Grants only: `session::confine`
/// never filters deny paths, since a group grant cannot subtract access.
///
/// after mxc `dispatcher.rs::filter_paths_needing_grant`
pub(crate) fn filter_paths_needing_grant(paths: Vec<PathBuf>, needed_mask: u32) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|p| !appcontainer_already_grants(p, needed_mask))
        .collect()
}

/// Canonical DACL order, smallest first: explicit deny, explicit allow,
/// explicit other, then everything inherited regardless of type.
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

/// Rebuild `path`'s DACL with every explicit ACE for `sid_str` dropped and
/// the `replay` ones put back in canonical order; inherited ACEs and other
/// trustees survive verbatim. `SetEntriesInAclW(REVOKE_ACCESS)` would be the
/// obvious shortcut, but on some Windows builds it leaves explicit
/// `ACCESS_DENIED` ACEs behind after what should be a full revoke, so the
/// surgery is done by hand.
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
            &raw mut existing_dacl,
            std::ptr::null_mut(),
            &raw mut sd,
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

    let new_acl_ptr = new_acl_dwords.as_ptr().cast::<ACL>();
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
                    io::Error::from_raw_os_error(rc.cast_signed())
                ),
            });
        }
        return Err(win32_err(path, "SetNamedSecurityInfoW", rc));
    }
    Ok(())
}

/// The pure half of [`replace_explicit_aces_for_sid`]. The new ACL comes back
/// as a `Vec<u32>` because `InitializeAcl` wants 4-byte alignment and that is
/// the cheapest way to promise it.
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
        // SAFETY: non-null, and from a successful GetNamedSecurityInfoW.
        let ok = unsafe {
            GetAclInformation(
                existing_dacl,
                (&raw mut info).cast::<c_void>(),
                ACL_SIZE_INFORMATION_CB,
                AclSizeInformation,
            )
        };
        // Degrading to "no kept entries" would rebuild the DACL as if every
        // other trustee's explicit ACE had never existed, silently stripping
        // a user directory. Propagate, as `scan_explicit_aces_for_sid` does
        // for the same call.
        if ok == 0 {
            return Err(win32_err_str(
                path,
                &format!("GetAclInformation: {}", io::Error::last_os_error()),
            ));
        }
        for i in 0..info.AceCount {
            let mut ace_ptr: *mut c_void = std::ptr::null_mut();
            // SAFETY: `i` is within the count just reported for this ACL.
            if unsafe { GetAce(existing_dacl, i, &raw mut ace_ptr) } == 0 {
                continue;
            }
            // SAFETY: filled by a successful GetAce.
            let header = unsafe { &*(ace_ptr as *const ACE_HEADER) };
            let inherited = (header.AceFlags & INHERITED_ACE_FLAG) != 0;
            let mut drop_it = false;
            if !inherited && (header.AceType == 0x00 || header.AceType == 0x01) {
                let ace_struct = ace_ptr as *const ACCESS_ALLOWED_ACE;
                // SAFETY: the shared-prefix layout again.
                let ace_sid = (unsafe { &raw const (*ace_struct).SidStart }) as PSID;
                // SAFETY: both point at valid SID buffers for this call.
                if unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0 {
                    drop_it = true;
                }
            }
            if !drop_it {
                entries.push(Entry::Kept {
                    ptr: ace_ptr,
                    size: u32::from(header.AceSize),
                    bucket: canonical_bucket(header.AceType, inherited),
                    order: next_order,
                });
                next_order += 1;
                keeps_bytes += u32::from(header.AceSize);
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

    // Stable, so within a bucket the original DACL layout holds and
    // unrelated ACEs never shuffle.
    entries.sort_by_key(|e| match e {
        Entry::Kept { bucket, order, .. } | Entry::Replay { bucket, order, .. } => {
            (*bucket, *order)
        }
    });

    // Per-ACE byte size: ACE_HEADER (4) + ACCESS_MASK (4) + SID bytes.
    // SAFETY: `sid` owns a valid PSID for this call.
    let sid_len: u32 = unsafe { GetLengthSid(sid.as_psid()) };
    let per_replay_size: u32 = 8 + sid_len;
    let replay_bytes: u32 =
        per_replay_size.saturating_mul(u32::try_from(replay.len()).unwrap_or(u32::MAX));

    let mut new_acl_size: u32 = ACL_CB + keeps_bytes + replay_bytes;
    new_acl_size = (new_acl_size + 3) & !3; // DWORD-align, per ACL_SIZE_INFORMATION's units
    let min_acl_size = ACL_CB;
    if new_acl_size < min_acl_size {
        new_acl_size = min_acl_size;
    }

    let dwords = (new_acl_size as usize).div_ceil(4);
    let mut new_acl_buf: Vec<u32> = vec![0u32; dwords];
    let new_acl_ptr = new_acl_buf.as_mut_ptr().cast::<ACL>();

    // SAFETY: freshly allocated, zeroed, 4-byte-aligned, and at least
    // `new_acl_size` bytes long.
    if unsafe { InitializeAcl(new_acl_ptr, new_acl_size, ACL_REVISION) } == 0 {
        return Err(win32_err_str(
            path,
            &format!("InitializeAcl: {}", io::Error::last_os_error()),
        ));
    }

    // Neither `AddAce` nor the `AddAccess{Allowed,Denied}AceEx` family
    // canonicalizes on insert — both simply append — so emission order here
    // is the final ACL order.
    for entry in &entries {
        match entry {
            Entry::Kept { ptr, size, .. } => {
                // SAFETY: `new_acl_ptr` was initialized above; `ptr` is a live
                // ACE of exactly `size` bytes in the still-valid query result.
                if unsafe { AddAce(new_acl_ptr, ACL_REVISION, u32::MAX, *ptr, *size) } == 0 {
                    return Err(win32_err_str(
                        path,
                        &format!("AddAce(keep): {}", io::Error::last_os_error()),
                    ));
                }
            }
            Entry::Replay { prior, .. } => {
                let flags = u32::from(prior.inherit_flags);
                // SAFETY: `new_acl_ptr` was sized above to hold every replay
                // entry; `sid` is a valid PSID.
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

struct ProcessHandleGuard(HANDLE);

impl Drop for ProcessHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a valid handle from `OpenProcess`.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// True when `pid` runs under the expected image basename and, given a
/// non-zero `expected_start_filetime`, was created at exactly that instant.
/// `GetProcessTimes` reports one fixed timestamp for a process's whole life,
/// so equality is the right test and any deviation means the PID has been
/// recycled. `None` or zero falls back to pid-and-image, all an older ledger
/// records.
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

    /// Wide chars of room: `MAX_PATH` is 260, and the extended-length forms
    /// this may meet stay well inside.
    const IMAGE_NAME_CHARS: u32 = 1024;
    let mut buf = [0u16; IMAGE_NAME_CHARS as usize];
    let mut sz: u32 = IMAGE_NAME_CHARS;
    // SAFETY: `handle.0` was checked above; `buf`/`sz` are stack locals
    // sized to the buffer.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_FORMAT::default(),
            buf.as_mut_ptr(),
            &raw mut sz,
        )
    };
    if ok == 0 || sz == 0 {
        return false;
    }
    let full = String::from_utf16_lossy(&buf[..sz as usize]);
    let basename = crate::path::basename(&full);
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
    let gpt = unsafe {
        GetProcessTimes(
            handle.0,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if gpt == 0 {
        return false;
    }
    let live = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    live == recorded
}

/// This process's creation time as a Windows FILETIME: 100-ns intervals
/// since 1601-01-01 UTC.
fn process_creation_filetime() -> Result<u64, DaclError> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: `GetCurrentProcess` yields a pseudo-handle needing no close;
    // the four out-params are stack locals of the right type.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
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
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    /// An uncancelled scope: every test here exercises the stamp itself, while
    /// the cancel path has its own test below.
    fn live() -> CancelScope {
        CancelScope::default()
    }

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
                    inherit_flags: u8::try_from(OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE)
                        .expect("ACE inherit flags are AceFlags bits, and that field is one byte"),
                }],
            }],
            profiles: vec!["ral.sandbox.s42.p0".into()],
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
        assert_eq!(parsed.profiles, vec!["ral.sandbox.s42.p0".to_string()]);
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
        assert!(parsed.profiles.is_empty());
    }

    // The mask-shape invariants are `const _` asserts at the declarations
    // above, not tests here, so drift fails the build rather than depending
    // on someone running an unfiltered suite.

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
        // "Now" in FILETIME units by arithmetic, so the bound check pulls in
        // no extra Win32 feature.
        let now_ft = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(u64::MAX, |d| {
                UNIX_EPOCH_AS_FILETIME + u64::try_from(d.as_nanos()).unwrap_or(u64::MAX) / 100
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
    // These mutate real filesystem ACLs, each under its own XDG_STATE_HOME,
    // serialized by `ENV_LOCK` because the suite runs multi-threaded and two
    // tests' env mutations must not interleave. `crate::test_env` is the same
    // pattern but `cfg(unix)`, so this module keeps a copy of it.

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_scoped_state_dir<R>(f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let scratch = tempfile::tempdir().unwrap();
        let state_home = scratch.path().to_string_lossy().into_owned();
        let prev = std::env::var_os("XDG_STATE_HOME");
        // SAFETY: ENV_LOCK is held, so no other thread reads or writes
        // XDG_STATE_HOME meanwhile.
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
            m.grant_appcontainer_access("S-1-1-0", &[td.path().to_path_buf()], &[], &live())
                .unwrap();
            m.restore().unwrap();
            assert!(m.applied.is_empty());
        });
    }

    /// A wall or an Esc must be able to end the stamp, and end it having
    /// touched nothing.
    #[test]
    fn a_cancel_in_force_stops_the_stamp_before_it_applies() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let cancel = CancelScope::default();
            cancel.cancel(CancelCause::Deadline);

            let mut m = DaclManager::new().unwrap();
            let err =
                m.grant_appcontainer_access("S-1-1-0", &[td.path().to_path_buf()], &[], &cancel);
            assert!(
                matches!(err, Err(DaclError::Cancelled(CancelCause::Deadline))),
                "the poll must report the cause it saw, not a grant failure"
            );
            assert!(
                m.applied.is_empty(),
                "a cancel read before the first path leaves no ACE to restore"
            );
        });
    }

    #[test]
    fn apply_overlapping_then_restore_does_not_leak_rights() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let target = td.path().to_path_buf();
            let everyone = "S-1-1-0";

            // RO, then RW from a separate manager. Same trustee, so the two
            // merge into one ACE and the second manager can only unwind via
            // the prior_state it captured.
            let mut outer = DaclManager::new().unwrap();
            outer
                .grant_appcontainer_access(everyone, &[], std::slice::from_ref(&target), &live())
                .unwrap();

            let mut inner = DaclManager::new().unwrap();
            inner
                .grant_appcontainer_access(everyone, std::slice::from_ref(&target), &[], &live())
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
            m.grant_appcontainer_access("S-1-1-0", &[td.path().to_path_buf()], &[], &live())
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
                m.grant_appcontainer_access("S-1-1-0", &[td.path().to_path_buf()], &[], &live())
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
                &live(),
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
                &live(),
            );
            // Which variant depends on how the host resolves an unreachable
            // server name; all three mean "never silently succeeded". The
            // unit tests above pin the classification deterministically.
            match &err {
                Err(
                    DaclError::NetworkPathRejected(_)
                    | DaclError::PathNotFound(_)
                    | DaclError::Win32 { .. },
                ) => {}
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
                m.grant_appcontainer_access("S-1-1-0", std::slice::from_ref(&target), &[], &live())
                    .unwrap();
                // Forge a dead-PID ledger for recovery to find, then empty
                // this manager so only the recovery path does the undoing.
                let dir = ensure_ledger_dir().unwrap();
                let synthetic = dir.join("pid-2147483646-orphan.json");
                let l = Ledger {
                    run_id: "pid-2147483646-orphan".into(),
                    pid: 0x7FFF_FFFE,
                    image_name: "ral.exe".into(),
                    started_at_filetime: 0,
                    applied: m.applied.clone(),
                    profiles: Vec::new(),
                };
                write_ledger(&synthetic, &l).unwrap();
                m.applied.clear();
            }
            let report = recover_orphaned_state().unwrap();
            assert!(report.files_processed >= 1);
        });
    }

    #[test]
    fn record_and_forget_profile_round_trip_the_ledger() {
        with_scoped_state_dir(|| {
            let mut m = DaclManager::new().unwrap();
            m.record_profile("ral.sandbox.s1.p0").unwrap();
            let persisted = read_ledger(&m.ledger_path).unwrap();
            assert_eq!(persisted.profiles, vec!["ral.sandbox.s1.p0".to_string()]);
            m.forget_profile("ral.sandbox.s1.p0").unwrap();
            // Nothing recorded — the ledger file is removed outright.
            assert!(matches!(m.ledger_path.try_exists(), Ok(false)));
        });
    }

    #[test]
    fn recovery_deletes_dead_sessions_profiles() {
        with_scoped_state_dir(|| {
            // One profile genuinely registered, one ledgered but never
            // created — the crash window between record_profile and the OS
            // create. Recovery deletes the first and consumes the ledger
            // either way, so the absent one is not retried forever.
            let created = format!("ral.test.recovery.{}", std::process::id());
            let ghost = format!("ral.test.recovery.ghost.{}", std::process::id());
            let profile =
                super::super::appcontainer::AppContainerProfile::create_or_reuse(&created)
                    .expect("test profile creation");
            drop(profile); // frees the in-memory SID; the OS profile stays registered

            let dir = ensure_ledger_dir().unwrap();
            let synthetic = dir.join("pid-2147483646-profiles.json");
            let l = Ledger {
                run_id: "pid-2147483646-profiles".into(),
                pid: 0x7FFF_FFFE,
                image_name: "ral.exe".into(),
                started_at_filetime: 0,
                applied: Vec::new(),
                profiles: vec![created, ghost],
            };
            write_ledger(&synthetic, &l).unwrap();

            let report = recover_orphaned_state().unwrap();
            assert!(
                report.profiles_deleted >= 1,
                "the registered profile must be deleted: {report:?}"
            );
            assert!(
                matches!(synthetic.try_exists(), Ok(false)),
                "the orphaned ledger must be consumed: {report:?}"
            );
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
                profiles: Vec::new(),
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

    /// Every allow/deny ACE for `sid_str` as `(AceType byte, inherited?)` in
    /// ACL order — inherited ones included, unlike
    /// [`scan_explicit_aces_for_sid`] — so a test can assert the ordering.
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
                &raw mut existing_dacl,
                std::ptr::null_mut(),
                &raw mut sd,
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "GetNamedSecurityInfoW failed");
        let mut out = Vec::new();
        if !existing_dacl.is_null() {
            let mut info = ACL_SIZE_INFORMATION::default();
            unsafe {
                GetAclInformation(
                    existing_dacl,
                    (&raw mut info).cast::<c_void>(),
                    ACL_SIZE_INFORMATION_CB,
                    AclSizeInformation,
                );
            }
            for i in 0..info.AceCount {
                let mut ace_ptr: *mut c_void = std::ptr::null_mut();
                if unsafe { GetAce(existing_dacl, i, &raw mut ace_ptr) } == 0 {
                    continue;
                }
                let header = unsafe { &*(ace_ptr as *const ACE_HEADER) };
                if header.AceType != 0x00 && header.AceType != 0x01 {
                    continue;
                }
                let ace_struct = ace_ptr as *const ACCESS_ALLOWED_ACE;
                let ace_sid = (unsafe { &raw const (*ace_struct).SidStart }) as PSID;
                if unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0 {
                    out.push((header.AceType, (header.AceFlags & INHERITED_ACE_FLAG) != 0));
                }
            }
        }
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(sd);
        }
        out
    }

    #[test]
    fn deny_nested_in_grant_overrides_inherited_allow_and_reverts() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let parent = td.path().to_path_buf();
            let child = parent.join("cred");
            std::fs::create_dir(&child).unwrap();
            let sid = "S-1-1-0";

            let mut m = DaclManager::new().unwrap();
            m.grant_appcontainer_access(sid, std::slice::from_ref(&parent), &[], &live())
                .unwrap();
            m.add_deny_aces(sid, std::slice::from_ref(&child), &live())
                .unwrap();

            let explicit = scan_explicit_aces_for_sid(&child, sid).unwrap();
            assert_eq!(explicit.len(), 1, "one explicit ACE expected: {explicit:?}");
            assert_eq!(explicit[0].ace_type, AceType::Deny);

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

    #[test]
    fn deny_ace_recorded_in_ledger() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let parent = td.path().to_path_buf();
            let child = parent.join("secret");
            std::fs::create_dir(&child).unwrap();
            let sid = "S-1-1-0";

            let mut m = DaclManager::new().unwrap();
            m.grant_appcontainer_access(sid, std::slice::from_ref(&parent), &[], &live())
                .unwrap();
            m.add_deny_aces(sid, std::slice::from_ref(&child), &live())
                .unwrap();

            assert!(
                m.applied.iter().any(|a| a.ace_type == AceType::Deny),
                "in-memory ledger must record the deny stamp"
            );
            let persisted = read_ledger(&m.ledger_path).unwrap();
            assert!(
                persisted
                    .applied
                    .iter()
                    .any(|a| a.ace_type == AceType::Deny),
                "on-disk ledger must record the deny stamp"
            );

            m.restore().unwrap();
        });
    }

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
                &live(),
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

    #[test]
    fn effective_access_is_zero_on_fresh_dir() {
        with_scoped_state_dir(|| {
            let td = tempfile::tempdir().unwrap();
            let access = compute_appcontainer_effective_access(td.path()).unwrap();
            assert_eq!(access, 0);
        });
    }
}
