use crate::authority::grant::freeze::FreezeCtx;
use crate::authority::policy::GrantLayer;
use crate::data::FOValue;
use crate::data::json::{JsonDocument, JsonFormat, decode_structural, serialize_structural};
use crate::fact::{
    Count, DataStep, DecodeCause, DecodeFailure, FrameEncodeFailure, JsonEncodeCause, ProtocolData,
    ProtocolFailure, ProtocolOperation, Reason, ReservedVar,
};
use crate::ir::{Comp, CompKind, Module, Phrase, Toplevel, Val};
use crate::marshal::{self, Marshalled, wire};
use crate::shell::boot::BakedPrelude;
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::{
    BindingLease, Context, DefaultPolicy, Env, FrameKind, HandlerEntry, HandlerFrame, HookName,
    HookSig, Image, RawContext, Runtime, Shell, Terms, ValClos, View,
};
use crate::source::{SourceFile, SourceOrigin, Spanned};
use crate::sync::LockExt as _;
use serde_json::json;
use std::sync::Arc;

fn runtime() -> Runtime {
    crate::shell::boot::boot_runtime(
        crate::io::terminal::TerminalState::default(),
        BakedPrelude::runtime(),
        &crate::shell::boot::HostSurface::default(),
    )
    .unwrap()
}

fn no_base(_: &str, _: &FreezeCtx) -> Result<GrantLayer, crate::Error> {
    panic!("only a Base grant consults the narrower")
}

fn run(shell: &mut Shell, source: impl Into<String>) -> ValClos {
    shell
        .run(crate::protocol::Run::captured(
            source.into(),
            SourceOrigin::named("<marshal-session>"),
        ))
        .expect_ran()
        .into_settled()
        .unwrap()
}

fn parent() -> Shell {
    let mut shell = crate::shell::boot::boot_shell(
        crate::io::terminal::TerminalState::default(),
        BakedPrelude::runtime(),
        &crate::shell::boot::HostSurface::default(),
        crate::test_helper::process_cwd(),
    )
    .unwrap();
    run(
        &mut shell,
        "let kept = 7; let blk = { return $kept }; let id = { |x| return $x }",
    );
    shell
        .context
        .set_env_var("MARSHAL_CONTEXT", "value")
        .unwrap();
    shell.context.args = vec!["first".into(), String::new()];
    shell.session.stack_limit = 12_345;
    shell.session.child_limits = crate::process::ChildLimits {
        open_files: Some(128),
        core_bytes: Some(0),
        stack_bytes: Some(8 << 20),
    };
    shell.set_exit_hints(crate::shell::runtime::ExitHints::from_text(
        "false 1 hinted\n",
    ));
    shell.set_scratch("/the/session/scratch");
    shell.arm_binding_lease(BindingLease {
        idle_calls: 3,
        large_binding_bytes: 1 << 20,
    });
    shell.arm_worker_retention(5);
    shell
        .fixings()
        .fix(crate::ty::UnitId::fresh(), 0, crate::ty::Side::Number, "/n")
        .unwrap();
    let block = shell.env.get("blk").unwrap().clone();
    shell.context.handlers.push(
        vec![
            HandlerEntry::ral_per_name("named".into(), block.clone()),
            HandlerEntry::ral_per_name("handler".into(), block.clone()),
        ],
        Some(block.clone()),
    );
    shell
        .context
        .handlers
        .push_handler(vec![HandlerEntry::ral_per_name(
            "persistent".into(),
            block.clone(),
        )]);
    shell
        .register_hook(
            HookName::session("prompt"),
            block,
            HookSig::Block,
            DefaultPolicy::denied(),
        )
        .unwrap();
    shell.install_script_context(&SourceOrigin::named("<extra>"), "echo extra\n");
    shell
}

fn image(shell: &Shell) -> Image {
    Image {
        env: shell.env.clone(),
        context: shell
            .context
            .with_handlers(shell.context.handlers.iter().cloned().collect()),
        sources: shell.session.sources.clone(),
        terms: Terms {
            stack_limit: shell.session.stack_limit,
            child_limits: shell.session.child_limits,
            exit_hints: (*shell.session.exit_hints).clone(),
            scratch: shell.session.scratch.clone(),
            lease: shell.local.bindings.lease(),
            retention: shell.local.workers.retention(),
        },
        fixings: shell.fixings().snapshot(),
    }
}

