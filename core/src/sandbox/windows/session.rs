//! Session-scoped `AppContainer` confinement: one profile per distinct fs
//! projection the session confines, minted lazily and held until teardown.
//!
//! A projection's own profile buys it a deny-by-default token and a private
//! named-object namespace; its *filesystem* reach rides elsewhere. Every
//! `(path, kind)` a projection grants names a capability SID (`dacl` derives
//! the name from the canonical path), whose ACE is stamped on the prefix once
//! ever and left there, and a spawn reaches the path only because this session
//! minted that capability into its token. So attenuation stays kernel-enforced
//! for the same reason as before, by a cheaper mechanism: a narrowed `grant` —
//! or a subagent forked with narrowed permissions — hashes to a different key
//! and spawns under a token whose capability set simply omits the wider paths.
//! Identity is the projection's own fs prefix lists, which the grant fold
//! leaves sorted and deduplicated, so equal keys mean equal projections, not
//! equal spelling; profile names are unique by construction rather than
//! content-hashed, since a collision would merge two projections' namespaces.
//!
//! Nothing here reverts anything. Grant frames do not, because a
//! worker may outlive its frame under the token it was born with; and
//! [`teardown`] does not either, because a stamped ACE no live token names is
//! inert — the `AppContainer` pass of the access check intersects the user's own,
//! so an unnamed capability grants nobody anything. That is the point of the
//! scheme: the expensive part, propagating an inheritable ACE through a whole
//! tree, is paid once per `(path, kind)` across every session that ever asks.
//! Teardown is left with only the profiles to give back.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use windows_sys::Win32::Security::{PSID, SID_AND_ATTRIBUTES};
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;

use crate::capability::FsScope;
use crate::path::{FrozenPath, RealPath};
use crate::process::Launch;
use crate::process::cancel::Scope;
use crate::sandbox::Refusal;
use crate::sandbox::{FsRules, SandboxProjection};
use crate::sync::LockExt as _;

use super::appcontainer::{AppContainerProfile, CapabilitySids, OwnedCapabilitySid};
use super::dacl::{self, DaclError, DaclManager, FrozenPin, GrantKind};

/// One fs projection's identity, as [`FsRules`](crate::sandbox::FsRules)
/// carries it: the whole `read` list, prefixes and places alike, since
/// capabilities accumulate per key and reach every later token minted under
/// it, so two launches that differ only in a place never share one. `net` is
/// absent on purpose: network authority rides per-spawn on capability SIDs,
/// never stamped on disk, so it needs no SID of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProjectionKey {
    read: Vec<String>,
    write: Vec<String>,
    deny: Vec<String>,
}

/// One projection's profile, the allow `(path, kind)` pairs already ensured
/// for it, and the capability names its grants resolved to — the exact set
/// [`confine`] mints into every token it builds for this projection.
struct ProjectionSandbox {
    profile: AppContainerProfile,
    /// Allow grants only, keyed on the frozen real path: an object replaced
    /// under that name loses the ACE that went with the old NTFS object, so a
    /// memo hit on a stale entry silently skips the restamp. For rw and ro
    /// that fails closed — the child simply cannot reach the new object. A
    /// deny memoized the same way would fail *open*, so denies are never
    /// memoized; see [`ensure_denies`]. The memo is consulted only after the
    /// redirect check, never in place of it.
    granted: HashSet<(PathBuf, GrantKind)>,
    /// In first-grant order, without repeats. A path the filters dropped —
    /// already covered by the well-known `AppContainer` SIDs, or not on disk —
    /// contributes no capability, since there is no stamp for one to name.
    fs_caps: Vec<String>,
}

/// One capability SID this session has derived, with its string spelling
/// cached beside it: both the derivation and the `S-1-…` rendering are pure
/// functions of the name, so each is paid once per name per session.
struct FsCap {
    sid: OwnedCapabilitySid,
    sid_string: String,
}

/// One shell session's confinement state: one DACL guard for the whole
/// session, one sandbox per fs projection, and one derived SID per capability
/// name any of them reached for.
struct SessionSandbox {
    dacl: DaclManager,
    network_caps: Option<CapabilitySids>,
    fs_cap_sids: HashMap<String, FsCap>,
    projections: HashMap<ProjectionKey, ProjectionSandbox>,
    next_profile_index: u32,
}

