//! The reach `core/tests/*` has, and no host does.
//!
//! Core's integration tests link `ral-core` as an external crate, so an
//! internal they assert on would otherwise have to be public to every
//! embedder (`docs/ral-wiki/decisions/260909_pub-crate-by-default.md`).
//! Each door here goes through a `pub(crate)` item behaviourally, never by
//! handing out core's representation, and the whole test-only reach is one
//! auditable list.
//!
//! Gated on `test-util`, which `core`'s own dev-dependency on itself turns on
//! for every test, example and benchmark target of this package.  With the
//! feature off the module does not exist, so the items behind it stay
//! `pub(crate)` and `dead_code` still names one whose last in-crate caller
//! went away.

use crate::ir::{CaseArm, Comp, Exec, Val};
use crate::typecheck::{Scheme, Ty, TypeError, Unifier};
use crate::types::{BuiltinEntry, FsProjection, FsRules, Settled, Shell};

/// The computation a case arm runs, whichever way the surface reached it —
/// an arm's `ArmBody` spelling is core's business, the branch is the test's.
pub fn case_arm_comp(arm: &CaseArm) -> &std::sync::Arc<Comp> {
    arm.body.comp()
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
    (entry.type_rule)(unifier)
}

/// The type a scheme quantifies over.
pub fn scheme_ty(scheme: &Scheme) -> &Ty {
    &scheme.ty
}

/// The constraint provenance behind a type error, which `hint` composes into
/// prose and a test asserts on directly.
pub fn type_error_reason(err: &TypeError) -> Option<&crate::typecheck::Reason> {
    err.reason.as_ref()
}

/// `FsProjection::rules`: the rules when restricted, `None` at the top.
pub fn fs_rules<N>(projection: &FsProjection<N>) -> Option<&FsRules<N>> {
    projection.rules()
}

/// `Shell::check_exec_call`: the grant gate one exec faces.
///
/// # Errors
/// `Err` if the active grant denies the command, or admits only a
/// subcommand set that `args`'s first element misses.
pub fn check_exec_call(
    shell: &mut Shell,
    display_name: &str,
    deny_names: &[&str],
    policy_names: &[&str],
    args: &[String],
) -> Settled<()> {
    shell.check_exec_call(display_name, deny_names, policy_names, args)
}

/// `Shell::leased_binding_count`: how many bindings hold a terminal lease.
pub fn leased_binding_count(shell: &Shell) -> usize {
    shell.leased_binding_count()
}

/// `Val::from_word`: the value a bare word denotes — the numeral doctrine's
/// single point of decision.
pub fn val_from_word(word: &str) -> Val {
    Val::from_word(word)
}

/// A scheme quantifying `comp_ty_vars` over `ty`.
///
/// `comp_ty_bindings` snapshots a cyclic root.  This is the one scheme shape
/// the formatter's tests write by hand, so they need not know the rest of a
/// scheme's fields.
pub fn scheme_over_comp_vars(
    comp_ty_vars: Vec<crate::typecheck::CompTyVar>,
    ty: Ty,
    comp_ty_bindings: Vec<(u32, crate::typecheck::CompTy)>,
) -> Scheme {
    Scheme {
        comp_ty_vars,
        ty,
        comp_ty_bindings,
        ..Scheme::mono(Ty::Unit)
    }
}

/// The peer end of a wire, driven directly: `core/tests/wire_write_stall.rs`
/// plays the far side of a transport the host would own.
pub fn wire_from_stream(stream: impl Into<crate::wire::WireStream>) -> crate::wire::WireChannel {
    crate::wire::WireChannel::from_stream(stream)
}

/// One frame written on such a peer channel.
///
/// # Errors
/// The write's own failure.
pub fn wire_write_frame(
    channel: &mut crate::wire::WireChannel,
    frame: &crate::protocol::Frame,
) -> std::io::Result<()> {
    channel.write_frame(frame)
}

/// One frame read from such a peer channel, `None` at a clean end.
///
/// # Errors
/// The read's own failure.
pub fn wire_read_frame(
    channel: &mut crate::wire::WireChannel,
) -> std::io::Result<Option<crate::protocol::Frame>> {
    channel.read_frame()
}

/// The full ariadne rendering of one type error, as the REPL prints it.
pub fn format_type_error_ariadne(file: &str, source: &str, err: &TypeError) -> String {
    crate::diagnostic::format_type_error_ariadne(file, source, err)
}