fn assert_env(sent: &Env, received: &Env) {
    assert_eq!(
        sent.names().collect::<std::collections::BTreeSet<_>>(),
        received.names().collect::<std::collections::BTreeSet<_>>()
    );
    for (name, binding) in sent.iter() {
        let back = received.binding(name).unwrap();
        assert_eq!(binding.scheme, back.scheme, "scheme of {name}");
        assert_value(&binding.value, &back.value);
    }
}

fn assert_value(sent: &ValClos, received: &ValClos) {
    match (sent.view(), received.view()) {
        (View::Thunk { comp: a, env: ae }, View::Thunk { comp: b, env: be }) => {
            assert_eq!(a, b);
            assert_env(ae, be);
        }
        _ => assert_eq!(sent, received),
    }
}

fn assert_frames(sent: &[HandlerFrame], received: &[HandlerFrame]) {
    assert_eq!(sent.len(), received.len());
    for (a, b) in sent.iter().zip(received) {
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.entries.len(), b.entries.len());
        for (a, b) in a.entries.iter().zip(&b.entries) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.arity, b.arity);
            assert_value(&a.thunk, &b.thunk);
        }
        match (&a.catch_all, &b.catch_all) {
            (Some(a), Some(b)) => assert_value(a, b),
            (None, None) => {}
            _ => panic!("catch-all presence changed"),
        }
    }
}

fn cwd_grant() -> SpawnGrant {
    SpawnGrant::Restrict(
        FOValue::try_from(&ValClos::map(vec![(
            "fs".into(),
            ValClos::map(vec![(
                "read".into(),
                ValClos::list(vec![ValClos::variant("cwd", Some(ValClos::string(".")))]),
            )]),
        )]))
        .unwrap(),
    )
}

#[test]
fn the_whole_image_and_each_unfrozen_spawn_grant_round_trip() {
    let parent = parent();
    let sent = image(&parent);
    assert_eq!(
        sent.context
            .handlers
            .iter()
            .map(|frame| frame.kind)
            .collect::<Vec<_>>(),
        [FrameKind::Within, FrameKind::Handler]
    );
    assert!(
        sent.env
            .binding("id")
            .unwrap()
            .scheme
            .as_ref()
            .unwrap()
            .is_poly()
    );
    for grant in [
        SpawnGrant::Inherit,
        SpawnGrant::Base("not-resolved-by-the-codec".into()),
        cwd_grant(),
    ] {
        let bytes = marshal::write(&sent, &grant).unwrap();
        let (back, back_grant) = marshal::read(&runtime(), &bytes).unwrap();
        assert_env(&sent.env, &back.env);
        assert_frames(&sent.context.handlers, &back.context.handlers);
        assert_eq!(
            serde_json::to_value(sent.context.with_handlers(())).unwrap(),
            serde_json::to_value(back.context.with_handlers(())).unwrap()
        );
        assert_eq!(sent.sources, back.sources);
        assert_eq!(sent.terms, back.terms);
        assert_eq!(
            *sent.fixings.0.lock_ignore_poison(),
            *back.fixings.0.lock_ignore_poison()
        );
        assert_eq!(
            grant, back_grant,
            "the codec does not freeze or resolve grants"
        );
        let (comp, _) = back.env.get("blk").unwrap().block().unwrap();
        let span = comp.span.expect("the closure keeps its source span");
        assert_eq!(back.sources.get(span.file), sent.sources.get(span.file));
        assert!(back.sources.get(span.file).is_some());
    }
}

#[test]
fn a_child_inherits_terms_but_no_host_local_authority() {
    let parent = parent();
    assert!(parent.session.hooks.is_some());
    let bytes = parent.marshal(SpawnGrant::Inherit).unwrap();
    let child = Shell::child_of(runtime().into(), &bytes, no_base).unwrap();
    assert!(child.session.hooks.is_none());
    assert!(child.session.terminal_lease.is_none());
    assert!(matches!(child.io.stdin, crate::io::Source::Empty));
    assert!(child.session.exit_hints.lookup("false", 1).is_some());
    assert_eq!(child.scratch(), Some("/the/session/scratch"));
    assert!(child.local.bindings.armed());
    assert_eq!(child.local.bindings.lease(), parent.local.bindings.lease());
    assert_eq!(child.local.bindings.leased_count(), 0);
    for (name, _) in parent.env.all_bindings(&parent.sig) {
        assert!(
            child.local.bindings.exempts(&name),
            "inherited {name} is baseline"
        );
    }
    assert_eq!(child.local.workers.retention(), Some(5));
    assert_eq!(child.session.stack_limit, 12_345);
    assert_eq!(child.session.child_limits, parent.session.child_limits);
    assert_eq!(child.session.sources, parent.session.sources);
    assert_eq!(child.context.grants.len(), parent.context.grants.len() + 1);
    let (unit, var) = *parent
        .fixings()
        .0
        .lock_ignore_poison()
        .keys()
        .next()
        .unwrap();
    assert!(
        child
            .fixings()
            .fix(unit, var, crate::ty::Side::Text, "/t")
            .is_err()
    );
    let fresh = crate::ty::UnitId::fresh();
    child
        .fixings()
        .fix(fresh, 0, crate::ty::Side::Text, "/t")
        .unwrap();
    assert!(
        !parent
            .fixings()
            .0
            .lock_ignore_poison()
            .contains_key(&(fresh, 0))
    );
}

