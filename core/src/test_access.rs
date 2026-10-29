//! The reach tests have, and no host does.
//!
//! Core's integration tests and the front-ends' tests link `ral-core` as an
//! external crate, so an internal they assert on would otherwise have to be
//! public to every embedder
//! (`docs/ral-wiki/decisions/260909_pub-crate-by-default.md`).
//! Representation-local tests belong beside their implementations. The doors
//! here serve cross-component tests, and keep their test-only reach in one
//! auditable list without widening the production API.
//!
//! Gated on `test-util`, which `core`'s own dev-dependency on itself turns on
//! for every test, example and benchmark target of this package, and a
//! front-end's dev-dependency on `core` turns on for its own.  With the
//! feature off the module does not exist, so the items behind it stay
//! `pub(crate)` and `dead_code` still names one whose last in-crate caller
//! went away.

use crate::authority::guard::check_exec;
use crate::authority::policy::FsAllow;
use crate::authority::policy::Program;
#[cfg(unix)]
use crate::authority::policy::{ExecAllow, ExecDeny};
use crate::authority::sandbox::FsProjection;
#[cfg(unix)]
use crate::authority::sandbox::{ExecProjection, SandboxProjection};
use crate::carrier::{IdentityTransport, ProbeError, Transport};
use crate::data::FOValue;
use crate::ir::{CaseArm, Comp, CompKind, Exec, Val, ValListElem};
#[cfg(unix)]
use crate::path::Walked;
use crate::protocol::Frame;
use crate::protocol::channel::{WireChannel, WireStream};
use crate::protocol::probe::Probe;
use crate::shell::runtime::{BuiltinEntry, Mooring, Settled, Shell};
use crate::ty::{Scheme, Ty};
use crate::typecheck::Unifier;

/// The thunk a case arm forces: its literal block, or a thunk in hand.
pub fn case_arm_val(arm: &CaseArm) -> &Val {
    &arm.body.item
}

/// The arguments of a call — an `Exec`'s argv or an `App`'s — in order,
/// spread and single alike; empty for any other node.
pub fn call_args(comp: &Comp) -> impl Iterator<Item = &Val> {
    let args: &[ValListElem] = match &comp.item {
        CompKind::Exec(exec) => &exec.args,
        CompKind::App { args, .. } => args,
        _ => &[],
    };
    args.iter().map(|elem| &elem.slot().item)
}

/// Whether an `Exec` carries redirects, which a fuzz invariant asserts it
/// eventually generates.
pub fn exec_has_redirects(exec: &Exec) -> bool {
    !exec.redirects.is_empty()
}

/// The scheme a builtin's type rule mints against `unifier` — the rule
/// itself is core's, so a caller runs it rather than holding it.
pub fn builtin_scheme(entry: &BuiltinEntry, unifier: &mut Unifier) -> Scheme {
    (entry.decl.type_rule)(unifier)
}

/// The real paths the fs projection reads, baseline, run places and support
/// among them; `None` at the unrestricted top.
pub fn fs_read_paths<A, X>(projection: &FsProjection<A, X>) -> Option<Vec<String>>
where
    A: ReadName,
{
    projection
        .rules()
        .map(|rules| rules.read.iter().map(ReadName::real_name).collect())
}

/// How an allowance names the real path it grants.
pub trait ReadName {
    fn real_name(&self) -> String;
}

impl ReadName for FsAllow {
    fn real_name(&self) -> String {
        self.real()
            .map_or_else(|| "/".to_string(), ToString::to_string)
    }
}

/// One exec rule as the kernel is handed it, real paths spelled out.
#[cfg(unix)]
#[derive(Debug)]
pub enum KernelExecRule {
    /// Every program.
    Any {
        allow: bool,
    },
    /// The directory and everything beneath it.
    Dir {
        path: String,
        allow: bool,
    },
    File {
        path: String,
        allow: bool,
    },
    /// A deny on every file of this final component.
    Name(String),
}

/// The exec rules the kernel is handed, support among them: every positive
/// clause, then every negative one; `None` when exec is unrestricted.
#[cfg(unix)]
pub fn exec_projection_rules(projection: &SandboxProjection) -> Option<Vec<KernelExecRule>> {
    let ExecProjection::Restricted(rules) = &projection.exec else {
        return None;
    };
    let positives = rules.allows().filter_map(|scope| match scope {
        ExecAllow::Any => Some(KernelExecRule::Any { allow: true }),
        ExecAllow::Dir(dir) => Some(KernelExecRule::Dir {
            path: dir.real.position().to_string(),
            allow: true,
        }),
        ExecAllow::File { real, .. } => Some(KernelExecRule::File {
            path: real.to_string(),
            allow: true,
        }),
        ExecAllow::Tool(_) => None,
    });
    let negatives = rules.denies().map(|scope| match scope {
        ExecDeny::Dir(dir) => KernelExecRule::Dir {
            path: dir.real.followed().real().to_string(),
            allow: false,
        },
        ExecDeny::File(file) => KernelExecRule::File {
            path: file.real.followed().real().to_string(),
            allow: false,
        },
        ExecDeny::Name(name) => KernelExecRule::Name(name.clone()),
    });
    Some(positives.chain(negatives).collect())
}

