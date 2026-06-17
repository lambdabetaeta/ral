//! Shared scaffolding for `core/tests/*.rs`: a once-elaborated prelude
//! `Comp` and the schemes baked from it.  Both are memoised so the
//! prelude is parsed and elaborated exactly once per test binary.
//!
//! Also installs a pre-main constructor that mimics the ral binary's
//! response to the `--ral-pipeline-stage-helper` sentinel (dispatched
//! at line 22 below).  Pipelines and capture-active standalone
//! invocations of bundled coreutils tools re-exec `current_exe()` with
//! that flag and run one stage via the pipeline helper protocol —
//! in a unit test, `current_exe()` is the test binary itself, so
//! without the constructor the re-exec would land in the test
//! framework instead of the helper, and bundled `test`/`wc`/`stat`
//! would never run.

#![allow(dead_code)] // not every test file uses every helper

use ral_core::host::BakedPrelude;
use ral_core::{Comp, Scheme};
use std::sync::{Arc, OnceLock};

#[ctor::ctor(unsafe)]
fn init_test_binary() {
    #[cfg(unix)]
    ral_core::builtins::uutils::init_signal_dispositions();
    // Two modes get dispatched ahead of the Rust test harness:
    //
    //   1. `--ral-pipeline-stage-helper` and friends — the pipeline
    //      re-execs `current_exe()` (which is *this* test binary) to
    //      serve one stage; without the trampoline that re-exec would
    //      land in the test framework instead of the helper.
    //   2. `--sandbox-projection ...` and `--internal-sandbox-block` —
    //      `grant { … }` re-execs the same way to install the OS
    //      sandbox.  `ral_core::sandbox::early_init` consumes the
    //      flag, enters the sandbox, and (for the internal-block
    //      mode) serves one IPC request.  Letting libtest see those
    //      flags would crash with "unknown argument".
    if let Some(code) = ral_core::try_run_pipeline_stage_helper() {
        std::process::exit(code as i32);
    }
    // `early_init` consumes the projection flag, pins `SANDBOX_SELF`, and
    // dispatches the internal-sandbox-block mode; the shared ctor wrapper
    // self-exits when this process is the re-exec child.
    ral_core::sandbox::early_init_or_exit_for_test_ctor();
}

/// The prelude baked once at runtime (test binaries have no build-time
/// blob), memoised for the accessors below and for `boot_shell`.
pub fn prelude() -> &'static BakedPrelude {
    static B: OnceLock<BakedPrelude> = OnceLock::new();
    B.get_or_init(BakedPrelude::bake_runtime)
}

/// The annotated prelude comp — its `Bind` nodes carry the checker's
/// schemes, so `builtins::register` installs each prelude binding's scheme
/// next to its value.
pub fn prelude_comp() -> &'static Arc<Comp> {
    prelude().comp()
}

/// The schemes harvested from the annotated prelude's `Bind` nodes.
pub fn prelude_schemes() -> &'static [(String, Scheme)] {
    prelude().schemes()
}

/// Visit every `Comp` in a tree, descending past the top-level spine into
/// thunk bodies, lambda bodies, branches, and pipeline stages — so the
/// nodes the annotation pass writes at any depth (a `Pipeline`'s wires, a
/// `Bind`'s RHS output mode) are all reached.
pub fn walk_comp(comp: &Comp, visit: &mut impl FnMut(&Comp)) {
    use ral_core::ir::{CompKind, Val};
    visit(comp);
    let mut sub = |c: &Arc<Comp>| walk_comp(c, visit);
    match &comp.item {
        CompKind::Seq(parts) | CompKind::Chain(parts) => parts.iter().for_each(&mut sub),
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
        CompKind::Force(Val::Thunk(c)) | CompKind::Return(Val::Thunk(c)) => walk_comp(c, visit),
        _ => {}
    }
}
