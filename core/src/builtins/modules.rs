//! `source` and `use` — load and evaluate `.ral` files.
//!
//! Both go through [`evaluate_source`] and differ only in scope and path
//! policy: `source` runs the file in the caller's scope and yields its
//! terminal value; `use` runs it under the session environment (S7) —
//! every `Define` of the current run already landed, none of the caller's
//! own block-local names — and returns its bindings as a Map.  Nothing is
//! cached, so the cycle and depth guards in [`evaluate_checked`] are what
//! keep repeated loads terminating.
//!
//! Every runtime source load funnels through here: the plugin loader and
//! [`crate::capability::load_capabilities_from_str`] call
//! [`evaluate_source`]; the REPL's rc loader, which renders type errors
//! itself, calls [`evaluate_checked`] with an already-checked `Toplevel`.
//! `source`/`use` themselves (§3.3) are [`builtin_source`]/[`builtin_use`],
//! below — `source` is a binder form now (S2), so [`evaluate_checked`] and
//! friends serve every *other* loader (rc, plugin, capability) alone.

use std::path::Path;

use crate::evaluator::{Mode, Ran};
use crate::ir::Toplevel;
use crate::source::Span;
use crate::types::{Break, CommandOrigin, Env, Mooring, Settled, Shell, Value, sig};

use super::util::arg0_str;

const MAX_SOURCE_DEPTH: usize = 100;

/// Evaluate an already-checked `top` under the cycle-detection stack and
/// depth guard, registering `source` under `virtual_path`, in the caller's
/// own scope.
///
/// Errors come back raw; each caller prefixes its own surface name
/// (`source:`, `use:`, `capability file <path>:`).
///
/// # Errors
/// Returns `Err` if `virtual_path` is already on the module stack (a
/// circular dependency), if the depth limit is exceeded, or if evaluating
/// `top` fails.
pub fn evaluate_checked(
    mooring: &Mooring,
    shell: &mut Shell,
    top: &std::sync::Arc<Toplevel>,
    source: &str,
    virtual_path: &str,
) -> Settled<Value> {
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
    shell.install_script_context(&key, source);
    shell.context.modules.stack.push(key);
    let ran = crate::evaluator::run_phrases(
        &top.phrases,
        shell.env.clone(),
        Mode::Local,
        mooring,
        shell,
    );
    shell.context.modules.stack.pop();
    // Unlike a nested `source`/`use`, this loader's whole point is to install
    // its defines into the running session — rc, a plugin, a capability
    // file, exarch's agent library — so its own `Ran::env` lands in
    // `shell.env` here, the one write-back `Mode::Local` itself skips (no
    // lease, no PATH-shadow check: this is host-installed library code, not
    // an interactive `let`).
    shell.env = ran.env;
    ran.outcome
}

/// Parse, elaborate, check, and evaluate `source` under `virtual_path`.
///
/// The path is virtual: the caller owns the filesystem read; this only
/// names the registered source and keys the cycle stack.
///
/// # Errors
/// Returns `Err` if `source` fails to compile, or for any error from
/// [`evaluate_checked`].
pub fn evaluate_source(
    mooring: &Mooring,
    shell: &mut Shell,
    source: &str,
    virtual_path: &str,
) -> Settled<Value> {
    let top = check_source(source, virtual_path, shell)?;
    evaluate_checked(mooring, shell, &top, source, virtual_path)
}

// ── Phrases (§3.3) ──────────────────────────────────────────────────────

