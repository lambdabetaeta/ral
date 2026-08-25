//! CBPV evaluation verbs: the top-level run, the bare in-session run, and
//! the block.
//!
//! Each funnels its result through [`absorb_tail`], the one seam that lands
//! an escaping [`TailCall`], so `Tail` never leaves this module tree.

pub mod audit;
pub(crate) mod call;
pub(crate) mod capture;
pub(crate) mod case;
pub(crate) mod comp;
pub mod expr;
pub(crate) mod observe;
pub(crate) mod pattern;
pub(crate) mod redirect;
pub(crate) mod scope;
pub(crate) mod trampoline;
pub(crate) mod val;

use crate::ir::{Comp, Phrase};
use crate::source::Spanned;
use crate::types::{Break, Control, Env, Mooring, Settled, Shell, Tail, TailCall, ThunkBody, Value};
use std::sync::Arc;

pub(crate) use capture::with_audit_capture;
pub use capture::with_capture;
pub(crate) use trampoline::apply;

// ── Tail absorption ──────────────────────────────────────────────────────

/// Land an escaping [`TailCall`] through the trampoline; pass `Break`
/// verbatim.  The one `Raw<Value>` → `Settled<Value>` seam: the verbs here,
/// thread-worker roots, `child_eval`'s cross-process stage, redirect bodies,
/// [`BuiltinEntry::run`](crate::types::BuiltinEntry::run).
pub(crate) fn absorb_tail(
    raw: crate::types::Raw<Value>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    match raw {
        Ok(v) => Ok(v),
        Err(Control::Tail(TailCall { callee, args })) => apply(callee, args, mooring, shell),
        Err(Control::Break(b)) => Err(b),
    }
}

/// Tail-absorbed run of `comp` in place — no mobile install or discard, no
/// scope frame, no transport dispatch.
///
/// For callers already inside a session (prelude bootstrap, module loading,
/// REPL prompt and config), where a run boundary would round-trip a mobile
/// they never wanted snapshotted. [`Tail::No`], since the caller has
/// obligations beyond `comp`.
///
/// # Errors
/// A `Break` from `comp` — a recoverable error, or an escape such as `exit`.
pub fn evaluate(comp: &Arc<Comp>, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    debug_assert!(
        !shell.mobile.context.grants.is_empty(),
        "grants must be non-empty; Shell::new pre-pushes root"
    );
    absorb_tail(
        comp::eval_comp(comp, mooring, shell, Tail::No),
        mooring,
        shell,
    )
}

// ── Boundary verbs ───────────────────────────────────────────────────────
//
// Both evaluate the body in this process: a `grant` is a dynamic effect
// scope, not a process boundary, so OS confinement happens per child in
// `build_command` (`runtime::command::process`), never at body entry.

/// Run `comp` as a top-level run, installing the post-run
/// [`Mobile`](crate::types::Mobile) for every outcome — Ok, Error, Exit —
/// so a `let` before a failing command still binds for the next run.
///
/// The program is the run's sole computation, hence [`Tail::Yes`]; its value
/// goes straight back to the [`Shell::run`](crate::Shell::run) door.  The
/// run's mobile is swapped in for the duration, so a terminal tail call is
/// absorbed under it.
pub(crate) fn eval_top_level(
    comp: &Arc<Comp>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let mobile = shell.mobile();
    let (post, outcome) = shell.run_with_mobile(mobile, |shell| {
        absorb_tail(
            comp::eval_comp(comp, mooring, shell, Tail::Yes),
            mooring,
            shell,
        )
    });
    shell.install_mobile(post);
    outcome
}

/// Run a thunk body as a block: scope-isolated, mobile discarded on exit, so
/// `let`, `cd`, module loads, plugin registrations, and env-var changes do
/// not propagate.  Only `last_status` and observations, which the body posts
/// straight to the shared trail, cross the boundary.
///
/// `captured` is the lexical environment a `Value::Block` carries;
/// [`Shell::with_thunk_body`] rescopes the body's mobile to it plus a fresh
/// frame and, as a [`ThunkBody::Block`], folds only `last_status` back.
///
/// `tail` is the block's own tail position, granted [`Tail::Yes`] only by the
/// trampoline's final argument.  The body's mobile is installed for the
/// absorption either way; tail-ness only chooses whether it trampolines.
///
/// Outside the crate the block contract is reached through [`apply`].
pub(crate) fn eval_block(
    body: &Arc<Comp>,
    captured: &Arc<Env>,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    shell.with_thunk_body(ThunkBody::Block, captured, |shell, mobile| {
        shell.run_with_mobile(mobile, |shell| {
            absorb_tail(comp::eval_comp(body, mooring, shell, tail), mooring, shell)
        })
    })
}

