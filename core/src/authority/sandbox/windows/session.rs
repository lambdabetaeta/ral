//! Session-scoped `AppContainer` confinement: one profile per distinct fs
//! projection the session confines, minted lazily and held until teardown.
//!
//! A projection's own profile buys it a deny-by-default token and a private
//! named-object namespace; its *filesystem* reach rides elsewhere. Every
//! `(path, kind)` a launch grants names a capability SID (`dacl` derives the
//! name from the canonical path), whose ACE is stamped on the prefix once ever
//! and left there, and a spawn reaches the path only because [`confine`]
//! minted that capability into its token. A token names its own launch's
//! grants and nothing another launch was granted, so a narrowed `grant` — or a
//! subagent forked with narrowed permissions — spawns under a token whose
//! capability set simply omits the wider paths. A profile is keyed by its
//! projection's own fs lists, which the grant fold leaves sorted and
//! deduplicated, so equal keys mean equal projections, not equal spelling;
//! profile names are unique by construction rather than content-hashed, since
//! a collision would merge two projections' namespaces.
//!
//! A deny mints nothing and stamps nothing: Windows cannot take back what a
//! capability's ACE admits (see `dacl`'s `deny` module), so an exclusion holds
//! only where no grant reaches. [`confine`] carves each exclusion out of the
//! allowances holding it ([`crate::authority::sandbox::carve`]), refuses one that sits on
//! or around an allowance, and proves at every launch that the token it builds
//! reaches nothing beneath any of them.
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
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use windows_sys::Win32::Security::{PSID, SID_AND_ATTRIBUTES};
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;

use crate::authority::policy::{FsAllow, GrantPath};
use crate::authority::sandbox::Refusal;
use crate::authority::sandbox::carve;
use crate::authority::sandbox::{FsRules, SandboxProjection};
use crate::path::{Excluded, RealPath, shown};
use crate::process::Launch;
use crate::process::cancel::Scope;
use crate::sync::LockExt as _;

use super::appcontainer::{AppContainerProfile, CapabilitySids, OwnedCapabilitySid};
use super::dacl::{self, DaclError, DaclManager, FrozenPin, GrantKind};

/// One fs projection's identity, as [`FsRules`](crate::authority::sandbox::FsRules)
/// carries it, which names the profile its launches run under: launches the
/// fs grants tell apart never share a named-object namespace. It decides no
/// capability, since a token names its own launch's grants alone, network
/// ones included.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProjectionKey {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    deny: Vec<PathBuf>,
}

/// One shell session's confinement state: one DACL guard for the whole
/// session, one profile per fs projection, and the capability of every grant
/// the session has ensured.
struct SessionSandbox {
    dacl: DaclManager,
    network_caps: Option<CapabilitySids>,
    /// By capability name, entered only once its grant holds, so a hit skips
    /// the stamp's witnesses. The name keys the frozen real path: an object
    /// since replaced under it lost the ACE that went with the old one, so a
    /// hit on it fails closed, the child simply cannot reach the new object.
    fs_cap_sids: HashMap<String, OwnedCapabilitySid>,
    projections: HashMap<ProjectionKey, AppContainerProfile>,
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

