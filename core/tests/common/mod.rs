//! Shared scaffolding for `core/tests/*.rs`: a once-elaborated prelude
//! `Comp` and the schemes baked from it.  Both are memoised so the
//! prelude is parsed and elaborated exactly once per test binary.
//!
//! Also installs a pre-main constructor that serves the re-exec roles
//! (see [`ral_core::invocation::serve_process_before_main`]):
//! the pipeline anchor and capture-active standalone invocations of bundled
//! coreutils tools re-exec `current_exe()` — the test binary itself — so
//! without the constructor the re-exec would land in the test framework
//! instead of the helper, and bundled `test`/`wc`/`stat` would never run.

#![allow(dead_code)] // not every test file uses every helper

use ral_core::io::terminal::TerminalState;
use ral_core::protocol::Run;
use ral_core::shell::boot::{BakedPrelude, HostSurface, boot_runtime, boot_shell};
use ral_core::shell::run::RunRequest;
use ral_core::shell::runtime::{Map, Observation, Settled, Shell, Value};
use ral_core::source::SourceOrigin;
use ral_core::{EventSink, SurfaceSink};
use ral_core::{Scheme, ir::Comp};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};

const HOST: &str = "a test host resolves its baseline";

#[ctor::ctor(unsafe)]
fn init_test_binary() {
    if let Some(code) = ral_core::invocation::serve_process_before_main(&[]) {
        std::process::exit(i32::from(code));
    }
}

/// The prelude baked once at runtime (test binaries have no build-time
/// blob), memoised for the accessors below and for `boot_shell`.
pub fn prelude() -> &'static BakedPrelude {
    BakedPrelude::runtime()
}

/// A root shell over core's surface and the prelude: no env vars.
pub fn prelude_shell() -> Shell {
    Shell::root(
        boot_runtime(TerminalState::default(), prelude(), &HostSurface::default())
            .expect(HOST)
            .into(),
        ral_core::test_helper::process_cwd(),
    )
}

/// A shell booted as every front end boots one: core's builtin surface,
/// the default env vars, the prelude registered with its schemes.
pub fn fresh_shell() -> Shell {
    boot_shell(
        TerminalState::default(),
        prelude(),
        &HostSurface::default(),
        ral_core::test_helper::process_cwd(),
    )
    .expect(HOST)
}

/// `surface`'s Σ, as the checker sees it.
pub fn manifest(surface: &HostSurface) -> ral_core::typecheck::Manifest {
    surface.manifest()
}

/// The schemes on the baked prelude's `Phrase::Define`s, in phrase order.
pub fn prelude_schemes() -> Vec<(String, Arc<Scheme>)> {
    prelude()
        .comp()
        .phrases
        .iter()
        .flat_map(|phrase| match &phrase.item {
            ral_core::ir::Phrase::Define { schemes, .. } => schemes.clone(),
            ral_core::ir::Phrase::Run(_) => Vec::new(),
        })
        .collect()
}

/// The checker's seed for a surface: the baked prelude's schemes over its Σ.
pub fn schemes_for(surface: &HostSurface) -> ral_core::SessionSchemes {
    ral_core::SessionSchemes::from_prelude(prelude().comp(), manifest(surface))
}