// SAFETY: the raw SIDs held here (each profile's SID, the network capability
// SIDs, and the `LocalAlloc`'d fs capability SIDs in `fs_cap_sids`) point at
// process-global OS-owned memory, not thread-local state, and live until
// `teardown` frees them. Every access to the struct is serialized by `cell`'s
// mutex; the only lock-free reads are the raw pointer *values*
// `Launch::security_capabilities` copies, and teardown runs at session
// shutdown, never concurrently with a spawn.
#[allow(
    clippy::non_send_fields_in_send_ty,
    reason = "the fields the lint names (`network_caps`, `fs_cap_sids`, `projections`) are exactly the raw SIDs the SAFETY note above argues about: OS-owned process-global memory, not thread-affine state, reached only under [`cell`]'s mutex. The lint can see that a `PSID` is not `Send`; it cannot read the argument for why these ones are."
)]
unsafe impl Send for SessionSandbox {}

fn cell() -> &'static Mutex<Option<SessionSandbox>> {
    static SESSION: LazyLock<Mutex<Option<SessionSandbox>>> = LazyLock::new(|| Mutex::new(None));
    &SESSION
}

impl SessionSandbox {
    fn create() -> Result<Self, Refusal> {
        let dacl = DaclManager::new()?;
        Ok(Self {
            dacl,
            network_caps: None,
            fs_cap_sids: HashMap::new(),
            projections: HashMap::new(),
            next_profile_index: 0,
        })
    }

    /// Mint `key`'s profile if the projection is new. The index is burned
    /// before the create so a failed retry never re-records a name, and the
    /// ledger entry precedes the create so a crash between them leaves
    /// recovery deleting a profile that may not exist, never a registered
    /// profile the ledger does not know.
    fn ensure_projection(&mut self, key: &ProjectionKey) -> Result<(), Refusal> {
        if self.projections.contains_key(key) {
            return Ok(());
        }
        let index = self.next_profile_index;
        self.next_profile_index += 1;
        let name = profile_name(index);
        self.dacl.record_profile(&name)?;
        let profile = AppContainerProfile::create_or_reuse(&name)
            .map_err(|e| io_refusal("sandbox: create AppContainer profile", &e))?;
        self.projections.insert(
            key.clone(),
            ProjectionSandbox {
                profile,
                granted: HashSet::new(),
                fs_caps: Vec::new(),
            },
        );
        Ok(())
    }
}

