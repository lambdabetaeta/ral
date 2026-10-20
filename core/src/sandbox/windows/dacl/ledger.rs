// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

use super::{DaclError, stamp::STAMP_STORE_FILE_NAME};
use crate::path::shown;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

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
    /// `AppContainer` profile names, recorded before the OS-level create so
    /// the orphan sweep deletes a crashed session's profiles.
    profiles: Vec<String>,
}

/// Aggregated outcome of [`recover_orphaned_state`].
#[derive(Debug, Default)]
pub(crate) struct RecoveryReport {
    pub(crate) files_processed: usize,
    pub(crate) profiles_deleted: usize,
    pub(crate) errors: Vec<String>,
}

/// Crash-safe ledger of what a session registers with the OS and must give
/// back: its `AppContainer` profiles. `session` keeps one per shell session.
/// Filesystem stamps are persistent and need no recovery.
#[derive(Debug)]
pub(crate) struct DaclManager {
    run_id: String,
    ledger_path: PathBuf,
    profiles: Vec<String>,
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
            profiles: Vec::new(),
            process_start_filetime,
        })
    }

    /// Durably record a profile name *before* the caller registers it with
    /// the OS. A crash in
    /// between leaves recovery trying to delete a profile that never existed,
    /// which is the harmless direction to fail in.
    pub(crate) fn record_profile(&mut self, name: &str) -> Result<(), DaclError> {
        self.profiles.push(name.to_string());
        if let Err(e) = self.persist_ledger() {
            self.profiles.pop();
            return Err(e);
        }
        Ok(())
    }

    /// Call once the OS-level profile is gone. The ledger file disappears
    /// outright when no profile is left recorded.
    pub(crate) fn forget_profile(&mut self, name: &str) -> Result<(), DaclError> {
        self.profiles.retain(|p| p != name);
        self.checkpoint()
    }

    fn checkpoint(&self) -> Result<(), DaclError> {
        if self.profiles.is_empty() {
            remove_ledger_best_effort(&self.ledger_path);
            Ok(())
        } else {
            self.persist_ledger()
        }
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:dacl-state-dir-recheck] Ensures the ledger's own directory exists before write, since the cached path may predate a later XDG_STATE_HOME change. Sandbox crash-safety infrastructure, not model data I/O."
    )]
    fn persist_ledger(&self) -> Result<(), DaclError> {
        let ledger = Ledger {
            run_id: self.run_id.clone(),
            pid: std::process::id(),
            image_name: current_image_basename(),
            started_at_filetime: self.process_start_filetime,
            profiles: self.profiles.clone(),
        };
        if let Some(dir) = self.ledger_path.parent() {
            fs::create_dir_all(dir)?;
        }
        write_ledger(&self.ledger_path, &ledger)
    }
}

/// Reap every ledger whose owning process is gone, deleting its profiles. `session::boot_recover`
/// runs this once at session start, before any grant is applied.
pub(crate) fn recover_orphaned_state() -> Result<RecoveryReport, DaclError> {
    let mut report = RecoveryReport::default();
    let dir = ledger_dir();
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:dacl-ledger-sweep] Startup orphan sweep: lists the ledger directory to find ledgers whose owner is gone. Sandbox crash-recovery infrastructure, not model data I/O."
    )]
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(DaclError::LedgerIo(e)),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        // The stamp store lives beside the ledgers but is not one: it has no
        // `run_id`, so parsing it as a `Ledger` always fails. Recognizing it
        // by file name keeps the sweep from quarantining a session's
        // completion witnesses on every boot.
        if path.file_name() == Some(OsStr::new(STAMP_STORE_FILE_NAME)) {
            continue;
        }
        report.files_processed += 1;
        let ledger = match read_ledger(&path) {
            Ok(s) => s,
            Err(e) => {
                report.errors.push(format!("parse {}: {e}", shown(&path)));
                let corrupt = crate::path::with_appended_suffix(&path, "corrupt");
                if let Err(e2) = quarantine_ledger(&path, &corrupt) {
                    report.errors.push(format!(
                        "quarantine {} -> {}: {e2}",
                        shown(&path),
                        shown(&corrupt),
                    ));
                }
                continue;
            }
        };
        if process_alive_with_image(ledger.pid, &ledger.image_name, ledger.started_at_filetime) {
            continue;
        }
        // Never retained for retry. The ledger precedes the OS-level create,
        // so a recorded profile may never have existed; and an undeletable one
        // is inert, since only a spawn from this codebase can acquire its SID.
        for name in &ledger.profiles {
            match super::super::appcontainer::delete_profile_by_name(name) {
                Ok(()) => report.profiles_deleted += 1,
                Err(e) => report.errors.push(format!("delete profile {name}: {e}")),
            }
        }
        if let Err(e) = remove_ledger(&path) {
            report.errors.push(format!("remove {}: {e}", shown(&path)));
        }
    }
    Ok(report)
}

