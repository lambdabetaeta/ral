//! CBPV evaluation: the machine (`machine::evaluate`), and the phrase-level
//! verb (`run_phrases`) that threads a session or a `use` body over it.

pub(crate) mod assemble;
pub(crate) mod call;
pub(crate) mod expr;
pub(crate) mod machine;
pub(crate) mod pattern;
pub(crate) mod scope;

use crate::fact::{ApplyFailure, CallOrForce, EvalFailure, Reason};
use crate::ir::{Binding, Comp, Env, Pattern, Phrase, Toplevel, ValClos, View};
use crate::shell::runtime::{Break, Error, Mooring, Settled, Shell};
use crate::ty::Closed;
use std::sync::Arc;

pub use crate::shell::exec::capture::with_capture;
#[cfg(all(test, unix))]
pub(crate) use machine::STAGE_EVAL;

// ── Phrases ──────────────────────────────────────────────────────────────

/// What one [`run_phrases`] run left behind.
pub(crate) struct Ran {
    /// The threaded `E` as it stood when the last phrase finished or the
    /// first one halted — written into `shell.env` only under
    /// `Mode::Session`.
    pub env: Env,
    pub(crate) outcome: Settled<ValClos>,
}

/// Only `Session` leases bindings and writes each landed `Define` back to
/// `shell.env`. `Local` threads its own lexical environment.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Session,
    Local,
}