/// The network capability SIDs, derived once per session. A `net: false`
/// projection never calls this — the empty array *is* the enforcement, since
/// an `AppContainer` with no network capability cannot open a socket.
fn ensure_network_caps(
    slot: &mut Option<CapabilitySids>,
) -> Result<&[SID_AND_ATTRIBUTES], Refusal> {
    if slot.is_none() {
        let caps = CapabilitySids::build(true)
            .map_err(|e| io_refusal("sandbox: derive network capability SIDs", &e))?;
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
/// projection to this session's profile for it, make sure each of its fs
/// prefixes carries the persistent ACE for its `(path, kind)` capability, and
/// attach the `SECURITY_CAPABILITIES` — profile SID plus exactly those
/// capabilities — that the spawn boundary threads into `CreateProcessW`.
///
/// The stamps are idempotent and shared: a prefix some earlier session already
/// stamped costs a witness check, not another tree walk, and nothing here ever
/// takes one back. What makes this token narrower than another is only which
/// capabilities it names.
///
/// `program_image`, which must already be a real path (the guard's resolved
/// image, never re-resolved here: a link on it is refused by the pin), is
/// granted read+execute so the `LowBox` token can load the
/// child's binary, mirroring the Linux backend's RO bind of the program path;
/// `None` (a bare name the caller could not resolve) leaves that to the read
/// projection and the `ALL APPLICATION PACKAGES` system paths. An
/// `Unrestricted` projection asks for no capability at all and shares the one
/// empty-key profile — the `AppContainer` is deny-by-default regardless.
///
/// Every `deny_path` gets its own capability, minted into this projection's
/// tokens and no others: an `AppContainer` token retains the `Everyone` SID and
/// `ALL APPLICATION PACKAGES` grants are system-wide, so a path outside this
/// projection's own grants is still reachable through whatever ambient grant
/// the token already carries — and a deny that were ambient would subtract from
/// every projection, not just this one.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the session lock is deliberately held for the whole of `confine`: see the comment at the guard"
)]
pub(crate) fn confine(
    launch: &mut Launch,
    projection: &SandboxProjection,
    program_image: Option<&Path>,
    cancel: &Scope,
) -> Result<(), Refusal> {
    // Traced in three parts, because each is a different kind of cost and only
    // the middle one is per-path kernel work: minting the profile, the
    // effective-access filter, and the stamp itself.
    #[cfg(debug_assertions)]
    let t_confine = std::time::Instant::now();
    // One critical section on purpose: minting a profile, stamping capability
    // ACEs and reading back the SIDs must not interleave with another thread
    // confining the same session — and `sandbox`/`proj` borrow out of the
    // guard until `security_capabilities`, so an early `drop` would not
    // compile.
    let mut guard = cell().lock_ignore_poison();
    if guard.is_none() {
        *guard = Some(SessionSandbox::create()?);
    }
    let sandbox = guard.as_mut().expect("session created above");

    // Empty when fs is `Unrestricted`: an AppContainer is deny-by-default, so
    // an unattenuated projection asks for nothing and rests on the ambient
    // ALL APPLICATION PACKAGES grants.
    let rules = projection.fs.rules().cloned().unwrap_or_default();
    refuse_universal_prefix(&rules)?;
    let (read, write) = (real_paths(&rules.read), real_paths(&rules.write));
    let deny: Vec<PathBuf> = rules
        .deny
        .iter()
        .map(|p| RealPath::frozen(p).as_path().to_path_buf())
        .collect();
    let key = ProjectionKey {
        read: strings(&read),
        write: strings(&write),
        deny: strings(&deny),
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
        network_caps,
        fs_cap_sids,
        projections,
        ..
    } = sandbox;
    let proj = projections.get_mut(&key).expect("projection ensured above");

    let readwrite = write;
    let mut readonly: Vec<PathBuf> = read
        .into_iter()
        .filter(|p| !readwrite.contains(p))
        .collect();
    if let Some(image) = program_image.map(Path::to_path_buf)
        && !readwrite.contains(&image)
        && !readonly.contains(&image)
    {
        readonly.push(image);
    }

    // The redirect check comes first, before every filter and whatever the
    // memo remembers: a path granted at an earlier launch and since replaced
    // by a junction must refuse this one. The pins it takes are what the
    // filters and the stamps then work from, so existence needs no second look.
    #[cfg(debug_assertions)]
    let t_filter = std::time::Instant::now();
    let readwrite = dacl::pin_all(&readwrite, GrantKind::ReadWrite)?;
    let readonly = dacl::pin_all(&readonly, GrantKind::ReadOnly)?;
    #[cfg(debug_assertions)]
    let (offered_rw, offered_ro) = (readwrite.len(), readonly.len());
    // Cheapest filter first: the memo, then the well-known-SID
    // effective-access check, which is a real `GetSecurityInfo` and DACL walk
    // per path.
    let readwrite = filter_out_granted(&proj.granted, readwrite, GrantKind::ReadWrite);
    let readonly = filter_out_granted(&proj.granted, readonly, GrantKind::ReadOnly);
    let readwrite = dacl::needing_grant(readwrite, dacl::RW_MASK);
    let readonly = dacl::needing_grant(readonly, dacl::RO_MASK);
    crate::dbg_trace!(
        "sandbox-win",
        "confine: access filter kept rw {}/{}, ro {}/{} in {:?}",
        readwrite.len(),
        offered_rw,
        readonly.len(),
        offered_ro,
        t_filter.elapsed(),
    );

    // Denies skip both the memo and the effective-access filter. The filter
    // answers whether the `AppContainer` already *has* this access, which says
    // nothing about whether subtracting it needs a stamp; the memo would
    // answer for a path whose object may since have been replaced, and a deny
    // that skips its restamp is gone. So every deny reaches
    // `dacl::ensure_fs_grant`, whose witness pair — stamp store plus a probe of
    // the live object's own DACL — is the sole authority for it, at one
    // `GetNamedSecurityInfoW` per deny path per launch.
    let deny = existing_paths(deny);

    // Each of these is a witness check on a path already stamped, and a whole
    // tree propagation on one that is not — once ever, for every session.
    #[cfg(debug_assertions)]
    let t_stamp = std::time::Instant::now();
    #[cfg(debug_assertions)]
    let (kept_rw, kept_ro, kept_deny) = (readwrite.len(), readonly.len(), deny.len());
    ensure_allows(proj, fs_cap_sids, readwrite, GrantKind::ReadWrite, cancel)?;
    ensure_allows(proj, fs_cap_sids, readonly, GrantKind::ReadOnly, cancel)?;
    ensure_denies(proj, fs_cap_sids, deny, cancel)?;
    crate::dbg_trace!(
        "sandbox-win",
        "confine: ensured {kept_rw} rw + {kept_ro} ro + {kept_deny} deny capability ACEs in {:?} \
         ({} capability/ies on this projection's tokens)",
        t_stamp.elapsed(),
        proj.fs_caps.len(),
    );

    let profile_sid: PSID = proj.profile.sid();
    // Copied out of the session's caches: `security_capabilities` copies the
    // slice, but the SIDs it points at stay owned here until teardown.
    let mut capabilities: Vec<SID_AND_ATTRIBUTES> = Vec::new();
    if projection.net {
        capabilities.extend_from_slice(ensure_network_caps(network_caps)?);
    }
    for name in &proj.fs_caps {
        let cap = fs_cap_sids
            .get(name)
            .expect("every name in fs_caps was derived when its grant was ensured");
        capabilities.push(SID_AND_ATTRIBUTES {
            Sid: cap.sid.sid(),
            Attributes: SE_GROUP_ENABLED as u32,
        });
    }
    launch.security_capabilities(profile_sid, &capabilities);
    crate::dbg_trace!("sandbox-win", "confine: total {:?}", t_confine.elapsed());
    Ok(())
}

