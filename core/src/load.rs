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

use crate::evaluator::{Mode, Ran};
use crate::ir::Toplevel;
use crate::source::Source;
use crate::types::{Env, Mooring, Settled, Shell, Value, sig};

const MAX_SOURCE_DEPTH: usize = 100;

/// Compile `source` under `virtual_path` and run it in the caller's own
/// scope, its defines landing in `shell.env`.
///
/// The path is virtual: the caller owns the filesystem read; this only
/// names the registered source and keys the cycle stack.  `contract` is the
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
    virtual_path: &str,
    contract: Option<crate::typecheck::ReturnContract>,
) -> Settled<Value> {
    let top = check_source(source, virtual_path, shell, contract)?;
    let load = ModuleLoad {
        top: &top,
        virtual_path,
    };
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
    pub(crate) virtual_path: &'a str,
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
    let ModuleLoad { top, virtual_path } = load;
    let key = virtual_path.to_string();
    if shell.context.modules.stack.contains(&key) {
        let cycle: Vec<&str> = shell
            .context
            .modules
            .stack
            .iter()
            .map(std::string::String::as_str)
            .collect();
        return Err(sig(format!(
            "circular dependency: {} -> {key}",
            cycle.join(" -> ")
        )));
    }
    if shell.context.modules.stack.len() >= MAX_SOURCE_DEPTH {
        return Err(sig(format!(
            "recursion depth limit ({MAX_SOURCE_DEPTH}) exceeded"
        )));
    }
    let frame = ModuleStackFrame::enter(shell, key);
    let ran = crate::evaluator::run_phrases(&top.phrases, env, mode, mooring, frame.shell);
    drop(frame);
    Ok(ran)
}

/// Pops the module stack on `Drop`, panic included — the guard [`module_phrases`] promised.
struct ModuleStackFrame<'a> {
    shell: &'a mut Shell,
}

impl<'a> ModuleStackFrame<'a> {
    fn enter(shell: &'a mut Shell, key: String) -> Self {
        shell.context.modules.stack.push(key);
        Self { shell }
    }
}

impl Drop for ModuleStackFrame<'_> {
    fn drop(&mut self) {
        self.shell.context.modules.stack.pop();
    }
}

/// Compile `source` seeded from the live session's schemes, so a loaded
/// file sees the names already installed.  `virtual_path` is the compile
/// door's script name too, so the file's `$SCRIPT` references bake to it.
///
/// Registers `source` before compiling, so every span the compile stamps,
/// a rejection's among them, resolves in the session's registry.
///
/// Also the binding-lease harvest seam: every runtime-compiled load passes
/// through here inside an already-committed run, so its referenced names
/// count as real uses and renewing them once here covers all of them.
pub(crate) fn check_source(
    source: &str,
    virtual_path: &str,
    shell: &mut Shell,
    contract: Option<crate::typecheck::ReturnContract>,
) -> Settled<Toplevel> {
    let text = Source::from_text(virtual_path, source);
    let file = shell.session.sources.register(text.clone());
    let top = crate::compile::compile_and_typecheck(
        source,
        shell.session_schemes(),
        file,
        virtual_path,
        contract,
    )
    .map_err(|error| error.into_error(text))?;
    if shell.local.bindings.armed() {
        shell.local.bindings.renew(top.referenced_names());
    }
    Ok(top)
}