/// Under the XDG `state` base rather than a hardcoded `%LOCALAPPDATA%`, so
/// `XDG_STATE_HOME` relocates it — which is how the tests get a private one.
/// With no home and no override the base is the temp dir: the ledger must land
/// somewhere absolute, and never under the cwd.
pub(super) fn ledger_dir() -> PathBuf {
    let dir = crate::path::app_dir_or_temp(crate::path::XdgKind::State, "ral").join("sandbox-dacl");
    debug_assert!(
        crate::path::is_absolute(&dir),
        "ledger dir must be absolute: {}",
        shown(&dir)
    );
    dir
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-state-dir] Ensures the per-user DACL ledger directory exists before the first ledger write. Sandbox crash-safety infrastructure, not model data I/O."
)]
pub(super) fn ensure_ledger_dir() -> Result<PathBuf, DaclError> {
    let dir = ledger_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn write_ledger(path: &Path, ledger: &Ledger) -> Result<(), DaclError> {
    let json = serde_json::to_vec_pretty(ledger)
        .map_err(|e| DaclError::LedgerParse(format!("serialize: {e}")))?;
    write_bytes_atomic(path, &json)
}

/// Stage to `<path>.tmp`, `fsync`, then rename over the destination, so a
/// reader — the next boot's sweep, another session's stamp check — sees
/// either the previous complete file or this one, never a half-written one.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-ledger-write] Atomic write for the DACL crash-safety state (ledgers, stamp store): stage to <path>.tmp, fsync, rename over the destination. Sandbox infrastructure, not model data I/O."
)]
pub(super) fn write_bytes_atomic(path: &Path, json: &[u8]) -> Result<(), DaclError> {
    let tmp = tmp_path_for(path);
    // A tmp left by a crashed write would fail `create_new` below with
    // ERROR_FILE_EXISTS; clearing it is best-effort.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        #[cfg_attr(not(debug_assertions), allow(unused_variables))]
        Err(e) => crate::dbg_trace!(
            "sandbox-win-dacl",
            "pre-write cleanup of {} failed ({e}); a leftover tmp may obstruct the write",
            shown(&tmp)
        ),
    }
    {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(json)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(DaclError::LedgerIo(e));
    }
    Ok(())
}

fn tmp_path_for(path: &Path) -> PathBuf {
    crate::path::with_appended_suffix(path, "tmp")
}

/// Set an unparseable ledger aside as `corrupt`, so the next sweep does not
/// meet it again and its contents stay for a human to read.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-ledger-quarantine] Renames an unparseable ledger aside during the startup orphan sweep. Sandbox crash-recovery infrastructure, not model data I/O."
)]
fn quarantine_ledger(path: &Path, corrupt: &Path) -> io::Result<()> {
    fs::rename(path, corrupt)
}

fn remove_ledger_best_effort(path: &Path) {
    let _ = remove_ledger(path);
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-ledger-remove] Removes a ledger once every profile it recorded has been deleted. Sandbox infrastructure, not model data I/O."
)]
fn remove_ledger(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

/// Retries `ERROR_SHARING_VIOLATION`/`ERROR_LOCK_VIOLATION`: a concurrent
/// writer mid-rename, or an on-access virus scanner, holds the file for a
/// moment, and without the retry a good ledger gets quarantined as corrupt.
fn read_ledger(path: &Path) -> Result<Ledger, DaclError> {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    const ATTEMPTS: u32 = 3;
    let mut last_err: Option<io::Error> = None;
    for i in 0..ATTEMPTS {
        #[allow(
            clippy::disallowed_methods,
            reason = "[silent:dacl-ledger-read] Reads back a persisted ledger for orphan recovery / crash-safety bookkeeping. Sandbox infrastructure, not model data I/O."
        )]
        let read = fs::read(path);
        match read {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes)
                    .map_err(|e| DaclError::LedgerParse(format!("{}: {e}", shown(path))));
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

#[allow(
    clippy::disallowed_methods,
    reason = "native: the image name the OS reports, compared with the one it reports for a live pid"
)]
fn current_image_basename() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().and_then(|s| s.to_str()).map(str::to_string))
        .unwrap_or_else(|| "ral.exe".to_string())
}

/// Both image and creation time must match: a PID can be recycled.
fn process_alive_with_image(pid: u32, expected_image: &str, expected_start_filetime: u64) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: no preconditions beyond a valid pid, which the OS validates.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    // SAFETY: OpenProcess returned a fresh, non-null handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };

    /// Wide chars of room: `MAX_PATH` is 260, and the extended-length forms
    /// this may meet stay well inside.
    const IMAGE_NAME_CHARS: u32 = 1024;
    let mut buf = [0u16; IMAGE_NAME_CHARS as usize];
    let mut sz: u32 = IMAGE_NAME_CHARS;
    // SAFETY: `handle.0` was checked above; `buf`/`sz` are stack locals
    // sized to the buffer.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle.as_raw_handle(),
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
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all four out-params are stack locals of the right type.
    let gpt = unsafe {
        GetProcessTimes(
            handle.as_raw_handle(),
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
    live == expected_start_filetime
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
mod tests;