/// Visit every `Comp` in a tree, descending past the top-level spine into
/// thunk bodies, lambda bodies, branches, call arguments, and pipeline
/// stages — so the nodes the annotation pass writes at any depth (a
/// `Pipeline`'s wires, a `Capture` node) are all reached.
pub fn walk_comp(comp: &Comp, visit: &mut impl FnMut(&Comp)) {
    use ral_core::ir::{CompKind, Val};
    visit(comp);
    let mut sub = |c: &Arc<Comp>| walk_comp(c, visit);
    match &comp.item {
        CompKind::Pipeline { stages } => stages.iter().for_each(&mut sub),
        CompKind::Lam { body, .. } => sub(body),
        CompKind::Bind {
            comp: rhs, rest, ..
        } => {
            sub(rhs);
            sub(rest);
        }
        CompKind::App { head, .. } => {
            sub(head);
            walk_args(comp, visit);
        }
        CompKind::Exec(_) => walk_args(comp, visit),
        CompKind::If { then, else_, .. } => {
            walk_val(&then.item, visit);
            walk_val(&else_.item, visit);
        }
        CompKind::Case { arms, .. } => arms
            .iter()
            .for_each(|arm| walk_val(ral_core::test_access::case_arm_val(arm), visit)),
        CompKind::Rec { group, .. } => group.shape().iter().for_each(|(_, m)| sub(m)),
        CompKind::Force(Val::Thunk(node)) | CompKind::Return(Val::Thunk(node)) => {
            walk_comp(node.shape(), visit);
        }
        CompKind::Capture(c) | CompKind::Redirect { body: c, .. } => walk_comp(c, visit),
        CompKind::Try {
            body: a,
            handler: b,
        }
        | CompKind::Guard {
            body: a,
            cleanup: b,
        } => {
            for v in [a, b] {
                walk_val(v, visit);
            }
        }
        CompKind::Grant { caps, body } => {
            for (_, v) in caps {
                walk_val(&v.item, visit);
            }
            walk_val(body, visit);
        }
        CompKind::Within {
            opts,
            handlers,
            body,
        } => {
            for (_, v) in opts {
                walk_val(&v.item, visit);
            }
            for arm in handlers.iter().flatten() {
                walk_val(&arm.value.item, visit);
            }
            walk_val(body, visit);
        }
        CompKind::Audit { body } | CompKind::Gather { body } => walk_val(body, visit),
        CompKind::Spawn { title, body } => {
            if let Some(t) = title {
                walk_val(&t.item, visit);
            }
            walk_val(body, visit);
        }
        _ => {}
    }
}

fn walk_args(comp: &Comp, visit: &mut impl FnMut(&Comp)) {
    for arg in ral_core::test_access::call_args(comp) {
        walk_val(arg, visit);
    }
}

fn walk_val(val: &ral_core::ir::Val, visit: &mut impl FnMut(&Comp)) {
    if let ral_core::ir::Val::Thunk(node) = val {
        walk_comp(node.shape(), visit);
    }
}

/// A sink that records every surfaced observation as the record a trail
/// carries.
pub struct Recorder(pub Arc<Mutex<Vec<Value>>>);

impl EventSink for Recorder {
    fn emit(&self, ev: &ral_core::data::FOValue) {
        if let Some(obs) = Observation::from_surface(ev) {
            self.0.lock().unwrap().push(Value::from_datum(obs));
        }
    }
}

/// Run `source` on `shell`, with a recording surface sink installed or not,
/// returning the settled value and whatever reached the sink.
pub fn run_recorded(shell: &mut Shell, source: &str, sink: bool) -> (Settled<Value>, Vec<Value>) {
    let log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder: SurfaceSink = Arc::new(Recorder(Arc::clone(&log)));
    let result = shell
        .run(RunRequest {
            surface: sink.then_some(recorder),
            ..RunRequest::from(Run::foreground(source, SourceOrigin::named("<test>")))
        })
        .expect_ran()
        .into_result();
    let events = log.lock().unwrap().clone();
    (result, events)
}

/// The `label` facts among a stream of observation records, in order, each
/// with the site its observation carries.
pub fn facts(observations: &[Value], label: &str) -> Vec<(Map, Option<Value>)> {
    observations
        .iter()
        .filter_map(|v| match v {
            Value::Map(m) => match m.get("what").as_deref() {
                Some(Value::Variant {
                    label: kind,
                    payload: Some(fact),
                }) if kind.as_ref() == label => match fact.as_ref() {
                    Value::Map(fact) => Some((fact.clone(), m.get("site").map(Cow::into_owned))),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The `fields` of a check fact's payload, `{resource, decision, fields}`.
pub fn check_fields(payload: &Map) -> Map {
    match payload.get("fields").as_deref() {
        Some(Value::Map(fields)) => fields.clone(),
        other => panic!("a check fact carries a fields map, got {other:?}"),
    }
}

/// `body` under a grant that runs `/bin/cat` alone.
pub fn under_grant(body: &str) -> String {
    format!("grant [exec: [allow: [`absolute /bin/cat]]] {{ {body} }}")
}
