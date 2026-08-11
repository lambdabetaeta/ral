//! Where a computation's payload lives.
//!
//! A computation writes bytes to stdout and, independently, returns a value.
//! Neither fact needs the type system: stdout is an operating-system stream
//! whose sink is chosen by position, and a returned value is simply the
//! evaluator's result.  What *does* need saying is which of the two a value
//! boundary should observe when it demands the computation as a value — a
//! `let`, a branch join, the final report of a pipeline.  That is the payload
//! route, and it is the whole of what survives of the old three-mode lattice.
//!
//! The route is not an output predicate.  A `Value`-routed computation may
//! write any number of bytes; a `Bytes`-routed one may write none.
//!
//! These types ride inside a [`super::Scheme`] into the postcard-baked
//! prelude, which carries no schema of its own; the serde derives are
//! load-bearing.
//!
//! They live under `typecheck` and go no further: elaboration commits every
//! route decision to explicit syntax — a [`crate::ir::PipeYield`], a
//! [`crate::ir::CompKind::Capture`] — so no route reaches the evaluator, and
//! the module boundary is what says so.

/// Unification variable for payload routes.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct PayloadVar(pub u32);

/// Where a computation's payload lives when a value boundary demands it:
/// `Value` takes the evaluator's return, `Bytes` captures its stdout.
///
/// The formation rule is `Bytes` implies the returned value is `Unit`, so
/// there is exactly one byte-routed computation type; landing on the byte
/// side means unifying with it whole, never writing a bare route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PayloadRoute {
    Value,
    Bytes,
    Var(PayloadVar),
}

/// A resolved [`PayloadRoute`], so "annotations are ground" is a fact of the
/// type rather than an invariant the reader must trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(in crate::typecheck) enum GroundRoute {
    Value,
    Bytes,
}

impl From<GroundRoute> for PayloadRoute {
    fn from(route: GroundRoute) -> Self {
        match route {
            GroundRoute::Value => Self::Value,
            GroundRoute::Bytes => Self::Bytes,
        }
    }
}

/// A `Value`/`Bytes` clash raised by `Unifier::unify_route`.
///
/// Each caller maps it onto its own diagnostic: the checker's
/// [`super::TypeErrorKind::RouteMismatch`], or a rejected handler arm in
/// `typecheck::alias_arm_scheme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteMismatch {
    pub left: PayloadRoute,
    pub right: PayloadRoute,
}
