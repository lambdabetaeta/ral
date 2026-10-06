//! `use` — load and evaluate a `.ral` module.
//!
//! `use` (§10.3) runs a file under the session environment — every `Define`
//! of the current run already landed, none of the caller's own block-local
//! names — and returns its bindings as a Map.  It is [`builtin_use`], below,
//! which drives [`crate::load::module_phrases`]'s own cycle-detection stack and
//! depth guard: nothing is cached, so those are what keep repeated loads
//! terminating.

use crate::evaluator::Mode;
use crate::load::{ModuleLoad, check_source, module_phrases};
use crate::ty::Site;
use crate::types::{Break, Error, Mooring, Settled, Shell, Value};
use std::sync::Arc;

/// Read and normalise a module's text, located and authorised as one walk
/// (`abs_path`, the path as the caller resolved it).
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:module-load] `use` module loading reads program text from disk, gated by `locate`. The documented reasoned-silent residual: code-loading is visible as its own statement, not turn-time model data I/O, so it raises no surface card."
)]
fn read_and_normalize(abs_path: &str, shell: &mut Shell) -> Settled<String> {
    let rp = shell.resolve(abs_path);
    let located = shell.locate(&rp, &crate::capability::FsOp::Read)?;
    let what = format!("use: {abs_path}");
    let mut file = located.read().map_err(|e| Error::io(&what, &e))?;
    let mut source = String::new();
    std::io::Read::read_to_string(&mut file, &mut source).map_err(|e| Error::io(&what, &e))?;
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

/// `use` stays a native (§10.3): it returns a map and binds nothing, so it
/// needs only *an* environment to run the module's phrases under — the
/// session environment, `Mode::Module`, never the caller's own block-local
/// `E`.  The map it returns is `ran.defined` filtered by the `_` rule, each
/// name read from `ran.env`.
pub(crate) fn builtin_use(
    args: &[Value],
    site: &Arc<Site>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let path = args[0].as_str("use")?.to_owned();
    let resolved = resolve_relative_to_current_script(&path, shell);
    // `use` falls back to a RAL_PATH search for a bare name.
    let abs_path = shell
        .resolve(&resolved.to_string_lossy())
        .canonicalise_strict()
        .ok()
        .or_else(|| crate::path::ral_path::find_file(&path, shell.context.env_overrides()))
        .map_or_else(|| path.clone(), |p| p.to_string_lossy().into_owned());

    let source = read_and_normalize(&abs_path, shell)?;
    let top = check_source(&source, &abs_path, shell, None).map_err(tag_loader_error)?;

    let env = shell.env.clone();
    let load = ModuleLoad {
        top: &top,
        virtual_path: &abs_path,
    };
    // `Mode::Module` never writes `shell.env` (only `Session` does),
    // so the module's own top-level names die with `env` here — no save or
    // restore needed to keep them from leaking into the caller.
    let ran = module_phrases(load, env, Mode::Module, mooring, shell).map_err(tag_loader_error)?;
    ran.outcome.map_err(tag_loader_error)?;
    let bindings: Vec<(String, Value)> = ran
        .defined
        .iter()
        // A leading underscore marks a name the module keeps private.
        .filter(|name| !name.starts_with('_'))
        .filter_map(|name| ran.env.get(name).map(|v| (name.clone(), v.clone())))
        .collect();
    let record = Value::map(bindings);
    site.admit_module(&record, &top.exported_schemes())
        .map_err(|mismatch| mismatch.refusal("use", shell))?;
    Ok(record)
}

/// Resolve `path` against the directory of the innermost load in flight —
/// the top of the stack [`module_phrases`] pushes to — falling back to
/// the run's own root source at top level, where the stack is empty.
fn resolve_relative_to_current_script(path: &str, shell: &Shell) -> std::path::PathBuf {
    let script = shell.context.modules.stack.last().map_or_else(
        || {
            shell
                .session
                .root_file
                .and_then(|file| shell.session.sources.get(file))
                .map_or("", |source| source.name())
        },
        String::as_str,
    );
    crate::path::resolve_relative_to_script(path, script)
}