// ── Phrases (§3.2) ───────────────────────────────────────────────────────

/// What one [`run_phrases`] run left behind.
pub(crate) struct Ran {
    /// The scope as it stood when the last phrase finished or the first one
    /// halted — `shell.mobile.scope`'s value at that moment.
    pub env: Env,
    /// Every name a `Define` bound, in order — what `use` collects.
    pub defined: Vec<String>,
    pub outcome: Settled<Value>,
}

/// Whose phrases these are.  Leases and the PATH-shadow check belong to
/// `Session` alone; a block-local `source` and a `use` body run under a mode
/// that leases nothing.  (`shell.env`'s per-`Define` session install — §1.3
/// — waits on the field itself, which is W2a's: on the recursive evaluator
/// the environment thread *is* `shell.mobile.scope`, so a `Session` run's
/// `Define`s are already visible to every later phrase without a separate
/// write-back.)
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "Session/Module/Prelude callers land at W1e's cutover")]
pub(crate) enum Mode {
    Session,
    Local,
    Module,
    Prelude,
}

/// Thread `phrases` over `shell.mobile.scope`: install `env` at the start,
/// run each phrase in order, and report the scope as it stood when the last
/// one finished or the first one halted.  The one place `Phrase::Define`
/// has meaning.
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
    shell.mobile.scope = env;
    let mut defined = Vec::new();
    let last = phrases.len().saturating_sub(1);
    let mut outcome = Ok(Value::Unit);
    for (i, phrase) in phrases.iter().enumerate() {
        let non_final = i != last;
        let result = match &phrase.item {
            Phrase::Run(m) => run_phrase_run(m, non_final, mooring, shell),
            Phrase::Define {
                pattern,
                comp,
                schemes,
            } => run_phrase_define(pattern, comp, schemes, mode, mooring, shell, &mut defined),
            Phrase::Source { path } => {
                run_phrase_source(path, phrase.span, mode, mooring, shell, &mut defined)
            }
        };
        match result {
            Ok(v) => outcome = Ok(v),
            Err(Break::Error(e)) if non_final && e.hint.is_none() => {
                outcome = Err(match comp::note_abandoned_steps(Control::Break(Break::Error(e))) {
                    Control::Break(b) => b,
                    // `note_abandoned_steps` never returns `Tail`.
                    Control::Tail(_) => unreachable!("a phrase's own error never turns tail"),
                });
                break;
            }
            Err(err) => {
                outcome = Err(err);
                break;
            }
        }
    }
    Ran {
        env: shell.mobile.scope.clone(),
        defined,
        outcome,
    }
}

/// `Run(M)`: the last phrase's value is the run's value; a non-final one
/// runs under the ambient sink exactly as a `Bind`'s RHS does (S11) — its
/// bytes are effect, not the run's value.
fn run_phrase_run(m: &Arc<Comp>, non_final: bool, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    if !non_final {
        return evaluate(m, mooring, shell);
    }
    match capture::with_ambient_stdout(shell, |shell| evaluate(m, mooring, shell)) {
        Ok(settled) => settled,
        Err(io_err) => Err(shell.err(format!("statement sink: {io_err}"), 1).into()),
    }
}

/// `Define { pattern, comp, schemes }`: the RHS runs under the ambient sink
/// (its bytes are effect, its value is what the pattern destructures); under
/// `Mode::Session` alone, the PATH-shadow check runs first and
/// [`Shell::note_define`] runs beside each name's install.  All-or-nothing,
/// as [`pattern::bind_pattern`] stages it.  A `Define`'s own value is `Unit`
/// — like a block ending in `let`, it is a value boundary by being one.
fn run_phrase_define(
    pattern: &crate::ir::IrPattern,
    comp: &Arc<Comp>,
    schemes: &[(String, crate::typecheck::Scheme)],
    mode: Mode,
    mooring: &Mooring,
    shell: &mut Shell,
    defined: &mut Vec<String>,
) -> Settled<Value> {
    if matches!(mode, Mode::Session) {
        pattern::check_pattern_shadow(pattern, shell).map_err(|control| match control {
            Control::Break(b) => b,
            Control::Tail(_) => unreachable!("a pattern-shadow check never applies a tail call"),
        })?;
    }
    let v = match capture::with_ambient_stdout(shell, |shell| evaluate(comp, mooring, shell)) {
        Ok(settled) => settled?,
        Err(io_err) => return Err(shell.err(format!("statement sink: {io_err}"), 1).into()),
    };
    comp::set_status_from_value(&v, shell);
    let staged = pattern::bind_pattern(pattern, &v, schemes, mooring, shell)?;
    for (name, binding) in staged {
        if matches!(mode, Mode::Session) {
            shell.note_define(&name, &binding);
        }
        shell.install_scope_binding(name.clone(), binding);
        defined.push(name);
    }
    Ok(Value::Unit)
}