#[test]
fn a_base_grant_runs_the_narrower_with_the_childs_anchors() {
    #[allow(clippy::unnecessary_wraps, reason = "GrantNarrower signature")]
    fn narrow(name: &str, ctx: &FreezeCtx) -> Result<GrantLayer, crate::Error> {
        assert_eq!(name, "confined");
        assert_eq!(ctx.cwd(), &crate::test_helper::process_cwd());
        assert_eq!(ctx.scratch(), Some("/the/session/scratch"));
        Ok(GrantLayer {
            net: Some(crate::authority::policy::Net::Offline),
            ..GrantLayer::root()
        })
    }
    let parent = parent();
    let bytes = parent.marshal(SpawnGrant::Base("confined".into())).unwrap();
    let child = Shell::child_of(runtime().into(), &bytes, narrow).unwrap();
    assert_eq!(child.context.grants.len(), parent.context.grants.len() + 1);
    assert_eq!(
        child.context.grants.layers().last().unwrap().net,
        Some(crate::authority::policy::Net::Offline)
    );
}

#[test]
fn a_restriction_freezes_against_the_childs_inherited_cwd() {
    use crate::authority::policy::{Baseline, FsOp};
    let tmp = tempfile::tempdir().unwrap();
    let dir = crate::test_helper::real(tmp.path()).as_path().to_path_buf();
    let cwd = dir.join("child");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("inside"), "").unwrap();
    std::fs::write(dir.join("outside"), "").unwrap();
    let mut parent = parent();
    parent.seed_cwd(crate::path::CwdPath::new(cwd.clone()).unwrap());
    let bytes = parent.marshal(cwd_grant()).unwrap();
    let child = Shell::child_of(runtime().into(), &bytes, no_base).unwrap();
    assert_eq!(child.context.cwd().as_path(), cwd);
    assert_eq!(child.context.grants.len(), parent.context.grants.len() + 1);
    let reads = |path: std::path::PathBuf| {
        child.context.grants.admits_fs(
            &Baseline::default(),
            FsOp::Read,
            &crate::test_helper::trace(path),
        )
    };
    assert!(reads(cwd.join("inside")));
    assert!(!reads(dir.join("outside")));
}

#[test]
fn a_linked_module_keeps_its_checked_top_and_origin_without_loading_again() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("linked.ral");
    std::fs::write(&file, "let n = 7").unwrap();
    let mut parent = parent();
    run(
        &mut parent,
        format!(
            "let linked = {{ use {} }}",
            crate::syntax::quote_word(file.to_str().unwrap())
        ),
    );
    let bytes = parent.marshal(SpawnGrant::Inherit).unwrap();
    std::fs::write(&file, "fail 'must not read this source'").unwrap();
    let mut child = Shell::child_of(runtime().into(), &bytes, no_base).unwrap();
    let (before, _) = parent.env.get("linked").unwrap().block().unwrap();
    let (after, _) = child.env.get("linked").unwrap().block().unwrap();
    assert_eq!(
        before, after,
        "Use carries its checked module, including origin"
    );
    assert_eq!(
        run(&mut child, "let m = linked; return $m[n]"),
        ValClos::int(7)
    );
}

