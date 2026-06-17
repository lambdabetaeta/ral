#![allow(clippy::disallowed_methods)]

//! Fail-closed property of the evaluator's transport dispatch.
//!
//! When the caller has pushed a `Capabilities` frame that produces a
//! non-`None` `SandboxProjection`, the evaluator must run the body
//! under the OS sandbox.  If the confined-transport backend is
//! unavailable — `SANDBOX_SELF` was never pinned, or the platform has
//! no backend — the documented rule is **fail closed**: surface a
//! clear error and refuse to run.  Silently falling back to local
//! would weaken the caller's restriction.
//!
//! This file is its own integration-test target on purpose.  It
//! **deliberately does not import** `core/tests/common`, whose
//! `#[ctor::ctor]` calls `ral_core::sandbox::early_init` and so
//! registers `SANDBOX_SELF`.  Without that ctor running,
//! `confined_availability()` returns `Unavailable` for the reason the
//! evaluator then quotes back in its error message.
//!
//! We do not mock or inject `confined_availability` — the property
//! under test is the evaluator's behaviour in the natural state where
//! the backend reports itself unavailable.

use ral_core::evaluator;
use ral_core::sandbox::{ConfinedAvailability, confined_availability};
use ral_core::types::{Break, Capabilities, FsPolicy, Shell};

#[test]
fn confined_backend_is_unavailable_in_this_binary() {
    // Sanity-check the file's own assumption.  If a future change
    // causes some other code path to register `SANDBOX_SELF` (for
    // example by linking `common` into this target by accident), the
    // remaining test would silently start exercising the wrong branch
    // — fail loudly here instead.
    match confined_availability() {
        ConfinedAvailability::Unavailable(_) => {}
        ConfinedAvailability::Ready => panic!(
            "this test target must run without SANDBOX_SELF pinned; \
             if you imported `common` here, drop it — that ctor calls \
             early_init and breaks this file's whole premise"
        ),
    }
}

#[test]
fn eval_top_level_fails_closed_when_projection_active_but_backend_unavailable() {
    let mut shell = Shell::default();

    // Push a capability frame that produces a non-`None` projection.
    // Any fs policy is enough: the reducer's `saw_fs` branch turns it
    // into a `Restricted(...)` projection.
    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/".into()],
            write_prefixes: vec!["/".into()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };

    // The body is irrelevant: the boundary's `Unavailable` branch
    // produces the error before the body ever runs.  We compile a
    // trivial Comp so the call shape is well-formed.
    let comp = std::sync::Arc::new(ral_core::compile("return ok").expect("compile"));

    let result = shell.with_capabilities(caps, |s| evaluator::eval_top_level(&comp, s));

    let err = match result {
        Err(Break::Error(e)) => e,
        Err(other) => panic!("expected Break::Error, got {other:?}"),
        Ok(v) => panic!(
            "expected fail-closed error, got Ok({v:?}); \
             silent local fallback would have produced exactly this"
        ),
    };
    assert!(
        err.message.contains("sandbox") && err.message.contains("unavailable"),
        "error must clearly say the sandbox is unavailable; got: {:?}",
        err.message
    );
}

#[test]
fn eval_block_fails_closed_when_projection_active_but_backend_unavailable() {
    // The same property at the block boundary: a `grant`/`within`-shape
    // forced thunk inside an active projection must error rather than
    // fall back to local.  We construct a thunk via the evaluator and
    // route it through the public block entry.
    let mut shell = Shell::default();
    ral_core::builtins::register(&mut shell, prelude_comp());

    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/".into()],
            write_prefixes: vec!["/".into()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };

    // Evaluate a thunk literal so we have a `Value::Thunk` to apply.
    // `apply` routes a `Value::Thunk` through the trampoline into the
    // block path — the same route `!{ … }` and `grant` take internally.
    let thunk_comp =
        std::sync::Arc::new(ral_core::compile("{ return ok }").expect("compile thunk"));
    let thunk_value = ral_core::evaluate(&thunk_comp, &mut shell).expect("thunk evaluates");

    let result = shell.with_capabilities(caps, |s| evaluator::apply(thunk_value, vec![], s));

    let err = match result {
        Err(Break::Error(e)) => e,
        Err(other) => panic!("expected Break::Error, got {other:?}"),
        Ok(v) => panic!("expected fail-closed error at the block boundary, got Ok({v:?})"),
    };
    assert!(
        err.message.contains("sandbox") && err.message.contains("unavailable"),
        "block boundary error must mention sandbox unavailability; got: {:?}",
        err.message
    );
}

/// The fail-closed top-level error must install `last_status = 1`
/// onto the parent shell's mobile, not leave the previous turn's value
/// in place.  This pins the contract of `finish_top_level`'s
/// "install the boundary's chosen status" step: the parent's `$?`
/// reports the transport refusal, not whatever happened before.
#[test]
fn eval_top_level_fail_closed_installs_status_1_into_mobile() {
    let mut shell = Shell::default();

    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/".into()],
            write_prefixes: vec!["/".into()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };

    // Plain `return ok`: the boundary refuses before the body runs, so
    // we don't need the prelude registered to bake this comp.
    let comp = std::sync::Arc::new(ral_core::compile("return ok").expect("compile"));

    let result = shell.with_capabilities(caps, |s| evaluator::eval_top_level(&comp, s));
    assert!(
        result.is_err(),
        "expected fail-closed error from active projection + unavailable backend, got Ok"
    );
    assert_eq!(
        shell.mobile.control.last_status, 1,
        "fail-closed top-level error must install last_status = 1 into the parent's mobile"
    );
}

