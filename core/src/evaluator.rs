//! CBPV evaluation: the machine (`machine::evaluate`), and the phrase-level
//! verbs (`run_phrases`) that thread a session, a `source`, or a `use` over
//! it.

pub mod audit;
pub(crate) mod capture;
pub mod expr;
pub(crate) mod machine;
pub(crate) mod observe;
pub(crate) mod pattern;
pub(crate) mod redirect;
pub(crate) mod scope;
pub(crate) mod val;

use crate::ir::{Comp, Phrase};
use crate::source::Spanned;
use crate::types::{Break, Env, Mooring, Settled, Shell, Value};
use std::sync::Arc;

pub(crate) use capture::with_audit_capture;
pub use capture::with_capture;

// ── Phrases (§3.2) ───────────────────────────────────────────────────────

/// What one [`run_phrases`] run left behind.
pub(crate) struct Ran {
    /// The scope as it stood when the last phrase finished or the first one
    /// halted — `shell.env`'s value at that moment.
    pub env: Env,
    /// Every name a `Define` bound, in order — what `use` collects.
    pub defined: Vec<String>,
    pub outcome: Settled<Value>,
}

/// Whose phrases these are.  Leases and the PATH-shadow check belong to
/// `Session` alone; a block-local `source` and a `use` body run under a mode
/// that leases nothing.  Only `Session` writes each landed `Define` back
/// into `shell.env` (§1.3, §3.2) — a `Local`/`Module`/`Prelude` run threads
/// its own `E` and never touches the session environment at all.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Session,
    Local,
    Module,
    Prelude,
}