#[test]
fn nested_linked_modules_round_trip_as_ir() {
    let inner = Arc::new(Module {
        top: Toplevel {
            phrases: vec![Spanned::synthetic(Phrase::Run(Arc::new(
                Spanned::synthetic(CompKind::Return(Val::Int(42))),
            )))],
            admits: Vec::new(),
        },
        origin: SourceOrigin::File(SourceFile::new("/marshal/inner.ral").unwrap()),
    });
    let outer = Arc::new(Module {
        top: Toplevel {
            phrases: vec![Spanned::synthetic(Phrase::Run(Arc::new(
                Spanned::synthetic(CompKind::Use(inner)),
            )))],
            admits: Vec::new(),
        },
        origin: SourceOrigin::File(SourceFile::new("/marshal/outer.ral").unwrap()),
    });
    let comp = Arc::new(Spanned::synthetic(CompKind::Lam {
        param: "unused".into(),
        body: Arc::new(Spanned::synthetic(CompKind::Use(outer))),
    }));
    let mut parent = parent();
    crate::test_access::bind_untyped(
        &mut parent,
        "nested",
        ValClos::thunk(comp.clone(), &Env::EMPTY),
    );
    let (back, _) =
        marshal::read(&runtime(), &parent.marshal(SpawnGrant::Inherit).unwrap()).unwrap();
    assert_eq!(back.env.get("nested").unwrap().block().unwrap().0, &comp);
}

fn walk_comp(comp: &Comp, visit: &mut impl FnMut(&Comp)) {
    visit(comp);
    match &comp.item {
        CompKind::Lam { body, .. } | CompKind::Capture(body) => walk_comp(body, visit),
        CompKind::Bind { comp, rest, .. } | CompKind::Seq { first: comp, rest } => {
            walk_comp(comp, visit);
            walk_comp(rest, visit);
        }
        CompKind::Pipeline { stages } => {
            for stage in stages {
                walk_comp(stage, visit);
            }
        }
        CompKind::App { head, .. } => walk_comp(head, visit),
        CompKind::Force(Val::Thunk(node)) | CompKind::Return(Val::Thunk(node)) => {
            walk_comp(node.shape(), visit);
        }
        CompKind::Prim(prim) => {
            for value in prim.operands() {
                if let Val::Thunk(node) = value {
                    walk_comp(node.shape(), visit);
                }
            }
        }
        _ => {}
    }
}

#[test]
fn checker_written_capture_and_boundary_sites_keep_their_unit() {
    for source in [
        "let f = { |x| let y = /bin/echo $x; /bin/cat | /bin/cat }",
        "let f = { |p| let a = from-json < $p; let b = from-json < $p; return $[$a[x] < $b[x]] }",
    ] {
        let mut parent = parent();
        run(&mut parent, source);
        let (before, _) = parent.env.get("f").unwrap().block().unwrap();
        let bytes = parent.marshal(SpawnGrant::Inherit).unwrap();
        let (back, _) = marshal::read(&runtime(), &bytes).unwrap();
        let (after, _) = back.env.get("f").unwrap().block().unwrap();
        assert_eq!(before, after);
        let mut capture = false;
        let mut sites = Vec::new();
        walk_comp(after, &mut |comp| match &comp.item {
            CompKind::Capture(_) => capture = true,
            CompKind::Exec(exec) => {
                if let Some(site) = &exec.site {
                    sites.push(site.clone());
                }
            }
            _ => {}
        });
        if source.contains("from-json") {
            assert_eq!(sites.len(), 2);
            assert!(sites[0].same_unit(&sites[1]));
        } else {
            assert!(capture, "the checker inserted Capture");
        }
    }
}

fn envelope() -> serde_json::Value {
    let bytes = crate::test_helper::core_shell()
        .marshal(SpawnGrant::Inherit)
        .unwrap();
    serde_json::from_slice(&bytes.0).unwrap()
}

fn failure(bytes: &[u8]) -> DecodeFailure {
    let Err(error) = marshal::read(
        &crate::test_helper::core_runtime(),
        &Marshalled(bytes.to_vec().into()),
    ) else {
        panic!("invalid parent accepted")
    };
    let Reason::Protocol(protocol) = error.reason else {
        panic!("protocol failure")
    };
    let ProtocolFailure::Decode(failure) = *protocol else {
        panic!("one decode envelope")
    };
    assert_eq!(failure.kind, ProtocolData::Parent);
    assert_eq!(failure.operation, "child");
    failure.cause
}

fn value_failure(value: &serde_json::Value) -> DecodeFailure {
    failure(&serde_json::to_vec(value).unwrap())
}