/// The capability `name` derives to, once per name per session.
fn derive<'a>(
    fs_cap_sids: &'a mut HashMap<String, FsCap>,
    name: &str,
) -> Result<&'a FsCap, Refusal> {
    Ok(match fs_cap_sids.entry(name.to_owned()) {
        Entry::Occupied(e) => e.into_mut(),
        Entry::Vacant(v) => {
            let sid = OwnedCapabilitySid::from_capability_name(name)
                .map_err(|e| io_refusal("sandbox: derive fs capability SID", &e))?;
            let sid_string = sid
                .sid_string()
                .map_err(|e| io_refusal("sandbox: fs capability SID string", &e))?;
            v.insert(FsCap { sid, sid_string })
        }
    })
}

/// Record `name` on the projection's tokens: ordered and deduplicated, so a
/// token's capability list reads as the projection's grant history.
fn remember(proj: &mut ProjectionSandbox, name: String) {
    if !proj.fs_caps.contains(&name) {
        proj.fs_caps.push(name);
    }
}

/// Ensure one kind's allows, each stamped through the handles its check
/// holds, under the capability its frozen real path names: a witness check on
/// a path already stamped, a whole tree propagation on one that is not.
fn ensure_allows(
    proj: &mut ProjectionSandbox,
    fs_cap_sids: &mut HashMap<String, FsCap>,
    pins: Vec<FrozenPin>,
    kind: GrantKind,
    cancel: &Scope,
) -> Result<(), Refusal> {
    for pin in pins {
        let name = dacl::fs_capability_name(pin.path(), kind);
        let cap = derive(fs_cap_sids, &name)?;
        dacl::ensure_frozen_grant(&pin, &cap.sid_string, kind, cancel)?;
        // The memo is a hint, never the check: `confine` pins every allowance
        // before it consults it, and an object replaced under this name loses
        // the ACE that went with the old one, which fails closed.
        proj.granted.insert((pin.path().to_path_buf(), kind));
        remember(proj, name);
    }
    Ok(())
}

/// Ensure the denies. An exclusion was re-resolved when the region was built
/// and is canonicalised here, the one place it meets the disk, and never
/// memoized: the same staleness that costs an
/// allow its access would restore access the projection refused.
fn ensure_denies(
    proj: &mut ProjectionSandbox,
    fs_cap_sids: &mut HashMap<String, FsCap>,
    paths: Vec<PathBuf>,
    cancel: &Scope,
) -> Result<(), Refusal> {
    for p in paths {
        let canonical = dacl::canonicalize_grant_target(&p)?;
        let name = dacl::fs_capability_name(&canonical, GrantKind::Deny);
        let cap = derive(fs_cap_sids, &name)?;
        dacl::ensure_fs_grant(&canonical, &cap.sid_string, GrantKind::Deny, cancel)?;
        remember(proj, name);
    }
    Ok(())
}

/// Refuse "every path, less exclusions". The universal prefix means every
/// path on every volume, and an `AppContainer` reaches only what is stamped for
/// it; stamped as a path it would narrow to one drive and stamp that whole.
/// The root is refused however it arrives (a prefix, a run place, an
/// exclusion), since the drive it opens is stamped, or walked, whole. Run before anything is stamped, so the guard
/// alone judges.
fn refuse_universal_prefix(rules: &FsRules) -> Result<(), Refusal> {
    let bare = |p: &Path| crate::path::lex::is_bare_root(&p.to_string_lossy());
    let universal = |s: &FsScope| match s {
        FsScope::Prefix(p) => p.is_root() || bare(RealPath::frozen(p).as_path()),
        FsScope::Place(r) => bare(r.as_path()),
    };
    let excluded = |p: &FrozenPath| p.is_root() || bare(RealPath::frozen(p).as_path());
    if !rules.read.iter().chain(&rules.write).any(universal) && !rules.deny.iter().any(excluded) {
        return Ok(());
    }
    let allows = if rules.deny.is_empty() {
        "every path"
    } else {
        "every path except some exclusions"
    };
    Err(Refusal::Unavailable(format!(
        "this grant allows {allows}, which Windows cannot enforce: a sandboxed program \
         reaches only folders stamped for it, and ral will not stamp every drive; grant \
         the folders the program needs instead"
    )))
}