/// A spawned concurrent block is a thunk evaluated on a worker thread
/// via `with_scope(eval_comp(body))`.  With a projection active and the
/// confined-eval backend unavailable, every nested forced block inside
/// the body (the literal `to-string 'bad' > '…'` here is parsed as one
/// such block) must fail closed — exactly the same way a forced
/// `grant`/`within` block does at top level — instead of running
/// locally with the parent's full authority.  `await` then surfaces
/// that failure rather than the user's intended payload, and the side
/// effect inside the body must not have happened.
#[test]
fn spawn_fails_closed_when_projection_active_and_backend_unavailable() {
    use std::io::Write;
    let mut shell = Shell::default();
    ral_core::builtins::register(&mut shell, prelude_comp());

    // A target path we can verify post-hoc.  If fail-closed is correct
    // the file is never created; if the body silently ran with host
    // authority, it appears.
    let dir = std::env::temp_dir().join("ral-fail-closed-spawn-tests");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let target = dir.join(format!(
        "spawn-leak-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // Best-effort pre-cleanup — `assert` after the boundary call is what
    // pins the property either way.
    let _ = std::fs::remove_file(&target);

    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/".into()],
            write_prefixes: vec!["/".into()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };

    let target_path = target.display().to_string();
    let source = format!(
        "let h = !{{spawn {{ to-string 'bad' > '{target_path}' }}}}\n\
         let r = await $h\n\
         return $r[value]"
    );

    let comp = match ral_core::compile_and_typecheck(&source, shell.session_schemes()) {
        ral_core::CompileOutcome::Compiled(c) => std::sync::Arc::new(c),
        ral_core::CompileOutcome::Parse(e) => panic!("parse: {e}"),
        ral_core::CompileOutcome::Types(errs) => {
            let msgs: Vec<_> = errs.iter().map(|e| e.kind.render_message()).collect();
            panic!("type: {}", msgs.join("; "));
        }
    };
    let result = shell.with_capabilities(caps, |s| evaluator::eval_top_level(&comp, s));

    let err = match result {
        Err(Break::Error(e)) => e,
        Err(other) => panic!("expected fail-closed error surfacing through await, got {other:?}"),
        Ok(v) => panic!(
            "expected fail-closed error, got Ok({v:?}); the spawn body \
             silently ran with host authority"
        ),
    };

    // Whether the error message is the boundary's own
    // ("sandbox confinement unavailable") or `await`'s
    // ("await: spawned thread panicked") depends on whether the worker
    // already reached the boundary refusal before we joined.  Either
    // way the body must not have succeeded — the side effect is the
    // load-bearing observation.
    assert!(
        !std::path::Path::new(&target_path).exists(),
        "fail-closed must prevent the body's side effect; file exists at {target_path}: {:?}",
        err.message
    );
    let _ = std::fs::remove_file(&target);
    drop(std::io::stderr().lock().flush());
}

/// `par` is prelude code over `spawn`, so the fail-closed property
/// extends through it: when projection is active and backend
/// unavailable, `par` must surface an error rather than silently
/// running its tasks with host authority.
#[test]
fn par_fails_closed_by_spawn_when_projection_active_and_backend_unavailable() {
    let mut shell = Shell::default();
    ral_core::builtins::register(&mut shell, prelude_comp());

    let dir = std::env::temp_dir().join("ral-fail-closed-par-tests");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let target = dir.join(format!(
        "par-leak-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&target);

    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/".into()],
            write_prefixes: vec!["/".into()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };

    let target_path = target.display().to_string();
    let source = format!("!{{par {{ |x| to-string 'bad' > '{target_path}'; return $x }} [1] 1}}");

    let comp = match ral_core::compile_and_typecheck(&source, shell.session_schemes()) {
        ral_core::CompileOutcome::Compiled(c) => std::sync::Arc::new(c),
        ral_core::CompileOutcome::Parse(e) => panic!("parse: {e}"),
        ral_core::CompileOutcome::Types(errs) => {
            let msgs: Vec<_> = errs.iter().map(|e| e.kind.render_message()).collect();
            panic!("type: {}", msgs.join("; "));
        }
    };
    let result = shell.with_capabilities(caps, |s| evaluator::eval_top_level(&comp, s));

    assert!(
        result.is_err(),
        "expected par to surface fail-closed error from spawn, got Ok"
    );
    assert!(
        !std::path::Path::new(&target_path).exists(),
        "fail-closed must prevent par's tasks from writing; file exists at {target_path}"
    );
    let _ = std::fs::remove_file(&target);
}

/// The annotated prelude comp paired with the schemes harvested off its
/// top-level `Bind` nodes — what one `bake_prelude` pass yields.
type BakedPrelude = (
    std::sync::Arc<ral_core::Comp>,
    Vec<(String, ral_core::Scheme)>,
);

/// The prelude checked once.  Duplicates `core/tests/common::baked`
/// deliberately — we must not link `common` into this target (see
/// file-level comment), as its ctor would pin `SANDBOX_SELF`.
fn baked() -> &'static BakedPrelude {
    use std::sync::{Arc, OnceLock};
    static B: OnceLock<BakedPrelude> = OnceLock::new();
    B.get_or_init(|| {
        let src = include_str!("../src/prelude.ral");
        let ast = ral_core::parse(src).expect("prelude parse");
        let comp = ral_core::elaborate(&ast, Default::default());
        let (annotated, schemes) = ral_core::bake_prelude(&comp);
        (Arc::new(annotated), schemes)
    })
}

/// The annotated prelude comp — its `Bind` nodes carry the checker's
/// schemes, so `builtins::register` installs each prelude binding's scheme.
fn prelude_comp() -> &'static std::sync::Arc<ral_core::Comp> {
    &baked().0
}