#[test]
fn a_parent_is_one_json_document_with_typed_syntax_and_shape_failures() {
    for bytes in [b"".as_slice(), b"{", b"not JSON", b"{}{}"] {
        assert!(matches!(*failure(bytes).cause, DecodeCause::JsonSyntax(_)));
    }
    assert!(matches!(
        *failure(b"false").cause,
        DecodeCause::JsonShape(_)
    ));
    let mut value = envelope();
    value["terms"]["stack_limit"] = json!(false);
    let error = value_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("terms".into()),
            DataStep::Field("stack_limit".into())
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::JsonShape(_)));
    let mut value = envelope();
    value["context"]["handlers"] =
        json!([{"entries": [["bad", {"not-a-value": null}]], "kind": "Within"}]);
    let error = value_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("context".into()),
            DataStep::Field("handlers".into()),
            DataStep::Index(0usize.into()),
            DataStep::Field("entries".into()),
            DataStep::Index(0usize.into()),
            DataStep::Index(1usize.into()),
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::UnknownTag(_)));
    let mut value = envelope();
    value["context"]["grants"] = json!([{"not-a-grant-field": null}]);
    let error = value_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("context".into()),
            DataStep::Field("grants".into()),
            DataStep::Index(0usize.into()),
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::UnknownField(_)));
}

