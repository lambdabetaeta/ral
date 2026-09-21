//! Shared scaffolding for `core/tests/*.rs`: a once-elaborated prelude
//! `Comp` and the schemes baked from it.  Both are memoised so the
//! prelude is parsed and elaborated exactly once per test binary.
//!
//! Also installs a pre-main constructor that serves the shared re-exec
//! stages (see [`ral_core::test_helper::run_pre_main_reexec_stages`]):
//! the pipeline anchor and capture-active standalone invocations of bundled
//! coreutils tools re-exec `current_exe()` — the test binary itself — so
//! without the constructor the re-exec would land in the test framework
//! instead of the helper, and bundled `test`/`wc`/`stat` would never run.

#![allow(dead_code)] // not every test file uses every helper

use ral_core::boot::{BakedPrelude, HostSurface, boot_shell};
use ral_core::io::TerminalState;
use ral_core::types::Shell;
use ral_core::{Scheme, ir::Comp, ir::Toplevel};
use std::sync::{Arc, OnceLock};

#[ctor::ctor(unsafe)]
#[allow(
    clippy::disallowed_methods,
    reason = "a re-exec stage that has served its request, in a ctor before main: no shell, reaper or staged write exists in the process yet"
)]
fn init_test_binary() {
    if let Some(code) = ral_core::test_helper::run_pre_main_reexec_stages() {
        std::process::exit(i32::from(code));
    }
}

/// The prelude baked once at runtime (test binaries have no build-time
/// blob), memoised for the accessors below and for `boot_shell`.
pub fn prelude() -> &'static BakedPrelude {
    static B: OnceLock<BakedPrelude> = OnceLock::new();
    B.get_or_init(BakedPrelude::bake_runtime)
}

/// A shell booted as every front end boots one: core's builtin surface,
/// the default env vars, the prelude registered with its schemes.
pub fn fresh_shell() -> Shell {
    boot_shell(TerminalState::default(), prelude(), &HostSurface::default())
}

/// The annotated prelude toplevel — its `Phrase::Define`s carry the
/// checker's schemes, so `builtins::register` installs each prelude
/// binding's scheme next to its value.
pub fn prelude_comp() -> &'static Arc<Toplevel> {
    prelude().comp()
}

/// The schemes harvested from the annotated prelude's `Bind` nodes.
pub fn prelude_schemes() -> &'static [(String, Scheme)] {
    prelude().schemes()
}

/// Visit every `Comp` in a tree, descending past the top-level spine into
/// thunk bodies, lambda bodies, branches, and pipeline stages — so the
/// nodes the annotation pass writes at any depth (a `Pipeline`'s wires, a
/// `Capture` node) are all reached.
pub fn walk_comp(comp: &Comp, visit: &mut impl FnMut(&Comp)) {
    use ral_core::ir::{CompKind, Val};
    visit(comp);
    let mut sub = |c: &Arc<Comp>| walk_comp(c, visit);
    match &comp.item {
        CompKind::Pipeline { stages, .. } => stages.iter().for_each(&mut sub),
        CompKind::Lam { body, .. } => sub(body),
        CompKind::Bind {
            comp: rhs, rest, ..
        } => {
            sub(rhs);
            sub(rest);
        }
        CompKind::App { head, .. } => sub(head),
        CompKind::If { then, else_, .. } => {
            sub(then);
            sub(else_);
        }
        CompKind::Case { arms, .. } => arms
            .iter()
            .for_each(|arm| sub(ral_core::test_access::case_arm_comp(arm))),
        CompKind::Rec { group, .. } => group.iter().for_each(|(_, m)| sub(m)),
        CompKind::Force(Val::Thunk(c))
        | CompKind::Return(Val::Thunk(c))
        | CompKind::Capture(c)
        | CompKind::Redirect { body: c, .. } => walk_comp(c, visit),
        CompKind::Try {
            body: a,
            handler: b,
        }
        | CompKind::Guard {
            body: a,
            cleanup: b,
        }
        | CompKind::Grant { caps: a, body: b } => {
            for v in [a, b] {
                walk_val(v, visit);
            }
        }
        CompKind::Within {
            opts,
            handlers,
            body,
        } => {
            walk_val(opts, visit);
            for arm in handlers.iter().flatten() {
                walk_val(&arm.value.item, visit);
            }
            walk_val(body, visit);
        }
        CompKind::Audit { body } => walk_val(body, visit),
        _ => {}
    }
}

fn walk_val(val: &ral_core::ir::Val, visit: &mut impl FnMut(&Comp)) {
    if let ral_core::ir::Val::Thunk(c) = val {
        walk_comp(c, visit);
    }
}
