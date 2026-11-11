use super::super::value::Value;
use super::*;
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::Nursery;

/// A narrower no test here may reach: only a Base grant consults one.
fn no_base(
    _: &str,
    _: &crate::authority::grant::freeze::FreezeCtx,
) -> Result<crate::authority::policy::GrantLayer, crate::error::Error> {
    panic!("only a Base grant consults the narrower")
}

/// Park then adopt returns a bequest from which a child still carries the
/// parent's whole lexical scope.
#[test]
fn nursery_round_trips_a_forked_session() {
    let mut shell = crate::test_helper::core_shell();
    shell.set_var("parent_binding".to_string(), Value::Int(42));
    let nursery = Nursery::default();
    let id = nursery.park(shell.bequeath(SpawnGrant::Inherit));
    let adopted = nursery
        .adopt(id)
        .expect("the parked bequest must be adoptable");
    let child = Shell::child_of(crate::test_helper::core_runtime().into(), adopted, no_base)
        .expect("a child");

    assert_eq!(
        child.env.get("parent_binding"),
        Some(&Value::Int(42)),
        "the bequest must carry the parent's lexical scope"
    );
}

#[test]
fn refused_host_blocks_install_neither_bindings_nor_hooks() {
    use crate::fact::{BlockRole, InvalidCause, Reason};
    use crate::ir::{CommandName, CommandWord, CompKind, Exec, Val, ValListElem};
    use crate::shell::runtime::{Closure, DefaultPolicy, HookName, HookSig};
    use crate::source::Spanned;
    let refused_as = |error: crate::error::Error, role: BlockRole| {
        let Reason::Invalid(failure) = error.reason else {
            panic!("a refused block is an invalid claim")
        };
        let InvalidCause::IllTyped(refusal) = failure.cause else {
            panic!("the cause is the checker's refusal")
        };
        assert_eq!(refusal.role, role);
        assert!(refusal.diagnostics.iter().any(|d| d.code == "T0057"));
    };
    let mut shell = crate::test_helper::core_shell();
    let body = Spanned::synthetic(CompKind::Exec(Exec {
        head: CommandWord::External(CommandName::Bare("echo".into())),
        argv: vec![ValListElem::Single(Spanned::synthetic(Val::Unit))].into(),
        redirects: crate::ir::Redirects::default(),
        site: None,
    }));
    let value = Value::Thunk(Closure::closed(body));
    shell.bind_value("host".into(), Value::Int(7)).unwrap();
    let error = shell.bind_value("host".into(), value.clone()).unwrap_err();
    refused_as(error, BlockRole::Binding("host".into()));
    assert_eq!(shell.scope_lookup("host"), Some(&Value::Int(7)));
    let name = HookName::session("refused");
    let error = shell
        .register_hook(name.clone(), value, HookSig::Block, DefaultPolicy::denied())
        .unwrap_err()
        .into_error();
    refused_as(error, BlockRole::Hook("refused".into()));
    assert!(!shell.has_hook(&name));
}

#[test]
fn write_sink_preserves_a_tee_prefix_and_a_typed_stream_io_failure() {
    use crate::fact::{IoOperation, IoSubject, Reason, StandardStream};
    use crate::io::{Sink, new_buffer};
    use std::sync::Arc;

    let file = tempfile::NamedTempFile::new().unwrap();
    let read_only = std::fs::File::open(file.path()).unwrap();
    let (prefix, written) = new_buffer();
    let mut sink = Sink::Tee(Box::new(prefix), Box::new(Sink::File(Arc::new(read_only))));
    let scope = crate::process::DurableRoot::default().child();
    let Err(Break::Error(error)) =
        Shell::write_sink(&mut sink, &scope, b"prefix", StandardStream::Stdout)
    else {
        panic!("read-only file cannot accept bytes");
    };
    let Reason::Io(failure) = &error.reason else {
        panic!("typed I/O")
    };
    assert_eq!(failure.operation, IoOperation::Write);
    assert_eq!(failure.subject, IoSubject::Stream(StandardStream::Stdout));
    assert_eq!(written.take().bytes, b"prefix");
    assert_eq!(error.hints(), []);
}

#[test]
fn write_sink_keeps_cancellation_owner_instead_of_turning_it_into_io() {
    use crate::fact::StandardStream;
    let owner = crate::process::DurableRoot::default().child();
    let scope = owner.child();
    owner.cancel(CancelCause::Cancelled);
    let (_reader, writer) = crate::process::cloexec_pipe().unwrap();
    let mut sink = crate::io::Sink::Pipe {
        writer: std::sync::Arc::new(writer),
        edge: crate::io::Edge::new(),
        stage: scope.id(),
    };
    let Err(Break::Escape(crate::shell::runtime::Escape::Stopped(stop))) =
        Shell::write_sink(&mut sink, &scope, b"unwritten", StandardStream::Stdout)
    else {
        panic!("cancellation remains a stop")
    };
    assert_eq!(stop.cause, CancelCause::Cancelled);
    assert_eq!(stop.owner, Some(owner.id()));
}
