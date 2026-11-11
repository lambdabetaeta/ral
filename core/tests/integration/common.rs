//! Scaffolding shared by core's integration tests: the prelude, baked once
//! per binary, recording sinks, and evaluation helpers.

#![allow(dead_code)] // not every test file uses every helper

use ral_core::io::terminal::TerminalState;
use ral_core::ir::Comp;
use ral_core::protocol::Run;
use ral_core::shell::boot::{BakedPrelude, HostSurface, boot_runtime};
use ral_core::shell::run::{RunReport, RunRequest};
use ral_core::shell::runtime::{Map, Observation, Settled, Shell, Value};
use ral_core::source::SourceOrigin;
use ral_core::{EventSink, SurfaceSink};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};

const HOST: &str = "a test host resolves its baseline";

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

/// One top-level run of `source` through the public `run` entry, settled.
/// Callers pick source that compiles: a static diagnostic is a test bug.
pub fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    shell
        .run(Run::foreground(source, SourceOrigin::named("<test>")))
        .expect_ran()
        .into_settled()
}

/// The checker's seed for a surface: the baked prelude's schemes over its Σ.
pub fn schemes_for(surface: &HostSurface) -> ral_core::SessionSchemes {
    ral_core::SessionSchemes::from_prelude(prelude().comp(), surface.manifest())
}

/// Visit every `Comp` in a tree, descending past the top-level spine into
/// thunk bodies, lambda bodies, branches, call arguments, and pipeline
/// stages — so checker-built nodes at any depth, such as `Capture`, are
/// all reached.
pub fn walk_comp(comp: &Comp, visit: &mut impl FnMut(&Comp)) {
    use ral_core::ir::CompKind;
    visit(comp);
    let mut sub = |c: &Arc<Comp>| walk_comp(c, visit);
    match &comp.item {
        CompKind::Pipeline { stages } => stages.iter().for_each(&mut sub),
        CompKind::Lam { body, .. } => sub(body),
        CompKind::Bind {
            comp: rhs, rest, ..
        }
        | CompKind::Seq { first: rhs, rest } => {
            sub(rhs);
            sub(rest);
        }
        CompKind::App { head, .. } => {
            sub(head);
            walk_args(comp, visit);
        }
        CompKind::Exec(_) => walk_args(comp, visit),
        CompKind::Prim(prim) => prim.operands().for_each(|value| walk_val(value, visit)),
        CompKind::If { then, else_, .. } => {
            walk_val(&then.item, visit);
            walk_val(&else_.item, visit);
        }
        CompKind::Case { arms, .. } => arms
            .iter()
            .for_each(|arm| walk_val(ral_core::test_access::case_arm_val(arm), visit)),
        CompKind::Rec { group, .. } => group.shape().iter().for_each(|(_, m)| sub(m)),
        CompKind::Force(value) | CompKind::Return(value) => walk_val(value, visit),
        CompKind::Capture(c) | CompKind::Redirect { body: c, .. } => {
            walk_comp(c, visit);
        }
        CompKind::Try { body, handler } => {
            for v in [body, handler] {
                walk_val(v, visit);
            }
        }
        CompKind::Guard {
            acquire,
            body,
            release,
        } => {
            for v in [acquire, body, release] {
                walk_val(v, visit);
            }
        }
        CompKind::Grant { caps: opts, body } | CompKind::Within { opts, body } => {
            for (_, v) in opts.shape() {
                walk_val(&v.item, visit);
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

/// Visit every `Comp` reachable from an annotated toplevel's phrases:
/// [`walk_comp`] from each phrase's own root.
pub fn walk_toplevel(top: &ral_core::ir::Toplevel, visit: &mut impl FnMut(&Comp)) {
    use ral_core::ir::Phrase;
    for phrase in &top.phrases {
        match &phrase.item {
            Phrase::Define { comp, .. } | Phrase::Run(comp) => walk_comp(comp, visit),
        }
    }
}

fn walk_args(comp: &Comp, visit: &mut impl FnMut(&Comp)) {
    for arg in ral_core::test_access::call_args(comp) {
        walk_val(arg, visit);
    }
}

fn walk_val(val: &ral_core::ir::Val, visit: &mut impl FnMut(&Comp)) {
    use ral_core::ir::Val;
    match val {
        Val::Thunk(node) => walk_comp(node.shape(), visit),
        Val::List(node) => {
            for value in node.shape() {
                walk_val(&value.item, visit);
            }
        }
        Val::Record(node) | Val::Map(node) => {
            for (_, value) in node.shape() {
                walk_val(&value.item, visit);
            }
        }
        Val::Variant { payload, .. } | Val::Protected { value: payload, .. } => {
            walk_val(payload, visit);
        }
        Val::Unit
        | Val::String(_)
        | Val::Int(_)
        | Val::Float(_)
        | Val::Bool(_)
        | Val::Variable(_) => {}
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
        .into_settled();
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

/// The `Command` facts among a stream of observation records, in order.
pub fn commands(observations: &[Value]) -> Vec<ral_core::fact::Command> {
    use ral_core::data::FOValue;
    use ral_core::data::datum::Datum as _;
    observations
        .iter()
        .map(|v| Observation::decode(&FOValue::try_from(v).unwrap()).unwrap())
        .filter_map(|obs| match obs.what {
            ral_core::fact::Observed::Command(command) => Some(command),
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

/// Evaluate `input` through the public run entry on an already-configured
/// `shell` — parse, elaborate, typecheck against the prelude schemes, run
/// the phrases.
pub fn run_on(shell: &mut Shell, input: &str) -> Settled<Value> {
    match shell.run(Run::foreground(input, SourceOrigin::named("<test>"))) {
        RunReport::Ran { ending, .. } => ending.into_settled(),
        RunReport::Static { diagnostics } => {
            panic!(
                "expected a valid runtime fixture:\n{}",
                diagnostics.render(shell.exit_hints()).0
            )
        }
    }
}

/// [`run_on`] against a fresh shell seeded with `path` on `PATH`.
pub fn eval_on_path(input: &str, path: &str) -> Settled<Value> {
    let mut shell = prelude_shell();
    shell.set_env_var("PATH", path).unwrap();
    run_on(&mut shell, input)
}

/// Evaluate independently of whichever tools happen to be installed on the
/// test host. Individual process tests supply an explicit path when needed.
pub fn eval(input: &str) -> Settled<Value> {
    #[cfg(unix)]
    let path = "/bin:/usr/bin".to_string();
    #[cfg(windows)]
    let path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .find(|dir| {
            ["cat.exe", "pwd.exe", "sleep.exe"]
                .iter()
                .all(|name| dir.join(name).is_file())
        })
        .unwrap_or_else(|| {
            panic!("eval needs cat.exe, pwd.exe, and sleep.exe in one PATH directory")
        })
        .to_string_lossy()
        .into_owned();

    eval_on_path(input, &path)
}

pub fn must_succeed(input: &str) -> Value {
    eval(input).unwrap_or_else(|e| panic!("should succeed: {input:?}\n  error: {e:?}"))
}
