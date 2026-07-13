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
//!   grants into it, idempotently, for the session's lifetime. A `granted`
//!   memo on [`SessionSandbox`] tracks every `(path, access-kind)` already
//!   stamped so a repeat command re-declaring the same prefix does not
//!   re-apply it — `DaclManager`'s ledger is append-only-per-*new*-grant,
//!   so skipping a redundant one is what keeps a long session's ledger from
//!   growing (and fsync-rewriting) on every single command.
//! - [`teardown`] reverts every grant and deletes the profile. The state
//!   lives in a process-global whose `Drop` is not guaranteed at process
//!   exit, so this is the explicit cleanup path; a session that exits without
//!   reaching it leaves its ledger for the next boot's sweep. Wired into
//!   `ral`'s and `exarch`'s clean-shutdown seams via
//!   [`crate::sandbox::teardown_session`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Security::{PSID, SID_AND_ATTRIBUTES};

use crate::process::Launch;
use crate::types::{Break, Error, SandboxProjection, Settled};

use super::appcontainer::{AppContainerProfile, CapabilitySids};
use super::dacl::{self, DaclError, DaclManager};

/// The three independent access levels [`confine`] stamps; the key type of
/// [`SessionSandbox::granted`]'s memo, so a repeat request for the same
/// `(path, kind)` is recognised and skipped regardless of which fs list it
/// arrived through.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum GrantKind {
    ReadWrite,
    ReadOnly,
    Deny,
}