/// `Source { path }`: load and run `path`'s phrases in *this* run's own
/// `mode` — a session's top-level `source` defines session names, with
/// leases; one nested inside a `use` body defines nothing.  A load refusal
/// stops the run before any of the file's phrases do; the file's own halt
/// (`ran.outcome`) stops it after its `Define`s are threaded (S12).
fn run_phrase_source(
    path: &Arc<Comp>,
    span: Option<crate::source::Span>,
    mode: Mode,
    mooring: &Mooring,
    shell: &mut Shell,
    defined: &mut Vec<String>,
) -> Settled<Value> {
    let path_val = evaluate(path, mooring, shell)?;
    let Value::String(p) = path_val else {
        return Err(shell
            .err_hint(
                format!("source: expected String, got {}", path_val.type_name()),
                "the path to `source` must be a computation of type F String",
                1,
            )
            .into());
    };
    let env = shell.mobile.scope.clone();
    let ran = crate::builtins::modules::source(&p, env, mode, span, mooring, shell)?;
    shell.mobile.scope = ran.env;
    defined.extend(ran.defined);
    ran.outcome.map(|_| Value::Unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile `source` through the real, already-landed `elaborate_toplevel`
    /// + `typecheck_toplevel` pair — the run door does not call them yet
    /// (W1e), so `run_phrases` has no other route to a checked `Toplevel`
    /// until then.  Not hand-built IR: real source text through the real
    /// front end, just not the one door wires wraps around it yet.
    fn toplevel(source: &str) -> Phrases {
        let ast = crate::syntax::parser::parse_with(source, crate::source::FileId::DUMMY)
            .expect("parse");
        let top = crate::elaborator::elaborate_toplevel(&ast, Default::default(), "<test>")
            .expect("elaborate");
        crate::typecheck::typecheck_toplevel(&top, crate::typecheck::SessionSchemes::default())
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
            shell.mobile.scope.clone(),
            Mode::Session,
            &Mooring::adrift(),
            &mut shell,
        );
        ran.outcome.expect("both defines must succeed");
        assert_eq!(ran.defined, vec!["rp_a".to_string(), "rp_b".to_string()]);
        assert_eq!(ran.env.get("rp_a"), Some(&Value::Int(1)));
        assert_eq!(ran.env.get("rp_b"), Some(&Value::Int(2)));
        assert_eq!(shell.mobile.scope.get("rp_a"), Some(&Value::Int(1)));
    }

    #[test]
    fn run_phrases_halts_but_keeps_defines_made_before_the_halt() {
        let phrases = toplevel("let rp_before = 1\nexit 7\nlet rp_after = 2");
        let mut shell = Shell::default();
        let ran = run_phrases(
            &phrases,
            shell.mobile.scope.clone(),
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
            shell.mobile.scope.clone(),
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
        let source = "let persist_top = 41; exit 7";
        let comp = Arc::new(crate::compile(source).expect("compile"));
        let mut shell = Shell::default();
        let _ = eval_top_level(&comp, &Mooring::adrift(), &mut shell);
        assert!(
            shell.mobile.scope.get("persist_top").is_some(),
            "top-level mobile must persist `let` bindings even on Exit"
        );
    }

    #[test]
    fn block_discards_let() {
        let source = "let leak_block = 1";
        let body = Arc::new(crate::compile(source).expect("compile"));
        let mut shell = Shell::default();
        let captured = shell.snapshot();
        let _ = eval_block(&body, &captured, Tail::No, &Mooring::adrift(), &mut shell);
        assert!(
            shell.mobile.scope.get("leak_block").is_none(),
            "block boundary must discard `let` bindings"
        );
    }
}
