//! Session-scoped `AppContainer` confinement: one profile per distinct fs
//! projection the session confines, minted lazily and held until teardown.
//!
//! Each profile's SID is stamped with exactly its own projection's paths, so a
//! narrowed `grant` — or a subagent forked with narrowed permissions — hashes
//! to a different key and spawns under a SID never granted the wider paths:
//! attenuation is enforced at the kernel, and stamping is paid once per
//! distinct projection rather than per command. Identity is the projection's
//! own fs prefix lists, which the grant fold leaves sorted and deduplicated,
//! so equal keys mean equal projections, not equal spelling; names are unique by
//! construction rather than content-hashed, since a collision would silently
//! merge two projections' authority under one SID.
//!
//! Grant frames deliberately revert nothing: a detached worker may outlive its
//! frame under the SID it was born with, and a SID no live child holds is
//! inert. ACEs come off at [`teardown`], or at the next [`boot_recover`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Security::{PSID, SID_AND_ATTRIBUTES};

use crate::process::Launch;
use crate::process::cancel::CancelScope;
use crate::types::{Break, Error, SandboxProjection, Settled};

use super::appcontainer::{AppContainerProfile, CapabilitySids};
use super::dacl::{self, DaclError, DaclManager};

/// The three access levels [`confine`] stamps, and half the memo key: a repeat
/// `(path, kind)` is skipped whichever fs list it arrived through.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum GrantKind {
    ReadWrite,
    ReadOnly,
    Deny,
}

/// One fs projection's identity, as [`FsRules`](crate::types::FsRules)
/// carries it. `net` is absent
/// on purpose: network authority rides per-spawn on
/// capability SIDs, never stamped on disk, so it needs no SID of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProjectionKey {
    read: Vec<String>,
    write: Vec<String>,
    deny: Vec<String>,
}

/// One projection's profile, plus the `(path, kind)` pairs already stamped
/// for its SID.
struct ProjectionSandbox {
    profile: AppContainerProfile,
    granted: HashSet<(PathBuf, GrantKind)>,
}

impl ProjectionSandbox {
    fn mark_granted(&mut self, paths: &[PathBuf], kind: GrantKind) {
        self.granted
            .extend(paths.iter().cloned().map(|p| (p, kind)));
    }
}

/// One shell session's confinement state: one DACL guard for the whole
/// session, one sandbox per fs projection.
struct SessionSandbox {
    dacl: DaclManager,
    network_caps: Option<CapabilitySids>,
    projections: HashMap<ProjectionKey, ProjectionSandbox>,
    next_profile_index: u32,
}

// SAFETY: the raw SIDs held here (each profile's SID, the network capability
// SIDs) point at process-global OS-owned memory, not thread-local state, and
// live until `teardown` frees them. Every access to the struct is serialized
// by `cell`'s mutex; the only lock-free reads are the raw pointer *values*
// `Launch::security_capabilities` copies, and teardown runs at session
// shutdown, never concurrently with a spawn.
#[allow(
    clippy::non_send_fields_in_send_ty,
    reason = "the fields the lint names (`network_caps`, `projections`) are exactly the raw SIDs the SAFETY note above argues about: OS-owned process-global memory, not thread-affine state, reached only under [`cell`]'s mutex. The lint can see that a `PSID` is not `Send`; it cannot read the argument for why these ones are."
)]
unsafe impl Send for SessionSandbox {}

fn cell() -> &'static Mutex<Option<SessionSandbox>> {
    static SESSION: OnceLock<Mutex<Option<SessionSandbox>>> = OnceLock::new();
    SESSION.get_or_init(|| Mutex::new(None))
}

impl SessionSandbox {
    fn create() -> Settled<Self> {
        let dacl = DaclManager::new().map_err(|e| dacl_break(&e))?;
        Ok(Self {
            dacl,
            network_caps: None,
            projections: HashMap::new(),
            next_profile_index: 0,
        })
    }