/// Load `path`'s text, compile it to a [`Toplevel`], and run its phrases
/// under `mode` — the `source` half of §3.3, mirroring [`evaluate_source`]'s
/// resolution but over phrases rather than one `Comp`.
///
/// # Errors
/// A load refusal — cycle, depth limit, read failure, compile failure —
/// stops before any phrase runs; see [`source_phrases`].
pub(crate) fn source(
    path: &str,
    env: Env,
    mode: Mode,
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Ran> {
    let resolved = resolve_relative_to_current_script(path, shell);
    let abs_path = shell
        .resolve(&resolved.to_string_lossy())
        .canonicalise_strict()
        .map_or_else(
            |_| resolved.to_string_lossy().into_owned(),
            |p| p.to_string_lossy().into_owned(),
        );
    let text = read_and_normalize(&resolved, &abs_path, "source", shell)?;
    let top = compile_toplevel(&text, &abs_path, shell).map_err(|e| tag_loader_error("source", e))?;
    let load = ModuleLoad {
        top: &top,
        virtual_path: &abs_path,
        source_text: &text,
        span,
    };
    let ran = source_phrases(load, env, mode, mooring, shell).map_err(|e| tag_loader_error("source", e))?;
    Ok(Ran {
        outcome: ran.outcome.map_err(|e| tag_loader_error("source", e)),
        ..ran
    })
}

/// Elaborate and typecheck `source_text` into a [`Toplevel`], seeded from
/// the live session's schemes.  The `FileId` is peeked, not minted, on the
/// same promise [`check_source`] relies on: [`source_phrases`]'s
/// `install_script_context` call, a moment later, is the one registration
/// in between.  Also the binding-lease harvest seam, mirroring
/// [`check_source`]: every name the file references counts as a real use.
fn compile_toplevel(
    source_text: &str,
    virtual_path: &str,
    shell: &mut Shell,
) -> Settled<Toplevel> {
    let file = shell.session.sources.next_id();
    let ast = crate::syntax::parser::parse_with(source_text, file).map_err(|e| sig(e.to_string()))?;
    let bindings = shell
        .session_schemes()
        .bindings
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let top = crate::elaborator::elaborate(&ast, bindings, virtual_path)
        .map_err(|e| sig(e.to_string()))?;
    let top = crate::typecheck::typecheck(&top, shell.session_schemes()).map_err(|errs| {
        sig(errs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"))
    })?;
    if shell.local.bindings.armed() {
        shell.local.bindings.renew(top.referenced_names());
    }
    Ok(top)
}

/// Run `top`'s phrases under `mode`, guarded by the same cycle/depth checks
/// and module-stack bookkeeping [`evaluate_checked`] applies; the call is
/// recorded in the audit trail as a native's is
/// ([`crate::evaluator::audit::frame_call`]), `span` standing in for the
/// call site a form has no argument list to carry.
///
/// # Errors
/// A circular dependency or a depth-limit refusal — before any phrase runs.
/// A phrase that halts is `Ran::outcome`, not this `Err`: `source` is not
/// transactional (S12), so every caller threads `Ran::env` before it
/// propagates `Ran::outcome`.
#[derive(Clone, Copy)]
pub(crate) struct ModuleLoad<'a> {
    pub top: &'a Toplevel,
    pub virtual_path: &'a str,
    pub source_text: &'a str,
    pub span: Option<Span>,
}

pub(crate) fn source_phrases(
    load: ModuleLoad<'_>,
    env: Env,
    mode: Mode,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Ran> {
    let ModuleLoad {
        top,
        virtual_path,
        source_text,
        span,
    } = load;
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
    shell.install_script_context(&key, source_text);
    shell.local.audit.call_site = span;
    let frame = ModuleStackFrame::enter(shell, key);
    let mut ran_slot = None;
    let _ = crate::evaluator::audit::frame_call(
        "source",
        &[Value::String(virtual_path.to_string())],
        CommandOrigin::Builtin,
        mooring,
        frame.shell,
        |shell, _frame| {
            let ran = crate::evaluator::run_phrases(&top.phrases, env, mode, mooring, shell);
            let outcome = match &ran.outcome {
                Ok(_) => Ok(Value::Unit),
                Err(e) => Err(e.clone()),
            };
            ran_slot = Some(ran);
            outcome
        },
    );
    drop(frame);
    Ok(ran_slot.expect("frame_call always invokes its body exactly once"))
}

/// Pops the module stack on `Drop`, panic included — the guard [`source_phrases`] promised.
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
/// The [`FileId`](crate::source::FileId) is peeked, not minted: the
/// registration [`evaluate_checked`] performs a moment later lands on it,
/// and nothing else registers a source in between.
///
/// Also the binding-lease harvest seam: every runtime-compiled load passes
/// through here inside an already-committed run, so its referenced names
/// count as real uses and renewing them once here covers all of them.
fn check_source(
    source: &str,
    virtual_path: &str,
    shell: &mut Shell,
) -> Settled<std::sync::Arc<Toplevel>> {
    let file = shell.session.sources.next_id();
    let top = crate::compile_and_typecheck(source, shell.session_schemes(), file, virtual_path)
        .into_comp_or_message()
        .map(std::sync::Arc::new)
        .map_err(sig)?;
    if shell.local.bindings.armed() {
        shell.local.bindings.renew(top.referenced_names());
    }
    Ok(top)
}

/// Read and normalise a module's text.  `check_fs_read` and the errors key
/// on `abs_path`, the path as the caller resolved it; `who` names the verb.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:module-load] `source`/`use` module loading reads program text from disk, gated by `check_fs_read`. The documented reasoned-silent residual: code-loading is visible as its own statement, not turn-time model data I/O, so it raises no surface card."
)]
fn read_and_normalize(
    resolved: &Path,
    abs_path: &str,
    who: &str,
    shell: &mut Shell,
) -> Settled<String> {
    let rp = shell.resolve(abs_path);
    shell.check_fs_read(&rp)?;
    let source = std::fs::read_to_string(resolved).map_err(|e| {
        sig(match e.kind() {
            std::io::ErrorKind::NotFound => format!("{who}: {abs_path}: not found"),
            std::io::ErrorKind::PermissionDenied => format!("{who}: {abs_path}: permission denied"),
            _ => format!("{who}: {abs_path}: {e}"),
        })
    })?;
    Ok(crate::source::normalize_source_text(source))
}

