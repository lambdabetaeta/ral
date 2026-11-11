//! The host's loading doors, outside the machine.
//!
//! Compile `.ral` text against the live session and run it, as the rc, a
//! profile, a plugin, a capability file and exarch's agent library are.  `use`
//! (`builtins::modules`) drives the same [`module_phrases`] door from inside a
//! run.
//!
//! [`evaluate_source`] is the host's door, and [`check_source`] the compile
//! step both share, so a file that fails to compile reaches every caller as an
//! `Error` carrying its [`Rejection`](crate::diagnostic::Rejection).

pub mod profile;

use crate::fact::{DepthFailure, DepthKind, ModuleCycle, Reason, StateFailure};
use crate::ir::Toplevel;
use crate::shell::evaluator::{Mode, Ran};
use crate::shell::runtime::{Env, Error, Mooring, Settled, Shell, Value};
use crate::source::{Source, SourceOrigin};

const MAX_SOURCE_DEPTH: usize = 100;

/// Compile `source`, read from `origin`, and run it in the caller's own scope,
/// its defines landing in `shell.env`.
///
/// The caller owns the filesystem read; `origin` only identifies the
/// registered source and keys the cycle stack.  `contract` is the
/// loading form's declared table, whose closed keyset `source`'s returned row
/// is held to — the plugin loader's door, for a manifest's fields — in the
/// same check as the rest of the file.
///
/// # Errors
/// A compile failure, a broken `contract` among them, as an error carrying its
/// rejection; a circular dependency or the depth limit; or the file's own failure.
pub fn evaluate_source(
    mooring: &Mooring,
    shell: &mut Shell,
    source: &str,
    origin: &SourceOrigin,
    contract: Option<crate::typecheck::ReturnContract>,
) -> Settled<Value> {
    let top = check_source(source, origin, shell, contract)?;
    let load = ModuleLoad { top: &top, origin };
    let ran = module_phrases(load, shell.env.clone(), Mode::Local, mooring, shell)?;
    // Unlike a nested `use`, this loader's whole point is to install its
    // defines into the running session — rc, a plugin, a capability file,
    // exarch's agent library — so its own `Ran::env` lands in `shell.env`
    // here, the one write-back `Mode::Local` itself skips (no lease: this is
    // host-installed library code, not an interactive `let`).
    shell.env = ran.env;
    ran.outcome
}

// ── Phrases (§10.3) ─────────────────────────────────────────────────────

/// A compiled, registered module ready to run: what both loaders hand
/// [`module_phrases`].
#[derive(Clone, Copy)]
pub(crate) struct ModuleLoad<'a> {
    pub(crate) top: &'a Toplevel,
    pub(crate) origin: &'a SourceOrigin,
}

/// Run `load`'s phrases under `mode`, guarded by the cycle check, the depth
/// limit and the module-stack frame: the one door every runtime load goes
/// through, `use` and [`evaluate_source`] alike.  `use` is a native, whose
/// frame stamps nothing, so this records nothing further.
///
/// # Errors
/// A circular dependency or a depth-limit refusal — before any phrase runs.
/// A phrase that halts is `Ran::outcome`, not this `Err`: a module is not
/// transactional (`docs/SPEC.md` §5.6), so every caller threads `Ran::env`
/// before it propagates `Ran::outcome`.
pub(crate) fn module_phrases(
    load: ModuleLoad<'_>,
    env: Env,
    mode: Mode,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Ran> {
    let ModuleLoad { top, origin } = load;
    if shell.context.modules.stack.contains(origin) {
        let mut chain: Vec<String> = shell
            .context
            .modules
            .stack
            .iter()
            .map(SourceOrigin::to_string)
            .collect();
        chain.push(origin.to_string());
        return Err(Error::new(Reason::State(Box::new(StateFailure::ModuleCycle(
            ModuleCycle { chain },
        ))))
        .into());
    }
    if shell.context.modules.stack.len() >= MAX_SOURCE_DEPTH {
        return Err(
            Error::new(Reason::State(Box::new(StateFailure::Depth(DepthFailure {
                kind: DepthKind::Source,
                limit: MAX_SOURCE_DEPTH.into(),
            }))))
            .into(),
        );
    }
    let frame = ModuleStackFrame::enter(shell, origin.clone());
    let ran = crate::shell::evaluator::run_phrases(top, env, mode, mooring, frame.shell);
    drop(frame);
    Ok(ran)
}

/// Pops the module stack on `Drop`, panic included — the guard [`module_phrases`] promised.
struct ModuleStackFrame<'a> {
    shell: &'a mut Shell,
}