/// Whether the projection restricts exec at all.
#[cfg(unix)]
pub fn exec_projection_restricts(projection: &SandboxProjection) -> bool {
    matches!(projection.exec, ExecProjection::Restricted(_))
}

/// The files the projection's exec rules admit by name, as real paths.
#[cfg(unix)]
pub fn exec_projection_files(projection: &SandboxProjection) -> Vec<String> {
    match &projection.exec {
        ExecProjection::Unrestricted => Vec::new(),
        ExecProjection::Restricted(rules) => {
            rules.allowed_files().map(ToString::to_string).collect()
        }
    }
}

/// A host file whose real path, and launch path, is `real`, which exists: a
/// program is the object a walk holds.
fn file(real: &str) -> Program {
    Program::File {
        located: Box::new(crate::test_helper::located(real)),
        arg0: real.into(),
    }
}

/// Whether the live stack admits running the file whose real path is `real`.
pub fn admits_file(shell: &Shell, real: &str) -> bool {
    shell
        .context
        .grants
        .admit(shell.baseline(), file(real))
        .is_ok()
}

/// The exec door on the file whose real path is `real`, its refusal recorded.
///
/// # Errors
/// `Err` if the active grant denies the file.
pub fn check_file(shell: &mut Shell, real: &str) -> Settled<()> {
    check_exec(&shell.context.grants, shell.baseline(), real, file(real))
        .map(drop)
        .map_err(|denial| shell.exec_refused(denial, &Mooring::adrift()))
}

/// The scheme `name` carries on the live scope: `None` when it is unbound or
/// bound without one.
pub fn binding_scheme(shell: &Shell, name: &str) -> Option<std::sync::Arc<Scheme>> {
    shell
        .session_schemes()
        .bindings
        .into_iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, scheme)| scheme)
}

/// `elaborator::word_val`: the value a bare word denotes — the numeral doctrine's
/// single point of decision.
pub fn val_from_word(word: &str) -> Val {
    crate::elaborator::word_val(word)
}

/// The peer end of a wire, driven directly: `core/tests/wire_write_stall.rs`
/// plays the far side of a transport the host would own.
pub fn wire_from_stream(stream: impl Into<WireStream>) -> WireChannel {
    WireChannel::from_stream(stream)
}

/// One frame written on such a peer channel.
///
/// # Errors
/// The write's own failure.
pub fn wire_write_frame(channel: &mut WireChannel, frame: &Frame) -> std::io::Result<()> {
    channel.write_frame(frame)
}

/// One frame read from such a peer channel, `None` at a clean end.
///
/// # Errors
/// The read's own failure.
pub fn wire_read_frame(channel: &mut WireChannel) -> std::io::Result<Option<Frame>> {
    channel.read_frame()
}

/// How many roots and path components this thread's walks have opened so
/// far: what a test reads to show that something walked nothing, or walked
/// once.
pub fn walk_steps() -> usize {
    crate::path::walk::steps_taken()
}

/// The engine's answer to one probe, read straight off `shell`.
pub fn answer_probe(shell: &Shell, probe: &Probe) -> FOValue {
    crate::engine::probe::answer(shell, probe)
}

/// How many workers the engine behind `transport` still holds.
///
/// # Errors
/// As [`Transport::cwd`].
pub fn worker_count(transport: &dyn Transport) -> Result<u64, ProbeError> {
    crate::carrier::read(transport, &Probe::WorkerCount)
}

/// How many capability layers the engine behind `transport` carries, root
/// layers included.
///
/// # Errors
/// As [`Transport::cwd`].
pub fn grant_depth(transport: &dyn Transport) -> Result<u64, ProbeError> {
    crate::carrier::read(transport, &Probe::GrantDepth)
}

/// `Shell::workers` on the engine behind `transport`, each entry's handle
/// included, for a test watching a worker outlive its engine.
pub fn workers(transport: &IdentityTransport) -> Vec<crate::shell::runtime::WorkerEntry> {
    transport.inspect(Shell::workers)
}

/// The site of a boundary call whose result the checker solved as `ty`: what a
/// test hands a door it calls directly, bypassing the checker.
pub fn site_of(ty: &Ty) -> std::sync::Arc<crate::ty::Site> {
    std::sync::Arc::new(crate::ty::Site::snapshot(
        &Unifier::new(),
        ty,
        crate::ty::UnitId::fresh(),
    ))
}

/// Whether a scheme quantifies a variable only its result mentions: what the
/// perimeter test asks of every table a host installs.
pub fn has_result_only_var(scheme: &Scheme) -> bool {
    crate::typecheck::has_result_only_var(scheme)
}

/// Whether a scheme leaves weak variables unquantified — the mark a boundary's
/// result carries into a stored binding.
pub fn has_weak_residuals(scheme: &Scheme) -> bool {
    !scheme.weak.is_empty()
}