#[test]
fn parent_structs_keep_positional_optional_duplicate_and_unknown_field_rules() {
    let value = envelope();
    let sequence = [
        "rows", "env", "context", "sources", "terms", "fixings", "grant",
    ]
    .map(|key| value[key].clone());
    marshal::read(
        &crate::test_helper::core_runtime(),
        &Marshalled(serde_json::to_vec(&sequence).unwrap().into()),
    )
    .unwrap();
    let bytes = serde_json::to_string(&value).unwrap();
    let duplicate = bytes.replacen('{', r#"{"env":null,"#, 1);
    assert_eq!(
        failure(duplicate.as_bytes()),
        DecodeFailure::new(DecodeCause::DuplicateField("env".into()))
    );
    let unknown = bytes.replacen('{', r#"{"ignored":[false],"ignored":null,"#, 1);
    marshal::read(
        &crate::test_helper::core_runtime(),
        &Marshalled(unknown.into_bytes().into()),
    )
    .unwrap();
    let mut optional = value.clone();
    optional["rows"] = json!([{"Env": [["bound", {"value": {"Closure": {"tags": [], "val": "Unit", "env": null}}}]]}]);
    optional["env"] = json!(0);
    let (back, _) = marshal::read(
        &crate::test_helper::core_runtime(),
        &Marshalled(serde_json::to_vec(&optional).unwrap().into()),
    )
    .unwrap();
    assert!(back.env.binding("bound").unwrap().scheme.is_none());
    let mut value = value;
    value.as_object_mut().unwrap().remove("terms");
    assert_eq!(
        value_failure(&value),
        DecodeFailure::new(DecodeCause::MissingField("terms".into()))
    );
}

fn context_envelope() -> serde_json::Value {
    envelope()["context"].clone()
}

fn raw_context(bytes: &[u8]) -> Result<Context<Vec<wire::Frame>>, DecodeFailure> {
    let document = JsonDocument::parse(bytes).unwrap();
    decode_structural::<RawContext<Vec<wire::Frame>>>(document.root())?.validate_with(Ok)
}

fn context_failure(value: &serde_json::Value) -> DecodeFailure {
    raw_context(&serde_json::to_vec(value).unwrap()).unwrap_err()
}

#[test]
fn raw_context_compact_pretty_and_positional_forms_add_no_defaults() {
    let value = context_envelope();
    let context = raw_context(&serde_json::to_vec(&value).unwrap()).unwrap();
    for format in [JsonFormat::Compact, JsonFormat::Pretty] {
        let raw = RawContext::from_value_with(&context, |_| Ok(Vec::<wire::Frame>::new())).unwrap();
        let back = raw_context(&serialize_structural(&raw, format)).unwrap();
        assert_eq!(
            serde_json::to_value(context.with_handlers(())).unwrap(),
            serde_json::to_value(back.with_handlers(())).unwrap()
        );
    }
    let positional =
        ["env_overrides", "cwd", "grants", "handlers", "args"].map(|key| value[key].clone());
    raw_context(&serde_json::to_vec(&positional).unwrap()).unwrap();
    assert!(matches!(
        *context_failure(&json!(&positional[..4])).cause,
        DecodeCause::Length(_)
    ));
    let mut missing = value;
    missing.as_object_mut().unwrap().remove("args");
    assert_eq!(
        context_failure(&missing),
        DecodeFailure::new(DecodeCause::MissingField("args".into()))
    );
}

#[test]
fn raw_context_duplicates_unknown_fields_and_environment_replacement_are_structural() {
    let cwd = serde_json::to_string(&context_envelope()["cwd"]).unwrap();
    let bytes = format!(
        r#"{{"env_overrides":{{"A":"first","A":"last"}},"cwd":{cwd},"grants":[],"handlers":[],"args":[],"args":[]}}"#
    );
    assert_eq!(
        raw_context(bytes.as_bytes()).unwrap_err(),
        DecodeFailure::new(DecodeCause::DuplicateField("args".into()))
    );
    let bytes = bytes.replace(
        r#""args":[],"args":[]"#,
        r#""args":[],"ignored":{"Float":18446744073709551615},"ignored":null"#,
    );
    let back = raw_context(bytes.as_bytes()).unwrap();
    assert_eq!(
        back.env_overrides.get("A").map(String::as_str),
        Some("last")
    );
    let duplicate_kind = bytes.replace(
        r#""handlers":[]"#,
        r#""handlers":[{"entries":[],"kind":"Within","kind":"Handler"}]"#,
    );
    assert_eq!(
        raw_context(duplicate_kind.as_bytes()).unwrap_err(),
        DecodeFailure::new(DecodeCause::DuplicateField("kind".into()))
            .index(0usize.into())
            .field("handlers"),
    );
    for (name, reserved) in [("PWD", ReservedVar::Pwd), ("OLDPWD", ReservedVar::Oldpwd)] {
        let mut value = context_envelope();
        value["env_overrides"] = json!({name: "value"});
        let error = context_failure(&value);
        assert_eq!(error.path, [DataStep::Field("env_overrides".into())]);
        assert!(
            matches!(*error.cause, DecodeCause::ReservedEnvironment(cause) if cause.name == name && cause.reserved == reserved)
        );
    }
    let mut value = context_envelope();
    value["env_overrides"] = json!({"pwd": "value"});
    assert_eq!(
        raw_context(&serde_json::to_vec(&value).unwrap()).is_err(),
        cfg!(windows)
    );
}

#[test]
fn raw_context_validates_cwd_and_prefixes_grant_paths() {
    let mut value = context_envelope();
    value["cwd"] = json!("relative");
    let error = context_failure(&value);
    assert_eq!(error.path, [DataStep::Field("cwd".into())]);
    assert!(matches!(*error.cause, DecodeCause::Path(_)));
    let mut value = context_envelope();
    let unwalked = format!("{}/../unwalked", value["cwd"].as_str().unwrap());
    value["cwd"] = json!(unwalked);
    let back = raw_context(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(back.with_handlers(())).unwrap()["cwd"],
        unwalked
    );
    let relative = if cfg!(windows) {
        json!({"Windows": "relative".encode_utf16().collect::<Vec<_>>()})
    } else {
        json!({"Unix": b"relative".to_vec()})
    };
    value["grants"] = json!([{"exec": {"allows": [{"File": {"written": "authored", "real": relative}}], "denies": []}}]);
    let error = context_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("grants".into()),
            DataStep::Index(0usize.into()),
            DataStep::Field("exec".into()),
            DataStep::Field("allows".into()),
            DataStep::Index(0usize.into()),
            DataStep::Payload("File".into()),
            DataStep::Field("real".into()),
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::Path(_)));
}