    /// Mint `key`'s profile if the projection is new. The index is burned
    /// before the create so a failed retry never re-records a name, and the
    /// ledger entry precedes the create so a crash between them leaves
    /// recovery deleting a profile that may not exist, never a registered
    /// profile the ledger does not know.
    fn ensure_projection(&mut self, key: &ProjectionKey) -> Settled<()> {
        if self.projections.contains_key(key) {
            return Ok(());
        }
        let index = self.next_profile_index;
        self.next_profile_index += 1;
        let name = profile_name(index);
        self.dacl
            .record_profile(&name)
            .map_err(|e| dacl_break(&e))?;
        let profile = AppContainerProfile::create_or_reuse(&name)
            .map_err(|e| io_break("sandbox: create AppContainer profile", &e))?;
        self.projections.insert(
            key.clone(),
            ProjectionSandbox {
                profile,
                granted: HashSet::new(),
            },
        );
        Ok(())
    }
}

/// The network capability SIDs, derived once per session. A `net: false`
/// projection never calls this — the empty array *is* the enforcement, since
/// an `AppContainer` with no network capability cannot open a socket.
fn ensure_network_caps(slot: &mut Option<CapabilitySids>) -> Settled<&[SID_AND_ATTRIBUTES]> {
    if slot.is_none() {
        let caps = CapabilitySids::build(true)
            .map_err(|e| io_break("sandbox: derive network capability SIDs", &e))?;
        *slot = Some(caps);
    }
    Ok(slot.as_ref().expect("network caps built above").entries())
}

/// Unique by construction: the pid keys the session, a monotonic counter the
/// projection. A recycled pid can still meet a crashed session's leftovers,
/// which `create_or_reuse` adopts — after [`boot_recover`] has swept the ACEs.
fn profile_name(index: u32) -> String {
    format!("ral.sandbox.s{}.p{index}", std::process::id())
}

