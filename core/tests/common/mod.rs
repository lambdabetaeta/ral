//! Shared scaffolding for `core/tests/*.rs`: a once-elaborated prelude
//! `Comp` and the schemes baked from it.  Both are memoised so the
//! prelude is parsed and elaborated exactly once per test binary.
//!
//! Also installs a pre-main constructor that serves the shared re-exec
//! stages (see [`ral_core::test_helper::run_pre_main_reexec_stages`]):
//! pipelines and capture-active standalone invocations of bundled
//! coreutils tools re-exec `current_exe()` — the test binary itself — so
//! without the constructor the re-exec would land in the test framework
//! instead of the helper, and bundled `test`/`wc`/`stat` would never run.

#![allow(dead_code)] // not every test file uses every helper

use ral_core::boot::BakedPrelude;
use ral_core::{Scheme, ir::Comp, ir::Toplevel};
use std::sync::{Arc, OnceLock};

#[ctor::ctor(unsafe)]
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
        CompKind::Chain(parts) => parts.iter().for_each(&mut sub),
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
        CompKind::Case { arms, .. } => arms.iter().for_each(|arm| sub(arm.body.comp())),
        CompKind::Rec { group, .. } => group.iter().for_each(|(_, m)| sub(m)),
        CompKind::Source { path, rest } => {
            sub(path);
            sub(rest);
        }
        CompKind::Force(Val::Thunk(c))
        | CompKind::Return(Val::Thunk(c))
        | CompKind::Capture(c)
        | CompKind::Decode(c)
        | CompKind::Redirect { body: c, .. } => walk_comp(c, visit),
        _ => {}
    }
}