/// One shell session's confinement state: the AppContainer profile every
/// confined child spawns under, the DACL guard that stamps (and later
/// reverts) grant ACEs for that profile's SID, the network capability SIDs
/// (built lazily the first time a `net: true` projection is confined), and
/// a memo of every `(path, access-kind)` this session has already stamped
/// so a later command re-declaring the same prefix does not re-apply it.
struct SessionSandbox {
    profile: AppContainerProfile,
    dacl: DaclManager,
    network_caps: Option<CapabilitySids>,
    granted: HashSet<(PathBuf, GrantKind)>,
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
            granted: HashSet::new(),
        })
    }

    /// Drop every path already stamped at `kind` this session — the memo
    /// that keeps a repeat command from re-applying (and re-fsyncing the
    /// ledger for) a grant it already holds.
    fn drop_already_granted(&self, paths: Vec<PathBuf>, kind: GrantKind) -> Vec<PathBuf> {
        filter_out_granted(&self.granted, paths, kind)
    }

    /// Record that `paths` are now stamped at `kind`, for future
    /// [`drop_already_granted`](Self::drop_already_granted) calls.
    fn mark_granted(&mut self, paths: &[PathBuf], kind: GrantKind) {
        self.granted
            .extend(paths.iter().cloned().map(|p| (p, kind)));
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
/// exists, stamp the projection's fs prefixes (plus the program image) into
/// the session DACL guard, and attach the `SECURITY_CAPABILITIES` (profile
/// SID + network capability SIDs) the spawn boundary threads into
/// `CreateProcessW`.
///
/// `program_image`, when `Some`, is the resolved path of the binary the child
/// will execute; it is granted RO (read + execute) so the LowBox token can
/// load the image — parity with the Linux backend binding the program path RO
/// into the bwrap argv, since a user-installed image is otherwise unreadable
/// to the AppContainer. `None` (a bare-name host program the caller could not
/// resolve) leaves the image's readability to the fs read projection / the
/// `ALL APPLICATION PACKAGES` system paths.
///
/// The profile SID and capability SIDs outlive every spawn: the session owns
/// them for the process lifetime, so `launch` may safely borrow their raw
/// values into the attribute list it copies at spawn time.
///
/// An `Unrestricted` fs projection stamps no projection prefixes (only the
/// program image, if any) — the AppContainer is deny-by-default, so such a
/// child otherwise reads only the `ALL APPLICATION PACKAGES` system paths.
///
/// Every `deny_path` gets an explicit deny-ACE (via
/// [`DaclManager::add_deny_aces`]), unconditionally — there is no "already
/// unreachable, skip it" case. An AppContainer token retains the `Everyone`
/// SID, and `ALL APPLICATION PACKAGES` grants are system-wide, so a
/// `deny_path` that falls outside every read/write prefix *this* projection
/// granted is not actually unreachable: it is reachable through whichever
/// ambient system grant the token already carries. MXC stamps every
/// `deny_path` the same way, with no containment filter
/// (`dispatcher.rs`'s `build_t3_dacl`, 0e7c3dd).
pub(crate) fn confine(
    launch: &mut Launch,
    projection: &SandboxProjection,
    program_image: Option<&Path>,
) -> Settled<()> {
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
    let mut readonly: Vec<PathBuf> = spec
        .read_prefixes
        .iter()
        .filter(|p| !spec.write_prefixes.contains(*p))
        .map(PathBuf::from)
        .collect();
    if let Some(image) = program_image {
        let image = image.to_path_buf();
        if !readwrite.contains(&image) && !readonly.contains(&image) {
            readonly.push(image);
        }
    }

    // Three independent filters narrow what actually needs stamping,
    // cheapest first: the session memo (a repeat of an already-applied
    // grant is pure syscall + ledger-fsync waste — see the module doc);
    // existence ([`existing_paths`] — a path that is not there cannot be
    // stamped, and hard-failing here would refuse every sandboxed command
    // under a base whose deny set names a commonly-absent path, e.g.
    // `xdg:config/{gh,op,gcloud}`); and the well-known-SID effective-access
    // filter (a real `GetNamedSecurityInfoW` + DACL walk, so it runs last).
    let readwrite = sandbox.drop_already_granted(readwrite, GrantKind::ReadWrite);
    let readonly = sandbox.drop_already_granted(readonly, GrantKind::ReadOnly);
    let readwrite = existing_paths(readwrite);
    let readonly = existing_paths(readonly);
    let readwrite = dacl::filter_paths_needing_grant(readwrite, dacl::RW_MASK);
    let readonly = dacl::filter_paths_needing_grant(readonly, dacl::RO_MASK);

    sandbox
        .dacl
        .grant_appcontainer_access(&sid_str, &readwrite, &readonly)
        .map_err(|e| dacl_break(&e))?;
    sandbox.mark_granted(&readwrite, GrantKind::ReadWrite);
    sandbox.mark_granted(&readonly, GrantKind::ReadOnly);

    // Every `deny_path` is stamped unconditionally (see this function's doc
    // for why "outside every grant" is not actually safe to skip on
    // Windows) — filtered only by the memo and by existence, the same two
    // cheap filters the grants get. Denies are deliberately *not* run
    // through the well-known-SID filter: that filter answers "does the
    // AppContainer already have this access", which is irrelevant to
    // subtracting access via a deny.
    let deny: Vec<PathBuf> = spec.deny_paths.iter().map(PathBuf::from).collect();
    let deny = sandbox.drop_already_granted(deny, GrantKind::Deny);
    let deny = existing_paths(deny);
    if !deny.is_empty() {
        sandbox
            .dacl
            .add_deny_aces(&sid_str, &deny)
            .map_err(|e| dacl_break(&e))?;
        sandbox.mark_granted(&deny, GrantKind::Deny);
    }

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
            if report.files_processed > 0 {
                crate::dbg_trace!(
                    "sandbox-win",
                    "orphan DACL recovery: {} ledger(s) processed, {} ACE(s) restored, \
                     {} pruned (target gone)",
                    report.files_processed,
                    report.aces_restored,
                    report.aces_pruned_missing,
                );
            }
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
    // Non-fatal per-entry restore failures accumulate here instead of
    // surfacing as `Err` above (see `DaclManager::restore`'s doc); route
    // them to the same diagnostic seam rather than dropping them.
    for w in sandbox.dacl.warnings() {
        crate::diagnostic::shell_warning(&format!("sandbox session teardown: {w}"));
    }
    if let Err(e) = sandbox.profile.delete() {
        crate::diagnostic::shell_warning(&format!(
            "sandbox session teardown: delete AppContainer profile failed: {e}"
        ));
    }
}

/// Drop paths that do not exist on disk. A grant or deny on an absent path
/// cannot be stamped (`WRITE_DAC` needs an object to apply to), and unlike
/// Linux's bwrap overlay — which can mask a path that is not there with an
/// empty tmpfs even for a deny (`sandbox/linux.rs`'s `--tmpfs` overlay is
/// deliberately *not* gated on existence) — Windows has no analogous "deny
/// something that might later exist" primitive. If the child later creates
/// the path under a granted read/write prefix, that is within the grant's
/// own authority, not a gap this filter opens. Skipping rather than
/// hard-failing also matches the Linux backend's own existence guard on its
/// ro/rw binds (`sandbox/linux.rs`'s `ro_binds` / `rw_binds` loops).
fn existing_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|p| crate::path::exists(&p.to_string_lossy()))
        .collect()
}