#[test]
fn raw_context_frames_keep_tuple_payload_optional_and_containing_record_paths() {
    let mut value = context_envelope();
    value["handlers"] = json!([{"entries": []}]);
    let error = context_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("handlers".into()),
            DataStep::Index(0usize.into())
        ]
    );
    assert_eq!(*error.cause, DecodeCause::MissingField("kind".into()));
    value["handlers"] = json!([[[], null, "Handler"]]);
    raw_context(&serde_json::to_vec(&value).unwrap()).unwrap();
    value["handlers"] = json!([{"entries": [], "kind": "Within"}]);
    raw_context(&serde_json::to_vec(&value).unwrap()).unwrap();
    for catch_all in [
        json!({"Closure": {"tags": [], "val": "Unit", "env": null}}),
        json!(null),
    ] {
        value["handlers"][0]["catch_all"] = catch_all;
        raw_context(&serde_json::to_vec(&value).unwrap()).unwrap();
    }
    value["handlers"][0]["catch_all"] =
        json!({"Closure": {"tags": [], "val": {"Float": f64::NAN.to_bits()}, "env": null}});
    let error = context_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("handlers".into()),
            DataStep::Index(0usize.into()),
            DataStep::Field("catch_all".into()),
            DataStep::Payload("Closure".into()),
            DataStep::Field("val".into()),
            DataStep::Payload("Float".into()),
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::FiniteBits(_)));
    value["handlers"][0]["catch_all"] = json!(null);
    value["handlers"][0]["entries"] = json!([["handler", {"Closure": {"tags": [], "val": {"Float": f64::INFINITY.to_bits()}, "env": u32::MAX}}]]);
    let error = context_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("handlers".into()),
            DataStep::Index(0usize.into()),
            DataStep::Field("entries".into()),
            DataStep::Index(0usize.into()),
            DataStep::Index(1usize.into()),
            DataStep::Payload("Closure".into()),
            DataStep::Field("val".into()),
            DataStep::Payload("Float".into()),
        ]
    );
    assert!(
        matches!(*error.cause, DecodeCause::FiniteBits(cause) if cause.bits == Count::from_u64(f64::INFINITY.to_bits()))
    );
    value["handlers"][0]["entries"][0][1]["Closure"]["val"] = json!({"Float": (-0.0f64).to_bits()});
    raw_context(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        matches!(
            *value_failure(&{
                let mut parent = envelope();
                parent["context"] = value;
                parent
            })
            .cause,
            DecodeCause::UnresolvedRow(_)
        ),
        "the handler adapter, not RawContext, resolves rows"
    );
}

#[test]
fn handler_and_spawn_grant_validation_keep_first_order_payload_paths() {
    for slot in ["entries", "catch_all"] {
        let mut value = envelope();
        let invalid = json!({"Failure": {"bytes": {"value": "!"}}});
        value["context"]["handlers"] = json!([{"entries": [], "kind": "Within"}]);
        let mut path = vec![
            DataStep::Field("context".into()),
            DataStep::Field("handlers".into()),
            DataStep::Index(0usize.into()),
            DataStep::Field(slot.into()),
        ];
        if slot == "entries" {
            value["context"]["handlers"][0][slot] = json!([["handler", invalid]]);
            path.extend([
                DataStep::Index(0usize.into()),
                DataStep::Index(1usize.into()),
            ]);
        } else {
            value["context"]["handlers"][0][slot] = invalid;
        }
        path.extend([
            DataStep::Payload("Failure".into()),
            DataStep::Payload("bytes".into()),
            DataStep::Field("value".into()),
        ]);
        let error = value_failure(&value);
        assert_eq!(error.path, path);
        assert!(matches!(*error.cause, DecodeCause::Base64(_)));
    }
    let mut value = envelope();
    value["grant"] = json!({"Restrict": {"bytes": {"value": "!"}}});
    let error = value_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("grant".into()),
            DataStep::Payload("Restrict".into()),
            DataStep::Payload("bytes".into()),
            DataStep::Field("value".into()),
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::Base64(_)));
}

#[test]
fn a_nonunicode_cwd_is_the_writers_only_typed_refusal() {
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(b"/invalid-\xff".to_vec())
    };
    #[cfg(windows)]
    let path = {
        use std::os::windows::ffi::OsStringExt;
        let units: Vec<_> = r"C:\invalid-".encode_utf16().chain([0xd800]).collect();
        std::ffi::OsString::from_wide(&units)
    };
    let mut parent = parent();
    parent.seed_cwd(crate::path::CwdPath::new(path.into()).unwrap());
    let error =
        RawContext::from_value_with(&image(&parent).context, |_| Ok(Vec::<wire::Frame>::new()))
            .unwrap_err();
    assert_eq!(error.path, [DataStep::Field("cwd".into())]);
    assert!(matches!(error.cause, JsonEncodeCause::NonUnicodePath(_)));
    let error = parent.marshal(SpawnGrant::Inherit).unwrap_err();
    let Reason::Protocol(protocol) = error.reason else {
        panic!("protocol")
    };
    let ProtocolFailure::Unencodable(error) = *protocol else {
        panic!("encoding")
    };
    assert_eq!(error.operation, ProtocolOperation::Marshal);
    let FrameEncodeFailure::Json(error) = error.cause else {
        panic!("JSON encoding")
    };
    assert_eq!(
        error.path,
        [
            DataStep::Field("context".into()),
            DataStep::Field("cwd".into())
        ]
    );
    assert!(matches!(error.cause, JsonEncodeCause::NonUnicodePath(_)));
}

