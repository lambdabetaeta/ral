//! CBPV evaluation: the machine (`machine::evaluate`), and the phrase-level
//! verb (`run_phrases`) that threads a session or a `use` body over it.

pub(crate) mod assemble;
pub(crate) mod audit;
pub(crate) mod capture;
pub(crate) mod expr;
pub(crate) mod machine;
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

/// A halt in a non-final step abandons what follows it. Said the same way
/// wherever a block is sequenced: a phrase list here, a `Bind` chain in
/// [`machine`].
const ABANDONED_TAIL_HINT: &str = "later steps in this block did not run; wrap a step in \
                                   `attempt` if its failure should not stop the rest";

// ── Phrases ──────────────────────────────────────────────────────────────

/// What one [`run_phrases`] run left behind.
pub(crate) struct Ran {
    /// The threaded `E` as it stood when the last phrase finished or the
    /// first one halted — written into `shell.env` only under
    /// `Mode::Session`.
    pub env: Env,
    /// Every name a `Define` bound, in order — what `use` collects.
    pub(crate) defined: Vec<String>,
    pub(crate) outcome: Settled<Value>,
}

/// Whose phrases these are.  Leases belong to `Session` alone; the host loading door (`evaluate_source` — rc, a
/// plugin, a capability file) and a `use` body run under a mode that leases
/// nothing.  Only `Session` writes each landed `Define` back into
/// `shell.env` (`docs/SPEC.md` §5.6) — a `Local`/`Module`/`Prelude` run
/// threads its own `E` and never touches the session environment at all.
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
/// `let` before a failing command still binds" (`docs/SPEC.md` §5.6).
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
            Phrase::Run(m) => machine::evaluate(Arc::clone(m), env.clone(), mooring, shell),
            Phrase::Define {
                pattern,
                comp,
                schemes,
            } => run_phrase_define(
                DefinePhrase {
                    pattern,
                    comp,
                    schemes,
                },
                mode,
                &mut env,
                mooring,
                shell,
                &mut defined,
            ),
        });
        match result {
            Ok(v) => outcome = Ok(v),
            // A non-final phrase's own hintless error gets the same
            // abandonment hint `Frame::To`'s halt gives a chain step.
            Err(Break::Error(e)) if non_final && e.hint.is_none() => {
                outcome = Err(Break::Error(e.with_hint(ABANDONED_TAIL_HINT)));
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

/// Before a session unit runs, admit each stored datum it uses against the
/// type the unit solved: a later line's misuse of decoded data is caught
/// where it is written, not where the value finally fails.
pub(crate) fn readmit(top: &crate::ir::Toplevel, shell: &Shell) -> Settled<()> {
    for (name, site) in &top.admits {
        if let Some(value) = shell.env.get(name) {
            site.admit(value)
                .map_err(|mismatch| mismatch.refusal(&format!("${name}"), shell))?;
        }
    }
    Ok(())
}

/// A `Phrase::Define`'s three fields, borrowed together — spreading them
/// across `run_phrase_define`'s own parameter list would push it past
/// clippy's argument-count lint.
#[derive(Clone, Copy)]
struct DefinePhrase<'a> {
    pattern: &'a crate::ir::IrPattern,
    comp: &'a Arc<Comp>,
    schemes: &'a [(String, Arc<crate::typecheck::Scheme>)],
}

/// `Define { pattern, comp, schemes }`: the RHS's value is what the pattern
/// destructures; under `Mode::Session` alone, the landed
/// binding is written to `shell.env` beside `E`, and [`Shell::note_define`]
/// runs beside each name's install.  All-or-nothing, as
/// [`pattern::bind_pattern`] stages it.  A `Define`'s own value is `Unit` —
/// like a block ending in `let`, it is a value boundary by being one.
fn run_phrase_define(
    define: DefinePhrase<'_>,
    mode: Mode,
    env: &mut Env,
    mooring: &Mooring,
    shell: &mut Shell,
    defined: &mut Vec<String>,
) -> Settled<Value> {
    let DefinePhrase {
        pattern,
        comp,
        schemes,
    } = define;
    let is_session = matches!(mode, Mode::Session);
    let v = machine::evaluate(Arc::clone(comp), env.clone(), mooring, shell)?;
    *env = pattern::bind_pattern_staged(
        pattern,
        &v,
        schemes,
        env.clone(),
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

/// `source` compiled against `shell`'s session and run as the session.
#[cfg(test)]
pub(crate) fn run_source(source: &str, shell: &mut Shell) -> Settled<Value> {
    let top = crate::compile_and_typecheck(
        source,
        shell.session_schemes(),
        crate::source::FileId::DUMMY,
        "<test>",
        None,
    )
    .expect("compile");
    readmit(&top, shell)?;
    run_phrases(
        &top.phrases,
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
    fn toplevel(source: &str) -> Vec<Spanned<Phrase>> {
        crate::compile_and_typecheck(
            source,
            crate::typecheck::SessionSchemes::default(),
            crate::source::FileId::DUMMY,
            "<test>",
            None,
        )
        .expect("compile")
        .phrases
    }

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
        let mut shell = Shell::default();
        let _ = run_source("let persist_top = 41\nexit 7", &mut shell);
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
