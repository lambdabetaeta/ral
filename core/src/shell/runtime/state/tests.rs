use super::*;
use crate::ir::ValClos;

#[test]
fn host_bindings_keep_the_declared_closed_scheme() {
    let mut shell = crate::test_helper::core_shell();
    let value = ValClos::from(&crate::io::terminal::TerminalState::default());
    let scheme = ValClos::terminal_scheme();
    shell.set_var("TERMINAL".into(), value.clone(), scheme.clone());
    assert_eq!(shell.scope_lookup("TERMINAL"), Some(&value));
    assert_eq!(
        shell
            .session_schemes()
            .bindings
            .into_iter()
            .find(|(name, _)| name == "TERMINAL"),
        Some(("TERMINAL".into(), Some(scheme))),
    );
}

#[test]
fn refused_host_blocks_install_no_hook() {
    use crate::fact::{BlockRole, InvalidCause, Reason};
    use crate::ir::Env;
    use crate::ir::{CommandName, CommandWord, CompKind, Exec, Val, ValListElem};
    use crate::shell::runtime::{DefaultPolicy, HookName, HookSig};
    use crate::source::Spanned;
    let refused_as = |error: crate::error::Error, role: BlockRole| {
        let Reason::Invalid(failure) = error.reason else {
            panic!("a refused block is an invalid claim")
        };
        let InvalidCause::IllTyped(refusal) = failure.cause else {
            panic!("the cause is the checker's refusal")
        };
        assert_eq!(refusal.role, role);
        assert!(refusal.diagnostics().iter().any(|d| d.code == "T0057"));
    };
    let mut shell = crate::test_helper::core_shell();
    let body = Spanned::synthetic(CompKind::Exec(Exec {
        head: CommandWord::External(CommandName::Bare("echo".into())),
        argv: vec![ValListElem::Single(Spanned::synthetic(Val::Unit))].into(),
        redirects: crate::ir::Redirects::default(),
        site: None,
    }));
    let value = ValClos::thunk(std::sync::Arc::new(body), &Env::EMPTY);
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
