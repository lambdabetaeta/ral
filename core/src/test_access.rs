//! The reach tests have, and no host does.
//!
//! Core's integration tests and the front-ends' tests link `ral-core` as an
//! external crate, so an internal they assert on would otherwise have to be
//! public to every embedder
//! (`docs/ral-wiki/decisions/260909_pub-crate-by-default.md`).
//! Each door here goes through a `pub(crate)` item behaviourally, never by
//! handing out core's representation, and the whole test-only reach is one
//! auditable list.
//!
//! Gated on `test-util`, which `core`'s own dev-dependency on itself turns on
//! for every test, example and benchmark target of this package, and a
//! front-end's dev-dependency on `core` turns on for its own.  With the
//! feature off the module does not exist, so the items behind it stay
//! `pub(crate)` and `dead_code` still names one whose last in-crate caller
//! went away.

#[cfg(unix)]
use crate::capability::ExecScope;
use crate::capability::Program;
use crate::carrier::{IdentityTransport, ProbeError, Transport};
use crate::first_order::FOValue;
use crate::ir::{CaseArm, Comp, CompKind, Exec, Val, ValListElem};
use crate::path::RealPath;
use crate::protocol::Frame;
use crate::protocol::channel::{WireChannel, WireStream};
use crate::protocol::probe::Probe;
#[cfg(unix)]
use crate::sandbox::{ExecProjection, SandboxProjection};
use crate::sandbox::{FsProjection, FsRules};
use crate::ty::{Scheme, Ty};
use crate::typecheck::Unifier;
use crate::types::{BuiltinEntry, Settled, Shell};

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

/// `Unifier::resolve_ty`: the type a variable stands for right now.
pub fn resolve_ty(unifier: &mut Unifier, ty: &Ty) -> Ty {
    unifier.resolve_ty(ty)
}

/// The scheme a builtin's type rule mints against `unifier` — the rule
/// itself is core's, so a caller runs it rather than holding it.
pub fn builtin_scheme(entry: &BuiltinEntry, unifier: &mut Unifier) -> Scheme {
    (entry.decl.type_rule)(unifier)
}

/// The type a scheme quantifies over.
pub fn scheme_ty(scheme: &Scheme) -> &Ty {
    &scheme.ty
}

/// `FsProjection::rules`: the rules when restricted, `None` at the top.
pub fn fs_rules<N>(projection: &FsProjection<N>) -> Option<&FsRules<N>> {
    projection.rules()
}

/// One exec rule as the kernel is handed it, real paths spelled out.
#[cfg(unix)]
#[derive(Debug)]
pub enum KernelExecRule {
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

/// The exec rules the kernel is handed, in the order a last-match-wins
/// renderer emits them; `None` when exec is unrestricted.
#[cfg(unix)]
pub fn exec_projection_rules(projection: &SandboxProjection) -> Option<Vec<KernelExecRule>> {
    let ExecProjection::Restricted(rules) = &projection.exec else {
        return None;
    };
    let kernel = (rules.precedence().into_iter()).filter_map(|(scope, verdict)| {
        let allow = !verdict.is_denied();
        match scope {
            ExecScope::Dir(path) => Some(KernelExecRule::Dir {
                path: path.to_string(),
                allow,
            }),
            ExecScope::File(path) => Some(KernelExecRule::File {
                path: path.to_string(),
                allow,
            }),
            ExecScope::Name(name) => Some(KernelExecRule::Name(name.clone())),
            ExecScope::Carrier(_) | ExecScope::Tool(_) => None,
        }
    });
    Some(kernel.collect())
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

/// A host file whose real path, and launch path, is `real`.
fn file(real: &str) -> Program {
    Program::File {
        path: real.into(),
        real: RealPath::assumed(real),
    }
}

/// Whether the live stack admits running the file whose real path is `real`.
pub fn admits_file(shell: &Shell, real: &str) -> bool {
    shell.context.grants.admits(&file(real))
}

/// `Shell::check_exec` on the file whose real path is `real`, with `args`.
///
/// # Errors
/// `Err` if the active grant denies the file, or admits only a subcommand
/// set that `args`'s first element misses.
pub fn check_file(shell: &mut Shell, real: &str, args: &[String]) -> Settled<()> {
    shell.check_exec(real, file(real), args.to_vec()).map(drop)
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

/// `Shell::leased_binding_count`: how many bindings hold a terminal lease.
pub fn leased_binding_count(shell: &Shell) -> usize {
    shell.leased_binding_count()
}

/// `elaborator::word_val`: the value a bare word denotes — the numeral doctrine's
/// single point of decision.
pub fn val_from_word(word: &str) -> Val {
    crate::elaborator::word_val(word)
}

/// A scheme quantifying `comp_ty_vars` over `ty`.
///
/// `comp_ty_bindings` snapshots a cyclic root.  This is the one scheme shape
/// the formatter's tests write by hand, so they need not know the rest of a
/// scheme's fields.
pub fn scheme_over_comp_vars(
    comp_ty_vars: Vec<crate::ty::CompTyVar>,
    ty: Ty,
    comp_ty_bindings: Vec<(crate::ty::CompTyVar, crate::ty::CompTy)>,
) -> Scheme {
    Scheme {
        comp_ty_vars,
        ty,
        comp_ty_bindings,
        ..Scheme::mono(Ty::Unit)
    }
}

/// A scheme over `ty` that leaves `weak` type variables unquantified, as a
/// unit stores a binding that mentions a weak variable.
pub fn scheme_with_weak_residuals(weak: Vec<crate::ty::TyVar>, ty: Ty) -> Scheme {
    Scheme {
        weak: crate::ty::WeakVars {
            tys: weak
                .into_iter()
                .map(|v| (v, crate::ty::Kind::ANY))
                .collect(),
            ..Default::default()
        },
        ..Scheme::mono(ty)
    }
}

/// `schemes` with `stored` added as bindings earlier units left behind.
pub fn with_stored_schemes(
    mut schemes: crate::typecheck::SessionSchemes,
    stored: Vec<(String, Scheme)>,
) -> crate::typecheck::SessionSchemes {
    schemes.bindings.extend(
        stored
            .into_iter()
            .map(|(name, scheme)| (name, Some(std::sync::Arc::new(scheme)))),
    );
    schemes
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

/// How many capability frames the engine behind `transport` carries.
///
/// # Errors
/// As [`Transport::cwd`].
pub fn grant_depth(transport: &dyn Transport) -> Result<u64, ProbeError> {
    crate::carrier::read(transport, &Probe::GrantDepth)
}

/// `Shell::workers` on the engine behind `transport`, each entry's handle
/// included, for a test watching a worker outlive its engine.
pub fn workers(transport: &IdentityTransport) -> Vec<crate::types::WorkerEntry> {
    transport.inspect(Shell::workers)
}

/// The site of a boundary call whose result the checker solved as `ty`: what a
/// test hands a door it calls directly, bypassing the checker.
pub fn site_of(ty: &Ty) -> std::sync::Arc<crate::ty::Site> {
    std::sync::Arc::new(crate::ty::Site::snapshot(
        &Unifier::new(),
        ty,
        crate::ty::Fixings::new(),
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