/// A region's frozen real paths, which the stamps and capability names key
/// on, never re-resolved.
fn real_paths(region: &[FsScope]) -> Vec<PathBuf> {
    region
        .iter()
        .map(|scope| match scope {
            FsScope::Prefix(p) => RealPath::frozen(p).as_path().to_path_buf(),
            FsScope::Place(r) => r.as_path().to_path_buf(),
        })
        .collect()
}

fn strings(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Recovery failures are logged; they do not prevent session startup.
pub(crate) fn boot_recover() {
    match dacl::recover_orphaned_state() {
        Ok(report) => {
            if report.files_processed > 0 {
                crate::dbg_trace!(
                    "sandbox-win",
                    "orphan DACL recovery: {} ledger(s) processed, \
                     {} profile(s) deleted",
                    report.files_processed,
                    report.profiles_deleted,
                );
            }
            #[cfg(debug_assertions)]
            for e in &report.errors {
                crate::dbg_trace!("sandbox-win", "orphan DACL recovery: {e}");
            }
        }
        #[cfg_attr(not(debug_assertions), allow(unused_variables))]
        Err(e) => crate::dbg_trace!("sandbox-win", "orphan DACL recovery failed: {e}"),
    }
}

/// Delete every projection's profile. The capability ACEs stay: they are
/// persistent by design, and one no live token names grants nobody anything, so
/// there is nothing here to revert — only the OS-level registrations this
/// session made. Idempotent, and the explicit path because the process-global's
/// `Drop` is not guaranteed at exit; a session that never reaches here leaves
/// its ledger for the next boot's sweep.
pub(crate) fn teardown() {
    let Some(sandbox) = cell().lock_ignore_poison().take() else {
        return;
    };
    // Traced separately from `confine`: one profile delete per projection, and
    // no ACE work at all — an exit that looks like a hang is no longer this.
    #[cfg(debug_assertions)]
    let t_teardown = std::time::Instant::now();
    let SessionSandbox {
        mut dacl,
        network_caps: _,
        fs_cap_sids: _,
        mut projections,
        next_profile_index: _,
    } = sandbox;
    for (_, proj) in projections.drain() {
        let name = proj.profile.name().to_string();
        match proj.profile.delete() {
            Ok(()) => {
                if let Err(e) = dacl.forget_profile(&name) {
                    crate::terminal::shell_warning(&format!(
                        "sandbox session teardown: ledger update for profile {name} failed: {e}"
                    ));
                }
            }
            Err(e) => {
                // The name stays ledgered; the next boot's sweep retries.
                crate::terminal::shell_warning(&format!(
                    "sandbox session teardown: delete AppContainer profile {name} failed: {e}"
                ));
            }
        }
    }
    crate::dbg_trace!(
        "sandbox-win",
        "teardown: profiles deleted in {:?}",
        t_teardown.elapsed()
    );
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
fn filter_out_granted<P: AsRef<Path>>(
    granted: &HashSet<(PathBuf, GrantKind)>,
    paths: Vec<P>,
    kind: GrantKind,
) -> Vec<P> {
    paths
        .into_iter()
        .filter(|p| !granted.contains(&(p.as_ref().to_path_buf(), kind)))
        .collect()
}

fn io_refusal(context: &str, e: &std::io::Error) -> Refusal {
    Refusal::Launch(format!("{context}: {e}"))
}

/// A cancel mid-stamp is not a sandbox failure, so it stays a cancellation
/// and never reads as `sandbox: fs grant failed`; nor does a redirected
/// allowance, which says what it is.
impl From<DaclError> for Refusal {
    fn from(e: DaclError) -> Self {
        match e {
            DaclError::Cancelled(cause) => Self::Cancelled(cause),
            e @ DaclError::Redirected { .. } => Self::Launch(e.to_string()),
            e => Self::Launch(format!("sandbox: fs grant failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests;