    /// `key`'s profile SID, its profile minted if the projection is new. The
    /// index is burned before the create so a failed retry never re-records a
    /// name, and the ledger entry precedes the create so a crash between them
    /// leaves recovery deleting a profile that may not exist, never a
    /// registered profile the ledger does not know.
    fn profile_sid(&mut self, key: ProjectionKey) -> Result<PSID, Refusal> {
        let vacant = match self.projections.entry(key) {
            Entry::Occupied(known) => return Ok(known.get().sid()),
            Entry::Vacant(vacant) => vacant,
        };
        let index = self.next_profile_index;
        self.next_profile_index += 1;
        let name = profile_name(index);
        self.dacl.record_profile(&name)?;
        let profile = AppContainerProfile::create_or_reuse(&name)
            .map_err(|e| io_refusal("sandbox: create AppContainer profile", &e))?;
        Ok(vacant.insert(profile).sid())
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
/// projection to this session's profile for it, make sure each of this
/// launch's grants carries the persistent ACE for its `(path, kind)`
/// capability, and attach the `SECURITY_CAPABILITIES` — profile SID plus
/// exactly those capabilities — that the spawn boundary threads into
/// `CreateProcessW`.
///
/// The stamps are idempotent and shared: a prefix some earlier session already
/// stamped costs a witness check, not another tree walk, and nothing here ever
/// takes one back. What makes this token narrower than another is only which
/// capabilities it names.
///
/// `program_image`, a [`RealPath`] (the guard's resolved image, never
/// re-resolved here: a link on it is refused by the pin), is granted
/// read+execute, to this launch alone, so the `LowBox` token can load the
/// child's binary, mirroring the Linux backend's RO bind of the program path;
/// `None` (a bare name the caller could not resolve) leaves that to the read
/// projection and the `ALL APPLICATION PACKAGES` system paths. An
/// `Unrestricted` projection asks for no capability but its image's, and shares
/// the one empty-key profile — the `AppContainer` is deny-by-default regardless.
///
/// A `deny` path is never stamped: Windows cannot take back access a
/// capability's ACE admits. So an allowance holding an exclusion is carved
/// first, on the tree as it stands at this launch: granted as its existing
/// entries less the excluded ones, down to each exclusion, every directory on
/// the way granted only its own listing ([`GrantKind::List`]). An exclusion on
/// or around an allowance, the program image included, cannot be carved and is
/// refused ([`refuse_deny_meeting_grant`]), before anything is minted. What no
/// grant of this projection then reaches an `AppContainer` token can still meet
/// through the system-wide `ALL APPLICATION PACKAGES` grants, or a hard link
/// carrying a granted file's ACE to a name beneath an exclusion. So once the
/// token's capabilities are settled, [`dacl::ensure_unreachable`] asks the
/// access check itself whether that token could open anything beneath each
/// exclusion, and refuses the launch if it could. Every launch, never
/// memoized: a check passed once says nothing of the tree now.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the session lock is deliberately held for the whole of `confine`: see the comment at the guard"
)]
pub(crate) fn confine(
    launch: &mut Launch,
    projection: &SandboxProjection,
    program_image: Option<&RealPath>,
    cancel: &Scope,
) -> Result<(), Refusal> {
    // Traced in three parts, each a different kind of cost: minting the
    // profile, ensuring the grants, and proving the exclusions.
    #[cfg(debug_assertions)]
    let t_confine = std::time::Instant::now();
    // One critical section on purpose: minting a profile, stamping capability
    // ACEs and reading back the SIDs must not interleave with another thread
    // confining the same session.
    let mut guard = cell().lock_ignore_poison();
    if guard.is_none() {
        *guard = Some(SessionSandbox::create()?);
    }
    let sandbox = guard.as_mut().expect("session created above");

    // Empty when fs is `Unrestricted`: an AppContainer is deny-by-default, so
    // an unattenuated projection grants nothing but the image and rests on the
    // ambient ALL APPLICATION PACKAGES grants.
    let rules = projection.fs.rules().cloned().unwrap_or_default();
    let (read, write) = (
        positions(&rules.read, &rules)?,
        positions(&rules.write, &rules)?,
    );
    let denied: Vec<RealPath> = rules.deny.iter().map(excluded_real).collect();
    let readwrite: Vec<RealPath> = write.iter().map(|&p| p.clone()).collect();
    let readonly: Vec<RealPath> = (read.iter().filter(|p| !write.contains(p)))
        .map(|&p| p.clone())
        .collect();
    let readwrite = carve::carve(&readwrite, &denied)?;
    let mut readonly = carve::carve(&readonly, &denied)?;
    readonly.whole.retain(|p| !readwrite.whole.contains(p));
    if let Some(image) = program_image
        && !readwrite.whole.contains(image)
        && !readonly.whole.contains(image)
    {
        readonly.whole.push(image.clone());
    }
    let granted: Vec<&RealPath> = readwrite.whole.iter().chain(&readonly.whole).collect();
    refuse_deny_meeting_grant(&granted, &denied)?;
    let as_paths = |positions: &[&RealPath]| -> Vec<PathBuf> {
        positions
            .iter()
            .map(|p| p.as_path().to_path_buf())
            .collect()
    };
    let profile_sid = sandbox.profile_sid(ProjectionKey {
        read: as_paths(&read),
        write: as_paths(&write),
        deny: denied.iter().map(|p| p.as_path().to_path_buf()).collect(),
    })?;
    crate::dbg_trace!(
        "sandbox-win",
        "confine: profile ensured in {:?} (read={} write={} deny={})",
        t_confine.elapsed(),
        read.len(),
        write.len(),
        rules.deny.len(),
    );
    let SessionSandbox {
        network_caps,
        fs_cap_sids,
        ..
    } = sandbox;
    let mut carved = readwrite.entries;
    carved.extend(readonly.entries);
    carved.sort();
    carved.dedup();

    // The redirect check comes first, before the memo: a path granted at an
    // earlier launch and since replaced by a junction must refuse this one.
    // The pins it takes are what the stamps then work from, so existence needs
    // no second look.
    #[cfg(debug_assertions)]
    let (t_grants, known) = (std::time::Instant::now(), fs_cap_sids.len());
    let pinned = [
        (
            dacl::pin_all(&readwrite.whole, GrantKind::ReadWrite)?,
            GrantKind::ReadWrite,
        ),
        (
            dacl::pin_all(&readonly.whole, GrantKind::ReadOnly)?,
            GrantKind::ReadOnly,
        ),
        (dacl::pin_all(&carved, GrantKind::List)?, GrantKind::List),
    ];
    // The image and the carving are this launch's, not the projection's.
    let mut names = BTreeSet::new();
    for (pins, kind) in pinned {
        for pin in pins {
            names.insert(ensure_grant(fs_cap_sids, &pin, kind, cancel)?);
        }
    }
    crate::dbg_trace!(
        "sandbox-win",
        "confine: {} fs capability/ies, {} first ensured this session, in {:?}",
        names.len(),
        fs_cap_sids.len() - known,
        t_grants.elapsed(),
    );

    // Copied out of the session's caches: `security_capabilities` copies the
    // slice, but the SIDs it points at stay owned here until teardown.
    let mut capabilities: Vec<SID_AND_ATTRIBUTES> = Vec::new();
    if projection.net {
        capabilities.extend_from_slice(ensure_network_caps(network_caps)?);
    }
    capabilities.extend(names.iter().map(|name| SID_AND_ATTRIBUTES {
        Sid: fs_cap_sids[name].sid(),
        Attributes: SE_GROUP_ENABLED as u32,
    }));

    // Against the very package and capabilities the spawn is handed, so what
    // is checked is the token the child gets.
    #[cfg(debug_assertions)]
    let t_deny = std::time::Instant::now();
    if !rules.deny.is_empty() {
        let access = dacl::ContainerAccess::of(profile_sid, &capabilities)?;
        for denied in &denied {
            dacl::ensure_unreachable(denied, &access, cancel)?;
        }
    }
    crate::dbg_trace!(
        "sandbox-win",
        "confine: {} exclusion(s) proven unreachable in {:?}",
        rules.deny.len(),
        t_deny.elapsed(),
    );
    launch.security_capabilities(profile_sid, &capabilities);
    crate::dbg_trace!("sandbox-win", "confine: total {:?}", t_confine.elapsed());
    Ok(())
}