/// Prefix `who` onto a loader failure while keeping its status: a sourced
/// `fail [status: 7]` must reach the caller's handler as 7, not the `1` a
/// fresh `sig` imposes.  Idempotent, so `source` in `source` does not stack.
fn tag_loader_error(who: &str, e: Break) -> Break {
    match e {
        Break::Error(mut err) => {
            let prefix = format!("{who}: ");
            if !err.message.starts_with(&prefix) {
                err.message = format!("{prefix}{}", err.message);
            }
            Break::Error(err)
        }
        other @ Break::Escape(_) => other,
    }
}

/// `source` is a form, elaborated straight to `Phrase::Source`/`CompKind::Source`
/// from a bare unbound `source path` — one argument, no redirects (S2).  Any
/// other call shape reaches this table entry instead, which stays only for
/// `help` and typing: a native has no lexical environment to extend, so it
/// cannot itself define anything.
pub(super) fn builtin_source(
    _args: &[Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<Value> {
    Err(sig(
        "`source` is a form, not a function: write `source path` as its own statement",
    ))
}

/// `use` stays a native (§3.3, S7): it returns a map and binds nothing, so it
/// needs only *an* environment to run the module's phrases under — the
/// session environment, `Mode::Module`, never the caller's own block-local
/// `E`.  The map it returns is `ran.defined` filtered by the `_` rule, each
/// name read from `ran.env`.
pub(crate) fn builtin_use(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let path = arg0_str(args);
    let resolved = resolve_relative_to_current_script(&path, shell);
    // Unlike `source`, `use` falls back to a RAL_PATH search for a bare name.
    let abs_path = shell
        .resolve(&resolved.to_string_lossy())
        .canonicalise_strict()
        .ok()
        .or_else(|| crate::path::ral_path::find_file(&path, shell.context.env_overrides()))
        .map_or_else(|| path.clone(), |p| p.to_string_lossy().into_owned());

    let source = read_and_normalize(abs_path.as_ref(), &abs_path, "use", shell)?;
    let top = compile_toplevel(&source, &abs_path, shell).map_err(|e| tag_loader_error("use", e))?;

    let env = shell.env.clone();
    let load = ModuleLoad {
        top: &top,
        virtual_path: &abs_path,
        source_text: &source,
        span: None,
    };
    // `Mode::Module` never writes `shell.env` (only `Session` does, §3.2),
    // so the module's own top-level names die with `env` here — no save or
    // restore needed to keep them from leaking into the caller.
    let ran = source_phrases(load, env, Mode::Module, mooring, shell);

    let ran = ran.map_err(|e| tag_loader_error("use", e))?;
    ran.outcome
        .map(|_| {
            let bindings: Vec<(String, Value)> = ran
                .defined
                .iter()
                // A leading underscore marks a name the module keeps private.
                .filter(|name| !name.starts_with('_'))
                .filter_map(|name| ran.env.get(name).map(|v| (name.clone(), v.clone())))
                .collect();
            Value::map(bindings)
        })
        .map_err(|e| tag_loader_error("use", e))
}

/// Resolve `path` against the directory of the innermost load in flight —
/// the top of the stack [`evaluate_checked`] pushes to — falling back to
/// the run's own root source at top level, where the stack is empty.
fn resolve_relative_to_current_script(path: &str, shell: &Shell) -> std::path::PathBuf {
    let script = shell.context.modules.stack.last().map_or_else(
        || {
            shell
                .session
                .sources
                .get(shell.session.root_file)
                .map_or("", crate::source::Source::name)
        },
        String::as_str,
    );
    crate::path::resolve_relative_to_script(path, script)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    /// Write `contents` to a fresh temp `.ral` file and return its path —
    /// the caller removes it; mirrors `core/tests/module_loader.rs`'s
    /// `write_module` for these crate-internal, direct-API tests.
    fn write_temp(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, contents).expect("write temp module");
        path
    }

    /// [`source`] end to end: `compile_toplevel` through the real
    /// `elaborate`/`typecheck`, `run_phrases` over the result.
    #[test]
    fn source_runs_a_files_defines_and_reports_unit() {
        let path = write_temp("ral_w1d_source_ok.ral", "let sourced_ok = 5");
        let p = path.to_string_lossy().into_owned();
        let mut shell = Shell::default();
        let env = shell.env.clone();
        let ran = source(&p, env, Mode::Local, None, &Mooring::adrift(), &mut shell)
            .expect("a well-formed file must load");
        assert!(
            matches!(ran.outcome, Ok(Value::Unit)),
            "`source`'s own value is Unit (S12)"
        );
        assert_eq!(ran.defined, vec!["sourced_ok".to_string()]);
        assert_eq!(ran.env.get("sourced_ok"), Some(&Value::Int(5)));
        std::fs::remove_file(&path).ok();
    }

    /// A file that halts midway is `Ran::outcome`, not the door's `Err`
    /// (S12): the `Define`s before the halt are still in `Ran::env`.
    #[test]
    fn source_halting_midway_keeps_the_defines_before_the_halt() {
        let path = write_temp(
            "ral_w1d_source_halt.ral",
            "let sourced_before = 1\nexit 9\nlet sourced_after = 2",
        );
        let p = path.to_string_lossy().into_owned();
        let mut shell = Shell::default();
        let env = shell.env.clone();
        let ran = source(&p, env, Mode::Local, None, &Mooring::adrift(), &mut shell)
            .expect("a load refusal is a different Err; the file itself loaded fine");
        assert!(ran.outcome.is_err(), "the file's own halt is Ran::outcome");
        assert_eq!(ran.defined, vec!["sourced_before".to_string()]);
        assert_eq!(ran.env.get("sourced_before"), Some(&Value::Int(1)));
        assert!(ran.env.get("sourced_after").is_none());
        std::fs::remove_file(&path).ok();
    }
}
