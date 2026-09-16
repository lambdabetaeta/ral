//! `use` — load and evaluate a `.ral` module.
//!
//! `use` (§10.4) runs a file under the session environment — every `Define`
//! of the current run already landed, none of the caller's own block-local
//! names — and returns its bindings as a Map.  It is [`builtin_use`], below,
//! which drives [`module_phrases`]'s own cycle-detection stack and depth
//! guard: nothing is cached, so those are what keep repeated loads
//! terminating.
//!
//! [`evaluate_checked`]/[`evaluate_source`] are the sibling door for every
//! *other* runtime load: [`crate::capability::load_capabilities_from_str`]
//! and the plugin loader call [`evaluate_source`], the plugin loader with
//! its manifest contract; the REPL's rc loader, which renders type errors
//! itself, calls [`evaluate_checked`] with an already-checked `Toplevel`.

use crate::evaluator::{Mode, Ran};
use crate::ir::Toplevel;
use crate::types::{Break, Env, Mooring, Settled, Shell, Value, sig};

use super::util::arg0_str;

const MAX_SOURCE_DEPTH: usize = 100;

/// Evaluate an already-checked `top` under the cycle-detection stack and
/// depth guard, registering its `source` text under `virtual_path`, in the
/// caller's own scope.
///
/// Errors come back raw; each caller prefixes its own surface name
/// (`use:`, `capability file <path>:`).
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
    let load = ModuleLoad {
        top,
        virtual_path,
        source_text: source,
    };
    let ran = module_phrases(load, shell.env.clone(), Mode::Local, mooring, shell)?;
    // Unlike a nested `use`, this loader's whole point is to install its
    // defines into the running session — rc, a plugin, a capability file,
    // exarch's agent library — so its own `Ran::env` lands in `shell.env`
    // here, the one write-back `Mode::Local` itself skips (no lease, no
    // PATH-shadow check: this is host-installed library code, not an
    // interactive `let`).
    shell.env = ran.env;
    ran.outcome
}

/// Parse, elaborate, check, and evaluate `source` under `virtual_path`.
///
/// The path is virtual: the caller owns the filesystem read; this only
/// names the registered source and keys the cycle stack.  `contract` is the
/// loading form's hold on `source`'s own returned literal map — the plugin
/// loader's door, for a manifest's fields — held in the same check as the
/// rest of the file.
///
/// # Errors
/// Returns `Err` if `source` fails to compile, breaks `contract`, or for any
/// error from [`evaluate_checked`].
pub fn evaluate_source(
    mooring: &Mooring,
    shell: &mut Shell,
    source: &str,
    virtual_path: &str,
    contract: Option<crate::typecheck::ReturnContract>,
) -> Settled<Value> {
    let top = check_source(source, virtual_path, shell, contract)?;
    evaluate_checked(mooring, shell, &top, source, virtual_path)
}

// ── Phrases (§10.4) ─────────────────────────────────────────────────────