/// Admit `top` against `env`, then run its phrases over that local `E`, each
/// as its own closed machine, and report `E` as it stood when the
/// last phrase finished or the first one halted.  The one place
/// `Phrase::Define` has meaning.
///
/// A phrase halting stops the loop; `Ran::env` is `env` as extended by the
/// phrases that ran before the halt, never rolled back — the whole of "a
/// `let` before a failing command still binds" (`docs/SPEC.md` §5.6).
pub(crate) fn run_phrases(
    top: &Toplevel,
    env: Env,
    mode: Mode,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Ran {
    if let Err(error) = readmit(top, &env, shell) {
        return Ran {
            env,
            outcome: Err(error),
        };
    }
    let mut ran = Ran {
        env,
        outcome: Ok(ValClos::UNIT),
    };
    let last = top.phrases.len().saturating_sub(1);
    for (i, phrase) in top.phrases.iter().enumerate() {
        let result = shell
            .poll()
            .map_err(|b| {
                b.locate(
                    match &phrase.item {
                        Phrase::Run(_) => "sequence",
                        Phrase::Define { .. } => "let",
                    },
                    phrase.span,
                    shell,
                )
            })
            .and_then(|()| match &phrase.item {
                Phrase::Run(m) => machine::evaluate(Arc::clone(m), ran.env.clone(), mooring, shell),
                Phrase::Define {
                    pattern,
                    comp,
                    hole: (),
                    schemes,
                } => ran
                    .define(pattern, comp, schemes, mode, mooring, shell)
                    .map_err(|b| b.locate("let", phrase.span, shell)),
            });
        match result {
            Ok(v) => ran.outcome = Ok(v),
            // A non-final phrase's error says what it abandoned, as
            // `Frame::Seq`'s halt does for a chain step.
            Err(Break::Error(e)) if i != last => {
                ran.outcome = Err(e.abandoning_tail().into());
                break;
            }
            Err(err) => {
                ran.outcome = Err(err);
                break;
            }
        }
    }
    ran
}

impl Ran {
    /// `Define { pattern, comp, schemes }`: the RHS's value is what the pattern
    /// destructures; under `Mode::Session` alone, each landed binding is also
    /// written to `shell.env`, and [`Shell::note_define`] runs beside it.
    /// All-or-nothing, as [`pattern::destructure`] stages it.  A `Define`'s own
    /// value is `Unit`.
    fn define(
        &mut self,
        pattern: &Pattern,
        comp: &Arc<Comp>,
        schemes: &[(String, Closed)],
        mode: Mode,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<ValClos> {
        let session = matches!(mode, Mode::Session);
        let v = machine::evaluate(Arc::clone(comp), self.env.clone(), mooring, shell)?;
        for (name, value) in pattern::destructure(pattern, &v)? {
            let scheme = schemes
                .iter()
                .find(|(n, _)| **n == *name)
                .map(|(_, scheme)| scheme.clone());
            let binding = Binding { value, scheme };
            if session {
                shell.note_define(&name, &binding);
            }
            self.env.bind(name, binding);
        }
        if session {
            shell.env = self.env.clone();
        }
        Ok(ValClos::UNIT)
    }
}

/// Push `args` and force a thunk or builtin, with a run frame installed.
///
/// Builtins that take function arguments call this, as does the run
/// door's hook arm ([`crate::Shell::run`]), which establishes that frame first.
///
/// # Errors
/// If `val` is not a function value, or the applied body fails.
pub fn apply(
    val: &ValClos,
    args: Vec<ValClos>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    match val.view() {
        View::Thunk { .. } | View::Builtin(_) => machine::apply(val.clone(), args, mooring, shell),
        _ => Err(Error::new(Reason::Evaluation(Box::new(EvalFailure::Apply(
            ApplyFailure {
                operation: CallOrForce::Call,
                found: val.found(),
            },
        ))))
        .into()),
    }
}

/// Before a unit runs, admit each stored datum it uses against the
/// type the unit solved: a later line's misuse of decoded data is caught
/// where it is written, not where the value finally fails.
fn readmit(top: &Toplevel, env: &Env, shell: &Shell) -> Settled<()> {
    for (name, site) in &top.admits {
        if let Some(value) = env.get(name) {
            site.admit(value, shell.fixings()).map_err(|mismatch| {
                let door = format!("${name}");
                mismatch
                    .refusal(&door)
                    .locate(&door, mismatch.witness(), shell)
            })?;
        }
    }
    Ok(())
}

/// `source` compiled against `shell`'s session and run as the session.
#[cfg(test)]
pub(crate) fn run_source(source: &str, shell: &mut Shell) -> Settled<ValClos> {
    let top = crate::compile::compile_and_typecheck(
        source,
        shell.session_schemes(),
        crate::source::FileId::DUMMY,
        None,
        crate::typecheck::Unit::Script,
        &mut |at, written, script| crate::shell::load::load_module(at, written, script, &[], shell),
    )
    .expect("compile");
    run_phrases(
        &top,
        shell.env.clone(),
        Mode::Session,
        &Mooring::adrift(),
        shell,
    )
    .outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real source text through the real front end, never hand-built IR.
    fn toplevel(source: &str) -> Toplevel {
        crate::compile::compile_and_typecheck(
            source,
            crate::test_helper::core_schemes(),
            crate::source::FileId::DUMMY,
            None,
            crate::typecheck::Unit::Script,
            &mut crate::compile::no_modules,
        )
        .expect("compile")
    }

    #[test]
    fn run_phrases_installs_defines_into_scope() {
        let top = toplevel("let rp_a = 1\nlet rp_b = 2");
        let mut shell = crate::test_helper::core_shell();
        let ran = run_phrases(
            &top,
            shell.env.clone(),
            Mode::Session,
            &Mooring::adrift(),
            &mut shell,
        );
        ran.outcome.expect("both defines must succeed");
        assert_eq!(ran.env.get("rp_a"), Some(&ValClos::int(1)));
        assert_eq!(ran.env.get("rp_b"), Some(&ValClos::int(2)));
        assert_eq!(shell.env.get("rp_a"), Some(&ValClos::int(1)));
    }

    #[test]
    fn run_phrases_halts_but_keeps_defines_made_before_the_halt() {
        let top = toplevel("let rp_before = 1\nexit 7\nlet rp_after = 2");
        let mut shell = crate::test_helper::core_shell();
        let ran = run_phrases(
            &top,
            shell.env.clone(),
            Mode::Session,
            &Mooring::adrift(),
            &mut shell,
        );
        assert!(ran.outcome.is_err(), "the run must halt at `exit 7`");
        assert_eq!(ran.env.get("rp_before"), Some(&ValClos::int(1)));
        assert!(ran.env.get("rp_after").is_none());
    }

    #[test]
    fn run_phrases_non_session_mode_defines_nothing_on_the_lease_ledger() {
        let top = toplevel("let rp_local = 1");
        let mut shell = crate::test_helper::core_shell();
        shell.arm_binding_lease(crate::shell::runtime::BindingLease {
            idle_calls: 1,
            large_binding_bytes: u64::MAX,
        });
        let before = shell.leased_binding_count();
        let ran = run_phrases(
            &top,
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
    fn run_phrases_admits_the_supplied_env_not_the_session_env() {
        let mut shell = crate::test_helper::core_shell();
        run_source("let d = !{to-string '1' | from-json}", &mut shell)
            .expect("store a residual datum");
        let top = crate::compile::compile_and_typecheck(
            "let before = 7\nlet result = $[$d + 1]",
            shell.session_schemes(),
            crate::source::FileId::DUMMY,
            None,
            crate::typecheck::Unit::Script,
            &mut crate::compile::no_modules,
        )
        .expect("compile a later unit");

        let mut env = shell.env.clone();
        let mut binding = env.binding("d").expect("stored datum").clone();
        binding.value = ValClos::string("bad");
        env.bind("d".into(), binding);
        let ran = run_phrases(
            &top,
            env.clone(),
            Mode::Local,
            &Mooring::adrift(),
            &mut shell,
        );
        let error = ran.outcome.expect_err("the supplied datum must be refused");
        assert!(error.to_string().contains("$d"), "{error}");
        assert_eq!(ran.env.get("d"), Some(&ValClos::string("bad")));
        assert!(ran.env.get("before").is_none());
        assert!(ran.env.get("result").is_none());
        assert_eq!(shell.env.get("d"), Some(&ValClos::int(1)));
        assert!(shell.env.get("before").is_none());

        let valid_env = shell.env.clone();
        shell.env = env;
        let ran = run_phrases(&top, valid_env, Mode::Local, &Mooring::adrift(), &mut shell);
        ran.outcome.expect("the supplied datum must be admitted");
        assert_eq!(ran.env.get("d"), Some(&ValClos::int(1)));
        assert_eq!(ran.env.get("before"), Some(&ValClos::int(7)));
        assert_eq!(ran.env.get("result"), Some(&ValClos::int(2)));
        assert_eq!(shell.env.get("d"), Some(&ValClos::string("bad")));
        assert!(shell.env.get("before").is_none());
    }

    #[test]
    fn block_discards_let() {
        // A bare `{ … }` statement elaborates to `Run(Return(Thunk(body)))`;
        // unwrap it to reach the block's own body — real source text, never
        // hand-built IR.  Its `let` is a machine-local `Bind`, never a
        // `Phrase::Define`, so it can never reach `shell.env` regardless of
        // how the block is entered.
        let top = toplevel("{ let leak_block = 1 }");
        let [phrase] = top.phrases.as_slice() else {
            panic!("expected one phrase, got {:?}", top.phrases);
        };
        let Phrase::Run(comp) = &phrase.item else {
            panic!("expected a Run phrase, got {:?}", phrase.item);
        };
        let crate::ir::CompKind::Return(crate::ir::Val::Thunk(body)) = &comp.item else {
            panic!("expected a thunked block, got {:?}", comp.item);
        };
        let mut shell = crate::test_helper::core_shell();
        let _ = machine::evaluate(
            Arc::clone(body.shape()),
            shell.env.clone(),
            &Mooring::adrift(),
            &mut shell,
        );
        assert!(
            shell.env.get("leak_block").is_none(),
            "block boundary must discard `let` bindings"
        );
    }
}