/// Ensure `pin`'s grant of `kind`, once per session, and name its capability.
fn ensure_grant(
    fs_cap_sids: &mut HashMap<String, OwnedCapabilitySid>,
    pin: &FrozenPin,
    kind: GrantKind,
    cancel: &Scope,
) -> Result<String, Refusal> {
    let name = dacl::fs_capability_name(pin.name(), kind);
    if !fs_cap_sids.contains_key(&name) {
        let sid = OwnedCapabilitySid::from_capability_name(&name)
            .map_err(|e| io_refusal("sandbox: derive fs capability SID", &e))?;
        let sid_string = sid
            .sid_string()
            .map_err(|e| io_refusal("sandbox: fs capability SID string", &e))?;
        dacl::ensure_frozen_grant(pin, &sid_string, kind, cancel)?;
        fs_cap_sids.insert(name.clone(), sid);
    }
    Ok(name)
}

/// The positions of `region`'s allowances, which the stamps and capability
/// names key on, never re-resolved.  Refused where one holds "every path,
/// less exclusions": the universal prefix means every path on every volume,
/// and an `AppContainer` reaches only what is stamped for it; stamped as a
/// path it would narrow to one drive and stamp that whole.  An exclusion of
/// the root is already no exclusion (`deny_all`), and never reaches here.
/// Run before anything is stamped, so the guard alone judges.
fn positions<'a>(region: &'a [FsAllow], rules: &FsRules) -> Result<Vec<&'a RealPath>, Refusal> {
    let positions: Option<Vec<_>> = region.iter().map(FsAllow::real).collect();
    positions.ok_or_else(|| {
        let allows = if rules.deny.is_empty() {
            "every path"
        } else {
            "every path except some exclusions"
        };
        Refusal::Unavailable(format!(
            "this grant allows {allows}, which Windows cannot enforce: a sandboxed program \
             reaches only folders stamped for it, and ral will not stamp every drive; grant \
             the folders the program needs instead"
        ))
    })
}

