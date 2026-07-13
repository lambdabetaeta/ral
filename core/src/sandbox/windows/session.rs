//! Session-scoped AppContainer confinement state.
//!
//! One shell session (the ral-core process) gets one AppContainer profile,
//! created lazily the first time a Windows projection becomes enforceable,
//! plus a session-scoped [`DaclManager`] that stamps grant ACEs for that
//! profile's SID and reverts them at teardown. [`confine`] wires both into a
//! [`Launch`]: it ensures the profile exists, stamps the projection's fs
//! prefixes, and attaches the `SECURITY_CAPABILITIES` the spawn boundary
//! threads into `CreateProcessW`.
//!
//! Lifecycle:
//! - [`boot_recover`] runs once per session at [`crate::sandbox::early_init`]
//!   and reclaims DACL grants a crashed prior session left stamped (the
//!   durable record is the ledger, not this process's memory).
//! - [`confine`] creates the session state on first use and accumulates
//!   grants into it, idempotently, for the session's lifetime.
//! - [`teardown`] reverts every grant and deletes the profile. The state
//!   lives in a process-global whose `Drop` is not guaranteed at process
//!   exit, so this is the explicit cleanup path; a session that exits without
//!   reaching it leaves its ledger for the next boot's sweep.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Security::{PSID, SID_AND_ATTRIBUTES};

use crate::process::Launch;
use crate::types::{Break, Error, SandboxProjection, Settled};

use super::appcontainer::{AppContainerProfile, CapabilitySids};
use super::dacl::{self, DaclError, DaclManager};

/// One shell session's confinement state: the AppContainer profile every
/// confined child spawns under, the DACL guard that stamps (and later
/// reverts) grant ACEs for that profile's SID, and — built lazily the first
/// time a `net: true` projection is confined — the network capability SIDs.
struct SessionSandbox {
    profile: AppContainerProfile,
    dacl: DaclManager,
    network_caps: Option<CapabilitySids>,
}

// SAFETY: the raw SIDs `SessionSandbox` holds (the profile SID, the network
// capability SIDs) point at process-global OS-owned memory
// (`FreeSid`/`LocalFree`-managed), not thread-local state. Every access to the
// struct is serialized by [`cell`]'s `Mutex`; the only lock-free reads are the
// raw SID pointer *values* copied into a `Launch` by
// `Launch::security_capabilities`, which reference immutable OS memory that
// lives until [`teardown`] frees it — and teardown runs only at session
// shutdown, never concurrently with a spawn.
unsafe impl Send for SessionSandbox {}

fn cell() -> &'static Mutex<Option<SessionSandbox>> {
    static SESSION: OnceLock<Mutex<Option<SessionSandbox>>> = OnceLock::new();
    SESSION.get_or_init(|| Mutex::new(None))
}

impl SessionSandbox {
    fn create() -> Settled<Self> {
        let profile = AppContainerProfile::create_or_reuse(&profile_name())
            .map_err(|e| io_break("sandbox: create AppContainer profile", &e))?;
        let dacl = DaclManager::new().map_err(|e| dacl_break(&e))?;
        Ok(Self {
            profile,
            dacl,
            network_caps: None,
        })
    }

    /// The network capability SID array, derived once on first request. For a
    /// `net: false` projection this is never called — the empty array *is* the
    /// enforcement (an AppContainer with no network capability cannot open a
    /// socket).
    fn ensure_network_caps(&mut self) -> Settled<&[SID_AND_ATTRIBUTES]> {
        if self.network_caps.is_none() {
            let caps = CapabilitySids::build(true)
                .map_err(|e| io_break("sandbox: derive network capability SIDs", &e))?;
            self.network_caps = Some(caps);
        }
        Ok(self
            .network_caps
            .as_ref()
            .expect("network caps built above")
            .entries())
    }
}

/// One profile per shell session; the ral-core process *is* the session, so
/// its pid keys the profile. `create_or_reuse` transparently adopts a
/// same-named profile a crashed prior session with the recycled pid left
/// registered, and [`boot_recover`] reclaims that session's stamped ACEs.
fn profile_name() -> String {
    format!("ral.sandbox.s{}", std::process::id())
}

/// Confine `launch` under this session's AppContainer: ensure the profile
/// exists, stamp the projection's fs prefixes into the session DACL guard,
/// and attach the `SECURITY_CAPABILITIES` (profile SID + network capability
/// SIDs) the spawn boundary threads into `CreateProcessW`.
///
/// The profile SID and capability SIDs outlive every spawn: the session owns
/// them for the process lifetime, so `launch` may safely borrow their raw
/// values into the attribute list it copies at spawn time.
///
/// An `Unrestricted` fs projection stamps no prefixes — the AppContainer is
/// deny-by-default, so such a child reads only the `ALL APPLICATION PACKAGES`
/// system paths. The projection's `deny_paths` are not stamped: AppContainer
/// grants are allow-only, and a deny nested inside a granted prefix is a rule
/// this backend cannot express.
pub(crate) fn confine(launch: &mut Launch, projection: &SandboxProjection) -> Settled<()> {
    let mut guard = cell().lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(SessionSandbox::create()?);
    }
    let sandbox = guard.as_mut().expect("session created above");

    let sid_str = sandbox
        .profile
        .sid_string()
        .map_err(|e| io_break("sandbox: AppContainer SID string", &e))?;

    let spec = projection.bind_spec();
    let readwrite: Vec<PathBuf> = spec.write_prefixes.iter().map(PathBuf::from).collect();
    let readonly: Vec<PathBuf> = spec
        .read_prefixes
        .iter()
        .filter(|p| !spec.write_prefixes.contains(*p))
        .map(PathBuf::from)
        .collect();
    sandbox
        .dacl
        .grant_appcontainer_access(&sid_str, &readwrite, &readonly)
        .map_err(|e| dacl_break(&e))?;

    let profile_sid: PSID = sandbox.profile.sid();
    let capabilities: &[SID_AND_ATTRIBUTES] = if projection.net {
        sandbox.ensure_network_caps()?
    } else {
        &[]
    };
    launch.security_capabilities(profile_sid, capabilities);
    Ok(())
}

/// Reclaim DACL grants a crashed prior session left stamped. Best-effort and
/// idempotent — logged, never fatal: a failed sweep must not stop a session
/// from starting, and any ledger it cannot clear is retried on the next boot.
pub(crate) fn boot_recover() {
    match dacl::recover_orphaned_state() {
        Ok(report) => {
            for e in &report.errors {
                crate::dbg_trace!("sandbox-win", "orphan DACL recovery: {e}");
            }
        }
        Err(e) => crate::dbg_trace!("sandbox-win", "orphan DACL recovery failed: {e}"),
    }
}

/// Tear down the session sandbox: revert every stamped grant ACE, then delete
/// the AppContainer profile. Idempotent; a no-op when no session state was
/// ever created.
pub(crate) fn teardown() {
    let Some(mut sandbox) = cell().lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    if let Err(e) = sandbox.dacl.restore() {
        crate::diagnostic::shell_warning(&format!(
            "sandbox session teardown: DACL restore failed: {e}"
        ));
    }
    if let Err(e) = sandbox.profile.delete() {
        crate::diagnostic::shell_warning(&format!(
            "sandbox session teardown: delete AppContainer profile failed: {e}"
        ));
    }
}

fn io_break(context: &str, e: &std::io::Error) -> Break {
    Break::Error(Error::new(format!("{context}: {e}"), 1))
}

fn dacl_break(e: &DaclError) -> Break {
    Break::Error(Error::new(format!("sandbox: fs grant failed: {e}"), 1))
}