/// Thread `phrases` over a local `E`, starting from `env`: run each phrase
/// in order as its own closed machine, and report `E` as it stood when the
/// last phrase finished or the first one halted.  The one place
/// `Phrase::Define` has meaning.
///
/// A phrase halting stops the loop; `Ran::env` is `env` as extended by the
/// phrases that ran before the halt, never rolled back — the whole of "a
/// `let` before a failing command still binds" (§1.3).
pub(crate) fn run_phrases(
    phrases: &[Spanned<Phrase>],
    env: Env,
    mode: Mode,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Ran {
    let mut env = env;
    let mut defined = Vec::new();
    let last = phrases.len().saturating_sub(1);
    let mut outcome = Ok(Value::Unit);
    for (i, phrase) in phrases.iter().enumerate() {
        let non_final = i != last;
        let result = crate::process::check(mooring).and_then(|()| match &phrase.item {
            Phrase::Run(m) => run_phrase_run(m, &env, non_final, mooring, shell),
            Phrase::Define {
                pattern,
                comp,
                schemes,
            } => run_phrase_define((pattern, comp, schemes), mode, &mut env, mooring, shell, &mut defined),
            Phrase::Source { path } => {
                run_phrase_source(path, phrase.span, mode, &mut env, mooring, shell, &mut defined)
            }
        });
        match result {
            Ok(v) => outcome = Ok(v),
            // A non-final phrase's own hintless error gets the same
            // abandonment hint `Frame::To`'s halt gives a chain step.
            Err(Break::Error(e)) if non_final && e.hint.is_none() => {
                outcome = Err(Break::Error(e.with_hint(
                    "later steps in this block did not run; wrap a step in `attempt` if its \
                     failure should not stop the rest",
                )));
                break;
            }
            Err(err) => {
                outcome = Err(err);
                break;
            }
        }
    }
    Ran {
        env,
        defined,
        outcome,
    }
}

/// `Run(M)`: the last phrase's value is the run's value; a non-final one
/// runs under the ambient sink exactly as a `Bind`'s RHS does (S11) — its
/// bytes are effect, not the run's value.
fn run_phrase_run(m: &Arc<Comp>, env: &Env, non_final: bool, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let closure = crate::types::Closure { comp: Arc::clone(m), env: env.clone() };
    if !non_final {
        return machine::evaluate(closure, mooring, shell);
    }
    capture::with_ambient_stdout(shell, |shell| machine::evaluate(closure, mooring, shell))
}

/// `Define { pattern, comp, schemes }`: the RHS runs under the ambient sink
/// (its bytes are effect, its value is what the pattern destructures); under
/// `Mode::Session` alone, the PATH-shadow check runs first, the landed
/// binding is written to `shell.env` beside `E`, and [`Shell::note_define`]
/// runs beside each name's install.  All-or-nothing, as
/// [`pattern::bind_pattern`] stages it.  A `Define`'s own value is `Unit` —
/// like a block ending in `let`, it is a value boundary by being one.
fn run_phrase_define(
    (pattern, comp, schemes): (
        &crate::ir::IrPattern,
        &Arc<Comp>,
        &[(String, crate::typecheck::Scheme)],
    ),
    mode: Mode,
    env: &mut Env,
    mooring: &Mooring,
    shell: &mut Shell,
    defined: &mut Vec<String>,
) -> Settled<Value> {
    if matches!(mode, Mode::Session) {
        pattern::check_pattern_shadow(pattern, shell)?;
    }
    let closure = crate::types::Closure { comp: Arc::clone(comp), env: env.clone() };
    let v = capture::with_ambient_stdout(shell, |shell| machine::evaluate(closure, mooring, shell))?;
    machine::set_status(&v, shell);
    let is_session = matches!(mode, Mode::Session);
    *env = pattern::bind_pattern_staged(
        pattern,
        &v,
        schemes,
        env.clone(),
        mooring,
        shell,
        |name, binding, shell| {
            if is_session {
                shell.note_define(name, binding);
            }
            defined.push(name.to_string());
        },
    )?;
    if is_session {
        shell.env = env.clone();
    }
    Ok(Value::Unit)
}

/// `Source { path }`: load and run `path`'s phrases in *this* run's own
/// `mode` — a session's top-level `source` defines session names, with
/// leases; one nested inside a `use` body defines nothing.  A load refusal
/// stops the run before any of the file's phrases do; the file's own halt
/// (`ran.outcome`) stops it after its `Define`s are threaded (S12).  A
/// `Session` load's own `shell.env` writes happen transitively, one per
/// `Define` its (recursive) `run_phrases` call lands — nothing extra to
/// write back here.
fn run_phrase_source(
    path: &Arc<Comp>,
    span: Option<crate::source::Span>,
    mode: Mode,
    env: &mut Env,
    mooring: &Mooring,
    shell: &mut Shell,
    defined: &mut Vec<String>,
) -> Settled<Value> {
    let closure = crate::types::Closure { comp: Arc::clone(path), env: env.clone() };
    let path_val = machine::evaluate(closure, mooring, shell)?;
    let Value::String(p) = path_val else {
        return Err(shell
            .err_hint(
                format!("source: expected String, got {}", path_val.type_name()),
                "the path to `source` must be a computation of type F String",
                1,
            )
            .into());
    };
    let ran = crate::builtins::modules::source(&p, env.clone(), mode, span, mooring, shell)?;
    *env = ran.env;
    defined.extend(ran.defined);
    ran.outcome.map(|_| Value::Unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile `source` through the real front end: real source text, never
    /// hand-built IR.
    fn toplevel(source: &str) -> Phrases {
        let ast = crate::syntax::parser::parse_with(source, crate::source::FileId::DUMMY)
            .expect("parse");
        let top = crate::elaborator::elaborate(&ast, std::collections::HashSet::default(), "<test>")
            .expect("elaborate");
        crate::typecheck::typecheck(&top, crate::typecheck::SessionSchemes::default())
            .expect("typecheck")
            .phrases
    }

    type Phrases = Vec<Spanned<Phrase>>;

    #[test]
    fn run_phrases_installs_defines_into_scope_and_reports_them() {
        let phrases = toplevel("let rp_a = 1\nlet rp_b = 2");
        let mut shell = Shell::default();
        let ran = run_phrases(
            &phrases,
            shell.env.clone(),
            Mode::Session,
            &Mooring::adrift(),
            &mut shell,
        );
        ran.outcome.expect("both defines must succeed");
        assert_eq!(ran.defined, vec!["rp_a".to_string(), "rp_b".to_string()]);
        assert_eq!(ran.env.get("rp_a"), Some(&Value::Int(1)));
        assert_eq!(ran.env.get("rp_b"), Some(&Value::Int(2)));
        assert_eq!(shell.env.get("rp_a"), Some(&Value::Int(1)));
    }

    #[test]
    fn run_phrases_halts_but_keeps_defines_made_before_the_halt() {
        let phrases = toplevel("let rp_before = 1\nexit 7\nlet rp_after = 2");
        let mut shell = Shell::default();
        let ran = run_phrases(
            &phrases,
            shell.env.clone(),
            Mode::Session,
            &Mooring::adrift(),
            &mut shell,
        );
        assert!(ran.outcome.is_err(), "the run must halt at `exit 7`");
        assert_eq!(ran.defined, vec!["rp_before".to_string()]);
        assert_eq!(ran.env.get("rp_before"), Some(&Value::Int(1)));
        assert!(ran.env.get("rp_after").is_none());
    }

    #[test]
    fn run_phrases_non_session_mode_defines_nothing_on_the_lease_ledger() {
        let phrases = toplevel("let rp_local = 1");
        let mut shell = Shell::default();
        shell.arm_binding_lease(crate::types::BindingLease {
            idle_calls: 1,
            large_binding_bytes: u64::MAX,
        });
        let before = shell.leased_binding_count();
        let ran = run_phrases(
            &phrases,
            shell.env.clone(),
            Mode::Local,
            &Mooring::adrift(),
            &mut shell,
        );
        ran.outcome.expect("define must succeed");
        assert_eq!(
            shell.leased_binding_count(),
            before,
            "Mode::Local must not call Shell::note_define"
        );
    }

    #[test]
    fn top_level_persists_let_on_error() {
        let phrases = toplevel("let persist_top = 41\nexit 7");
        let mut shell = Shell::default();
        let _ = run_phrases(
            &phrases,
            shell.env.clone(),
            Mode::Session,
            &Mooring::adrift(),
            &mut shell,
        );
        assert!(
            shell.env.get("persist_top").is_some(),
            "the run door must persist `let` bindings even on Exit"
        );
    }

    #[test]
    fn block_discards_let() {
        // A bare `{ … }` statement elaborates to `Run(Return(Thunk(body)))`;
        // unwrap it to reach the block's own body — real source text, never
        // hand-built IR.  Its `let` is a machine-local `Bind`, never a
        // `Phrase::Define`, so it can never reach `shell.env` regardless of
        // how the block is entered.
        let phrases = toplevel("{ let leak_block = 1 }");
        let [phrase] = phrases.as_slice() else {
            panic!("expected one phrase, got {phrases:?}");
        };
        let Phrase::Run(comp) = &phrase.item else {
            panic!("expected a Run phrase, got {:?}", phrase.item);
        };
        let crate::ir::CompKind::Return(crate::ir::Val::Thunk(body)) = &comp.item else {
            panic!("expected a thunked block, got {:?}", comp.item);
        };
        let mut shell = Shell::default();
        let captured = shell.env.clone();
        let closure = crate::types::Closure { comp: body.clone(), env: captured };
        let _ = machine::evaluate(closure, &Mooring::adrift(), &mut shell);
        assert!(
            shell.env.get("leak_block").is_none(),
            "block boundary must discard `let` bindings"
        );
    }
}