/// The pure half of the session grant memo: drop every path already
/// recorded in `granted` at `kind`. Split out from
/// [`SessionSandbox::drop_already_granted`] so the memo's filtering logic
/// is unit-testable without standing up a real AppContainer profile / DACL
/// manager.
fn filter_out_granted(
    granted: &HashSet<(PathBuf, GrantKind)>,
    paths: Vec<PathBuf>,
    kind: GrantKind,
) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|p| !granted.contains(&(p.clone(), kind)))
        .collect()
}

fn io_break(context: &str, e: &std::io::Error) -> Break {
    Break::Error(Error::new(format!("{context}: {e}"), 1))
}

fn dacl_break(e: &DaclError) -> Break {
    Break::Error(Error::new(format!("sandbox: fs grant failed: {e}"), 1))
}

#[cfg(test)]
mod tests {
    use super::{GrantKind, existing_paths, filter_out_granted};
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[test]
    fn existing_paths_drops_a_path_that_is_not_there() {
        let td = tempfile::tempdir().unwrap();
        let present = td.path().to_path_buf();
        let absent = td.path().join("does-not-exist-xyzzy");
        assert_eq!(existing_paths(vec![present.clone(), absent]), vec![present]);
    }

    #[test]
    fn memo_drops_a_path_already_granted_at_the_same_kind() {
        let mut granted = HashSet::new();
        granted.insert((PathBuf::from(r"C:\work"), GrantKind::ReadWrite));
        let kept = filter_out_granted(
            &granted,
            vec![PathBuf::from(r"C:\work"), PathBuf::from(r"C:\other")],
            GrantKind::ReadWrite,
        );
        assert_eq!(kept, vec![PathBuf::from(r"C:\other")]);
    }

    #[test]
    fn memo_is_specific_to_access_kind() {
        // A path already granted read-only is not "already covered" for a
        // read-write request at the same path: the two are stamped with
        // different access masks, so a read-write command must still stamp
        // its own ACE even where a narrower one already exists.
        let mut granted = HashSet::new();
        granted.insert((PathBuf::from(r"C:\work"), GrantKind::ReadOnly));
        let kept = filter_out_granted(
            &granted,
            vec![PathBuf::from(r"C:\work")],
            GrantKind::ReadWrite,
        );
        assert_eq!(kept, vec![PathBuf::from(r"C:\work")]);
    }

    #[test]
    fn memo_lets_an_unrelated_path_through() {
        let mut granted = HashSet::new();
        granted.insert((PathBuf::from(r"C:\work"), GrantKind::Deny));
        let kept = filter_out_granted(
            &granted,
            vec![PathBuf::from(r"C:\Users\me\.ssh")],
            GrantKind::Deny,
        );
        assert_eq!(kept, vec![PathBuf::from(r"C:\Users\me\.ssh")]);
    }
}