/// Confine `launch` under its projection's `AppContainer`: resolve the
/// projection to this session's profile for it, stamp the projection's fs
/// prefixes for that profile's SID, and attach the `SECURITY_CAPABILITIES`
/// the spawn boundary threads into `CreateProcessW`.
///
/// `program_image` is granted read+execute so the `LowBox` token can load the
/// child's binary, mirroring the Linux backend's RO bind of the program path;
/// `None` (a bare name the caller could not resolve) leaves that to the read
/// projection and the `ALL APPLICATION PACKAGES` system paths. An
/// `Unrestricted` projection stamps no prefixes at all and shares the one
/// empty-key profile — the `AppContainer` is deny-by-default regardless.
///
/// Every `deny_path` is stamped unconditionally: an `AppContainer` token
/// retains the `Everyone` SID and `ALL APPLICATION PACKAGES` grants are
/// system-wide, so a path outside this projection's own grants is still
/// reachable through whatever ambient grant the token already carries.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the session lock is deliberately held for the whole of `confine` — see the comment at the guard"
)]
pub(crate) fn confine(
    launch: &mut Launch,
    projection: &SandboxProjection,
    program_image: Option<&Path>,
    cancel: &CancelScope,
) -> Settled<()> {
    // Traced in three parts, because each is a different kind of cost and only
    // the middle one is per-path kernel work: minting the profile, the
    // effective-access filter, and the stamp itself.
    let t_confine = std::time::Instant::now();
    // One critical section on purpose: minting a profile, stamping its ACEs
    // and reading back its SIDs must not interleave with another thread
    // confining the same session — and `sandbox`/`proj` borrow out of the
    // guard until `security_capabilities`, so an early `drop` would not
    // compile.
    let mut guard = cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.is_none() {
        *guard = Some(SessionSandbox::create()?);
    }
    let sandbox = guard.as_mut().expect("session created above");

    // Empty when fs is `Unrestricted`: an AppContainer is deny-by-default, so
    // an unattenuated projection stamps nothing and rests on the ambient
    // ALL APPLICATION PACKAGES grants.
    let rules = projection.fs.rules().cloned().unwrap_or_default();
    let key = ProjectionKey {
        read: rules.read_prefixes.clone(),
        write: rules.write_prefixes.clone(),
        deny: rules.deny_paths.clone(),
    };
    sandbox.ensure_projection(&key)?;
    crate::dbg_trace!(
        "sandbox-win",
        "confine: profile ensured in {:?} (read={} write={} deny={})",
        t_confine.elapsed(),
        key.read.len(),
        key.write.len(),
        key.deny.len(),
    );
    let SessionSandbox {
        dacl,
        network_caps,
        projections,
        ..
    } = sandbox;
    let proj = projections.get_mut(&key).expect("projection ensured above");

    let sid_str = proj
        .profile
        .sid_string()
        .map_err(|e| io_break("sandbox: AppContainer SID string", &e))?;

    let readwrite: Vec<PathBuf> = rules.write_prefixes.iter().map(PathBuf::from).collect();
    let mut readonly: Vec<PathBuf> = rules
        .read_prefixes
        .iter()
        .filter(|p| !rules.write_prefixes.contains(*p))
        .map(PathBuf::from)
        .collect();
    if let Some(image) = program_image {
        let image = image.to_path_buf();
        if !readwrite.contains(&image) && !readonly.contains(&image) {
            readonly.push(image);
        }
    }

    // Cheapest filter first: the memo, then existence, then the well-known-SID
    // effective-access check, which is a real `GetNamedSecurityInfoW` and DACL
    // walk per path.
    let t_filter = std::time::Instant::now();
    let (offered_rw, offered_ro) = (readwrite.len(), readonly.len());
    let readwrite = filter_out_granted(&proj.granted, readwrite, GrantKind::ReadWrite);
    let readonly = filter_out_granted(&proj.granted, readonly, GrantKind::ReadOnly);
    let readwrite = existing_paths(readwrite);
    let readonly = existing_paths(readonly);
    let readwrite = dacl::filter_paths_needing_grant(readwrite, dacl::RW_MASK);
    let readonly = dacl::filter_paths_needing_grant(readonly, dacl::RO_MASK);
    crate::dbg_trace!(
        "sandbox-win",
        "confine: access filter kept rw {}/{}, ro {}/{} in {:?}",
        readwrite.len(),
        offered_rw,
        readonly.len(),
        offered_ro,
        t_filter.elapsed(),
    );

    let t_stamp = std::time::Instant::now();
    dacl.grant_appcontainer_access(&sid_str, &readwrite, &readonly, cancel)
        .map_err(|e| dacl_break(&e))?;
    crate::dbg_trace!(
        "sandbox-win",
        "confine: stamped {} rw + {} ro ACEs in {:?}",
        readwrite.len(),
        readonly.len(),
        t_stamp.elapsed(),
    );
    proj.mark_granted(&readwrite, GrantKind::ReadWrite);
    proj.mark_granted(&readonly, GrantKind::ReadOnly);

    // Denies skip the effective-access filter: it answers whether the
    // `AppContainer` already *has* this access, which says nothing about
    // whether subtracting it needs a stamp.
    let deny: Vec<PathBuf> = rules.deny_paths.iter().map(PathBuf::from).collect();
    let deny = filter_out_granted(&proj.granted, deny, GrantKind::Deny);
    let deny = existing_paths(deny);
    if !deny.is_empty() {
        dacl.add_deny_aces(&sid_str, &deny, cancel)
            .map_err(|e| dacl_break(&e))?;
        proj.mark_granted(&deny, GrantKind::Deny);
    }

    let profile_sid: PSID = proj.profile.sid();
    let capabilities: &[SID_AND_ATTRIBUTES] = if projection.net {
        ensure_network_caps(network_caps)?
    } else {
        &[]
    };
    launch.security_capabilities(profile_sid, capabilities);
    crate::dbg_trace!("sandbox-win", "confine: total {:?}", t_confine.elapsed());
    Ok(())
}

/// Reclaim what a crashed prior session left behind — stamped grant ACEs and
/// registered `AppContainer` profiles. Idempotent, and logged rather than
/// fatal: a failed sweep must not stop a session starting, and whatever it
/// cannot clear the next boot retries.
pub(crate) fn boot_recover() {
    match dacl::recover_orphaned_state() {
        Ok(report) => {
            if report.files_processed > 0 {
                crate::dbg_trace!(
                    "sandbox-win",
                    "orphan DACL recovery: {} ledger(s) processed, {} ACE(s) restored, \
                     {} pruned (target gone), {} profile(s) deleted",
                    report.files_processed,
                    report.aces_restored,
                    report.aces_pruned_missing,
                    report.profiles_deleted,
                );
            }
            for e in &report.errors {
                crate::dbg_trace!("sandbox-win", "orphan DACL recovery: {e}");
            }
        }
        Err(e) => crate::dbg_trace!("sandbox-win", "orphan DACL recovery failed: {e}"),
    }
}