impl<'a> ModuleStackFrame<'a> {
    fn enter(shell: &'a mut Shell, origin: SourceOrigin) -> Self {
        shell.context.modules.stack.push(origin);
        Self { shell }
    }
}

impl Drop for ModuleStackFrame<'_> {
    fn drop(&mut self) {
        self.shell.context.modules.stack.pop();
    }
}

/// Compile `source` seeded from the live session's schemes, so a loaded
/// file sees the names already installed.  A file `origin` is the compile
/// door's script too, so the file's `$SCRIPT` references bake to it.
///
/// Registers `source` before compiling, so every span the compile stamps,
/// a rejection's among them, resolves in the session's registry.
///
/// Also the binding-lease harvest seam: every runtime-compiled load passes
/// through here inside an already-committed run, so its referenced names
/// count as real uses and renewing them once here covers all of them.
pub(crate) fn check_source(
    source: &str,
    origin: &SourceOrigin,
    shell: &mut Shell,
    contract: Option<crate::typecheck::ReturnContract>,
) -> Settled<Toplevel> {
    let text = Source::from_text(origin.clone(), source);
    let file = shell.session.sources.register(text.clone());
    let top = crate::compile::compile_and_typecheck(
        source,
        shell.session_schemes(),
        file,
        origin.file(),
        contract,
    )
    .map_err(|error| {
        Error::new(Reason::Compile(Box::new(error.failure(origin.to_string()))))
            .with_rejection(error.reject(text))
    })?;
    if shell.local.bindings.armed() {
        shell.local.bindings.renew(top.referenced_names());
    }
    Ok(top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{CompilePhase, FailureCode as _};
    use crate::shell::runtime::Break;

    #[test]
    fn check_source_keeps_compile_summary_separate_from_rich_rejection() {
        for (source, parse) in [("let = ", true), ("$[1 + true]", false)] {
            let mut shell = crate::test_helper::core_shell();
            let origin = SourceOrigin::named("<load-summary>");
            let Err(Break::Error(error)) = check_source(source, &origin, &mut shell, None) else {
                panic!("source must fail compilation");
            };
            let Reason::Compile(summary) = &error.reason else {
                panic!("compile summary")
            };
            assert_eq!(summary.source, "<load-summary>");
            let rejection = error.rejection().expect("rich report");
            assert_eq!(rejection.source.origin(), &origin);
            assert_eq!(rejection.source.as_str(), source);
            match &summary.phase {
                CompilePhase::Parse => assert!(parse),
                CompilePhase::Types(errors) => {
                    assert!(!parse);
                    assert_eq!(errors.count, rejection.reports.len().into());
                }
            }
            assert_eq!(error.failure_code().get(), 1);
            assert!(
                rejection
                    .render()
                    .contains(if parse { "P0001" } else { "T0010" })
            );
            assert!(
                !error
                    .describe()
                    .contains(if parse { "P0001" } else { "T0010" })
            );
        }
    }

    #[test]
    fn module_phrases_refuses_cycle_and_depth_without_changing_the_stack() {
        let mut shell = crate::test_helper::core_shell();
        let origin = SourceOrigin::named("<cycle>");
        let top = check_source("return 1", &origin, &mut shell, None).unwrap();
        shell.context.modules.stack = vec![SourceOrigin::named("<outer>"), origin.clone()];
        let Err(Break::Error(error)) = module_phrases(
            ModuleLoad {
                top: &top,
                origin: &origin,
            },
            shell.env.clone(),
            Mode::Local,
            &Mooring::adrift(),
            &mut shell,
        ) else {
            panic!("cycle refusal");
        };
        assert_eq!(
            error.reason,
            Reason::State(Box::new(StateFailure::ModuleCycle(ModuleCycle {
                chain: vec!["<outer>".into(), "<cycle>".into(), "<cycle>".into()],
            })))
        );
        assert_eq!(shell.context.modules.stack.len(), 2);
        shell.context.modules.stack = (0..MAX_SOURCE_DEPTH)
            .map(|i| SourceOrigin::named(format!("<depth-{i}>")))
            .collect();
        let Err(Break::Error(error)) = module_phrases(
            ModuleLoad {
                top: &top,
                origin: &origin,
            },
            shell.env.clone(),
            Mode::Local,
            &Mooring::adrift(),
            &mut shell,
        ) else {
            panic!("depth refusal");
        };
        assert_eq!(
            error.reason,
            Reason::State(Box::new(StateFailure::Depth(DepthFailure {
                kind: DepthKind::Source,
                limit: MAX_SOURCE_DEPTH.into(),
            })))
        );
        assert_eq!(shell.context.modules.stack.len(), MAX_SOURCE_DEPTH);
    }
}