#[cfg(unix)]
fn denying(deny: &std::path::Path) -> GrantLayer {
    use crate::authority::policy::{Capabilities, FsPolicy};
    crate::test_helper::layer(Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![crate::path::FrozenPath::root()],
            deny_paths: vec![crate::path::FrozenPath::from_surface(
                crate::path::utf8(deny).unwrap(),
            )],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    })
}

#[cfg(unix)]
#[test]
fn grant_stack_traces_cross_unheld_then_the_reader_holds_them_again() {
    let dir = tempfile::tempdir().unwrap();
    let secret = crate::test_helper::real(dir.path())
        .as_path()
        .join("secret");
    std::fs::write(&secret, "s").unwrap();
    let mut parent = parent();
    parent.context.grants.push(denying(&secret));
    let held = parent.context.grants.held_ids();
    assert_eq!(held, [Some(crate::test_helper::trace(&secret).last_id())]);
    let bytes = parent.marshal(SpawnGrant::Inherit).unwrap();
    let document = JsonDocument::parse(&bytes.0).unwrap();
    let wire: wire::Wire = decode_structural(document.root()).unwrap();
    let raw_context = wire.context.validate_with(Ok).unwrap();
    assert_eq!(raw_context.grants.held_ids(), [None]);
    let (back, _) = marshal::read(&runtime(), &bytes).unwrap();
    assert_eq!(back.context.grants.held_ids(), held);
}

#[cfg(unix)]
#[test]
fn a_replaced_exclusion_keeps_its_position_and_its_old_identity() {
    use crate::authority::policy::{Baseline, FsOp};
    let tmp = tempfile::tempdir().unwrap();
    let dir = crate::test_helper::real(tmp.path()).as_path().to_path_buf();
    let (secret, kept, other) = (dir.join("secret"), dir.join("kept"), dir.join("other"));
    std::fs::write(&secret, "s").unwrap();
    std::fs::write(&other, "o").unwrap();
    std::fs::hard_link(&secret, &kept).unwrap();
    let mut parent = parent();
    parent.context.grants.push(denying(&secret));
    let bytes = parent.marshal(SpawnGrant::Inherit).unwrap();
    std::fs::rename(&secret, dir.join("old-secret")).unwrap();
    std::fs::write(&secret, "new").unwrap();
    let (back, _) = marshal::read(&runtime(), &bytes).unwrap();
    assert_eq!(back.context.grants.held_ids(), [None]);
    let reads = |path: &std::path::Path| {
        back.context.grants.admits_fs(
            &Baseline::default(),
            FsOp::Read,
            &crate::test_helper::trace(path),
        )
    };
    assert!(!reads(&secret), "the position stays denied");
    assert!(
        !reads(&kept),
        "the old object stays denied through a hard link"
    );
    assert!(reads(&other));
}

#[cfg(unix)]
#[test]
fn a_child_with_a_retargeted_allowance_link_admits_only_the_old_target() {
    use crate::authority::policy::{Baseline, Capabilities, FsOp, FsPolicy};
    let tmp = tempfile::tempdir().unwrap();
    let dir = crate::test_helper::real(tmp.path()).as_path().to_path_buf();
    for target in ["old", "new"] {
        std::fs::create_dir(dir.join(target)).unwrap();
        std::fs::write(dir.join(target).join("f"), "").unwrap();
    }
    std::os::unix::fs::symlink("old", dir.join("alink")).unwrap();
    let mut parent = parent();
    parent
        .context
        .grants
        .push(crate::test_helper::layer(Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![crate::path::FrozenPath::from_surface(
                    crate::path::utf8(&dir.join("alink")).unwrap(),
                )],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        }));
    let bytes = parent.marshal(SpawnGrant::Inherit).unwrap();
    std::fs::rename(dir.join("alink"), dir.join("old-link")).unwrap();
    std::os::unix::fs::symlink("new", dir.join("alink")).unwrap();
    let child = Shell::child_of(runtime().into(), &bytes, no_base).unwrap();
    let reads = |path: std::path::PathBuf| {
        child.context.grants.admits_fs(
            &Baseline::default(),
            FsOp::Read,
            &crate::test_helper::trace(path),
        )
    };
    assert!(reads(dir.join("old/f")));
    assert!(!reads(dir.join("new/f")));
}