/// Elaborate and typecheck `source_text` into a [`Toplevel`], seeded from
/// the live session's schemes.  The `FileId` is peeked, not minted, on the
/// same promise [`check_source`] relies on: [`module_phrases`]'s
/// `install_script_context` call, a moment later, is the one registration
/// in between.  Also the binding-lease harvest seam, mirroring
/// [`check_source`]: every name the file references counts as a real use.
fn compile_toplevel(source_text: &str, virtual_path: &str, shell: &mut Shell) -> Settled<Toplevel> {
    let file = shell.session.sources.next_id();
    let ast =
        crate::syntax::parser::parse_with(source_text, file).map_err(|e| sig(e.to_string()))?;
    let bindings = shell
        .session_schemes()
        .bindings
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let top = crate::elaborator::elaborate(&ast, bindings, virtual_path)
        .map_err(|e| sig(e.to_string()))?;
    let top = crate::typecheck::typecheck(&top, shell.session_schemes(), None).map_err(|errs| {
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

/// A compiled module ready to run: what both loaders hand [`module_phrases`].
#[derive(Clone, Copy)]
struct ModuleLoad<'a> {
    top: &'a Toplevel,
    virtual_path: &'a str,
    source_text: &'a str,
}

/// Run `load`'s phrases under `mode`, guarded by the cycle check, the depth
/// limit and the module-stack frame: the one door every runtime load goes
/// through, `use` and [`evaluate_checked`] alike.  `use` is a native, so
/// [`command_call::run_host_thunk`](crate::runtime::command_call) already
/// records its own audit frame; this records nothing further.
///
/// # Errors
/// A circular dependency or a depth-limit refusal — before any phrase runs.
/// A phrase that halts is `Ran::outcome`, not this `Err`: a module is not
/// transactional (`docs/SPEC.md` §5.6), so every caller threads `Ran::env`
/// before it propagates `Ran::outcome`.
fn module_phrases(
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
/// The [`FileId`](crate::source::FileId) is peeked, not minted: the
/// registration [`module_phrases`] performs a moment later lands on it, and
/// nothing else registers a source in between.
///
/// Also the binding-lease harvest seam: every runtime-compiled load passes
/// through here inside an already-committed run, so its referenced names
/// count as real uses and renewing them once here covers all of them.
fn check_source(
    source: &str,
    virtual_path: &str,
    shell: &mut Shell,
    contract: Option<crate::typecheck::ReturnContract>,
) -> Settled<std::sync::Arc<Toplevel>> {
    let file = shell.session.sources.next_id();
    let top = crate::compile_and_typecheck(
        source,
        shell.session_schemes(),
        file,
        virtual_path,
        contract,
    )
    .into_comp_or_message()
    .map(std::sync::Arc::new)
    .map_err(sig)?;
    if shell.local.bindings.armed() {
        shell.local.bindings.renew(top.referenced_names());
    }
    Ok(top)
}

/// Read and normalise a module's text, located and authorised as one walk
/// (`abs_path`, the path as the caller resolved it).
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:module-load] `use` module loading reads program text from disk, gated by `locate`. The documented reasoned-silent residual: code-loading is visible as its own statement, not turn-time model data I/O, so it raises no surface card."
)]
fn read_and_normalize(abs_path: &str, shell: &mut Shell) -> Settled<String> {
    let rp = shell.resolve(abs_path);
    let located = shell.locate(&rp, &crate::capability::FsOp::Read)?;
    let mut file = located.read().map_err(|e| {
        sig(match e.kind() {
            std::io::ErrorKind::NotFound => format!("use: {abs_path}: not found"),
            std::io::ErrorKind::PermissionDenied => format!("use: {abs_path}: permission denied"),
            _ => format!("use: {abs_path}: {e}"),
        })
    })?;
    let mut source = String::new();
    std::io::Read::read_to_string(&mut file, &mut source)
        .map_err(|e| sig(format!("use: {abs_path}: {e}")))?;
    Ok(crate::source::normalize_source_text(source))
}

/// Prefix `use:` onto a loader failure while keeping its status: a `fail
/// [status: 7]` inside a used module must reach the caller's handler as 7,
/// not the `1` a fresh `sig` imposes.  Idempotent, so a nested `use` does not
/// stack the prefix.
fn tag_loader_error(e: Break) -> Break {
    match e {
        Break::Error(mut err) => {
            if !err.message.starts_with("use: ") {
                err.message = format!("use: {}", err.message);
            }
            Break::Error(err)
        }
        other @ Break::Escape(_) => other,
    }
}

/// `use` stays a native (§10.4, S7): it returns a map and binds nothing, so it
/// needs only *an* environment to run the module's phrases under — the
/// session environment, `Mode::Module`, never the caller's own block-local
/// `E`.  The map it returns is `ran.defined` filtered by the `_` rule, each
/// name read from `ran.env`.
pub(crate) fn builtin_use(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let path = arg0_str(args);
    let resolved = resolve_relative_to_current_script(&path, shell);
    // `use` falls back to a RAL_PATH search for a bare name.
    let abs_path = shell
        .resolve(&resolved.to_string_lossy())
        .canonicalise_strict()
        .ok()
        .or_else(|| crate::path::ral_path::find_file(&path, shell.context.env_overrides()))
        .map_or_else(|| path.clone(), |p| p.to_string_lossy().into_owned());

    let source = read_and_normalize(&abs_path, shell)?;
    let top = compile_toplevel(&source, &abs_path, shell).map_err(tag_loader_error)?;

    let env = shell.env.clone();
    let load = ModuleLoad {
        top: &top,
        virtual_path: &abs_path,
        source_text: &source,
    };
    // `Mode::Module` never writes `shell.env` (only `Session` does, §3.2),
    // so the module's own top-level names die with `env` here — no save or
    // restore needed to keep them from leaking into the caller.
    let ran = module_phrases(load, env, Mode::Module, mooring, shell);

    let ran = ran.map_err(tag_loader_error)?;
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
        .map_err(tag_loader_error)
}

/// Resolve `path` against the directory of the innermost load in flight —
/// the top of the stack [`module_phrases`] pushes to — falling back to
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