/// Refuse what carving leaves: an exclusion meeting an allowance granted
/// whole — on it or around it, as only an allowance equal to or inside an
/// exclusion, or the program image, can be once carved. A Windows sandbox
/// reaches everything beneath a granted folder, and nothing can take that
/// access back, so such a pair is a promise the kernel does not keep. Run
/// before anything is minted or stamped.
///
/// Judged on the frozen real paths the stamps key on, under the deny reading
/// of names, which takes every spelling a filesystem could match as the same:
/// a near miss is refused, never let through.
fn refuse_deny_meeting_grant(granted: &[&RealPath], denied: &[RealPath]) -> Result<(), Refusal> {
    for d in denied {
        for g in granted {
            if d.contains_folded(g) || g.contains_folded(d) {
                let (d, g) = (shown(d.as_path()), shown(g.as_path()));
                return Err(Refusal::Unavailable(format!(
                    "this command is allowed '{g}' and '{d}' is excluded, which Windows \
                     cannot enforce together: a sandbox can be kept out of what lies beneath \
                     an allowance, but not out of the allowance itself or what holds it. \
                     Allow paths outside '{d}', run a program outside it, or drop the \
                     exclusion"
                )));
            }
        }
    }
    Ok(())
}

/// Where an exclusion lies: its link's target where its name is one, else
/// the object at its name.  One position, not both as the Unix renderers
/// render (`render_excluded`): the DACL side judged the followed path alone
/// before, carving never grants a link entry anyway, and `ensure_unreachable`
/// holds a position with every link refused, which the link's own would fail.
fn excluded_real(deny: &GrantPath<Excluded>) -> RealPath {
    let held = deny.real.target().unwrap_or_else(|| deny.real.named());
    held.real().clone()
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
        projections,
        next_profile_index: _,
    } = sandbox;
    for profile in projections.into_values() {
        let name = profile.name().to_string();
        match profile.delete() {
            Ok(()) => {
                if let Err(e) = dacl.forget_profile(&name) {
                    crate::io::terminal::shell_warning(&format!(
                        "sandbox session teardown: ledger update for profile {name} failed: {e}"
                    ));
                }
            }
            Err(e) => {
                // The name stays ledgered; the next boot's sweep retries.
                crate::io::terminal::shell_warning(&format!(
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

fn io_refusal(context: &str, e: &std::io::Error) -> Refusal {
    Refusal::Launch(format!("{context}: {e}"))
}

/// A cancel mid-stamp is not a sandbox failure, so it stays a cancellation
/// and never reads as `sandbox: fs grant failed`; nor does a redirected
/// allowance, which says what it is; nor does a position that is not Unicode;
/// and an exclusion the token could still reach is confinement this host
/// cannot establish.
impl From<DaclError> for Refusal {
    fn from(e: DaclError) -> Self {
        match e {
            DaclError::Cancelled(cause) => Self::Cancelled(cause),
            e @ (DaclError::Redirected { .. } | DaclError::NotUnicode(_)) => {
                Self::Launch(e.to_string())
            }
            e @ DaclError::Reachable { .. } => Self::Unavailable(e.to_string()),
            e => Self::Launch(format!("sandbox: fs grant failed: {e}")),
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