/// Revert every stamped ACE, then delete every projection's profile.
/// Idempotent, and the explicit path because the process-global's `Drop` is
/// not guaranteed at exit; a session that never reaches here leaves its ledger
/// for the next boot's sweep.
pub(crate) fn teardown() {
    let Some(sandbox) = cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        return;
    };
    // Traced separately from `confine`: the restore walks every ACE the session
    // stamped, so an exit that looks like a hang is usually this.
    let t_teardown = std::time::Instant::now();
    let SessionSandbox {
        mut dacl,
        network_caps: _,
        mut projections,
        next_profile_index: _,
    } = sandbox;
    let t_restore = std::time::Instant::now();
    if let Err(e) = dacl.restore() {
        crate::diagnostic::shell_warning(&format!(
            "sandbox session teardown: DACL restore failed: {e}"
        ));
    }
    crate::dbg_trace!(
        "sandbox-win",
        "teardown: DACL restore in {:?}",
        t_restore.elapsed()
    );
    // Per-entry restore failures accumulate here rather than surfacing as
    // `Err` above.
    for w in dacl.warnings() {
        crate::diagnostic::shell_warning(&format!("sandbox session teardown: {w}"));
    }
    for (_, proj) in projections.drain() {
        let name = proj.profile.name().to_string();
        match proj.profile.delete() {
            Ok(()) => {
                if let Err(e) = dacl.forget_profile(&name) {
                    crate::diagnostic::shell_warning(&format!(
                        "sandbox session teardown: ledger update for profile {name} failed: {e}"
                    ));
                }
            }
            Err(e) => {
                // The name stays ledgered; the next boot's sweep retries.
                crate::diagnostic::shell_warning(&format!(
                    "sandbox session teardown: delete AppContainer profile {name} failed: {e}"
                ));
            }
        }
    }
    crate::dbg_trace!("sandbox-win", "teardown: total {:?}", t_teardown.elapsed());
}

/// Drop paths that are not on disk: `WRITE_DAC` needs an object to apply to.
/// Where the Linux backend can mask an absent deny path with an empty tmpfs,
/// Windows has no "deny something that might later exist" primitive, so an
/// absent deny is simply skipped — a path the child creates later under a
/// granted prefix falls within that grant's own authority anyway.
fn existing_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|p| crate::path::exists(&p.to_string_lossy()))
        .collect()
}

/// The pure half of the grant memo, free-standing so the tests below can
/// exercise it without a real `AppContainer` profile or DACL manager.
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

/// A cancel mid-stamp is not a sandbox failure, so it surfaces as the cause's
/// own word and status — the same pair every other poll point raises
/// ([`crate::process::check`]) — and never as `sandbox: fs grant failed`.
fn dacl_break(e: &DaclError) -> Break {
    if let DaclError::Cancelled(cause) = e {
        return Break::Error(Error::new(cause.message(), cause.exit_code()));
    }
    Break::Error(Error::new(format!("sandbox: fs grant failed: {e}"), 1))
}

#[cfg(test)]
mod tests {
    use super::{GrantKind, ProjectionKey, existing_paths, filter_out_granted, profile_name};
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
        // Read-only coverage does not satisfy a read-write request: the two
        // stamp different access masks.
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

    #[test]
    fn projection_key_identity_is_the_fs_triple() {
        let a = ProjectionKey {
            read: vec![r"C:\work".into()],
            write: vec![r"C:\work\out".into()],
            deny: vec![],
        };
        let same = a.clone();
        let narrower = ProjectionKey {
            read: vec![r"C:\work".into()],
            write: vec![],
            deny: vec![],
        };
        let denied = ProjectionKey {
            deny: vec![r"C:\work\secret".into()],
            ..a.clone()
        };
        assert_eq!(a, same);
        assert_ne!(a, narrower);
        assert_ne!(a, denied);
    }

    #[test]
    fn profile_name_is_pid_and_counter_keyed_within_the_length_cap() {
        let n0 = profile_name(0);
        let n1 = profile_name(1);
        assert_ne!(n0, n1);
        assert!(n0.starts_with(&format!("ral.sandbox.s{}.p", std::process::id())));
        // AppContainer profile names cap at 64 UTF-16 code units.
        assert!(profile_name(u32::MAX).encode_utf16().count() <= 64);
    }
}
