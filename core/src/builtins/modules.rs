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
use crate::source::{SourceFile, SourceOrigin};
use crate::ty::Site;
use crate::types::{Break, Error, Mooring, Settled, Shell, Value, sig};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Read and normalise the module at `path`, located and authorised as one
/// walk; `shown` names it in errors.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:module-load] `use` module loading reads program text from disk, gated by `locate`. The documented reasoned-silent residual: code-loading is visible as its own statement, not turn-time model data I/O, so it raises no surface card."
)]
fn read_and_normalize(path: &Path, shown: &str, shell: &mut Shell) -> Settled<String> {
    let rp = shell.anchor(path);
    let located = shell.locate(&rp, &crate::capability::FsOp::Read)?;
    let what = format!("use: {shown}");
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
    let target = resolve_in_current_script(&path, shell)?;
    // `use` falls back to a RAL_PATH search for a bare name, and failing
    // both reads the target itself, to report why it cannot.
    let file = target
        .canonicalise_strict()
        .ok()
        .or_else(|| find_on_ral_path(&path, shell))
        .unwrap_or_else(|| target.into_inner());

    let file = SourceFile::new(file).map_err(|e| sig(format!("use: {e}")))?;
    let source = read_and_normalize(file.path(), &crate::path::shown(file.path()), shell)?;
    let origin = SourceOrigin::File(file);
    let top = check_source(&source, &origin, shell, None).map_err(tag_loader_error)?;

    let env = shell.env.clone();
    let load = ModuleLoad {
        top: &top,
        origin: &origin,
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
    site.admit_module(&record, &top.exported_schemes(), shell.fixings())
        .map_err(|mismatch| mismatch.refusal("use", shell))?;
    Ok(record)
}

/// The first `$dir/$name` over `RAL_PATH`, canonical, that is a regular file;
/// relative entries read against the shell's cwd.  A directory of that name
/// does not end the walk, or it would shadow a real file further down.
fn find_on_ral_path(name: &str, shell: &Shell) -> Option<PathBuf> {
    let resolver = shell.context.resolver();
    crate::path::ral_path_entries(shell.context.env_overrides())
        .into_iter()
        .find_map(|dir| {
            resolver
                .anchor(&dir.join(name))
                .canonicalise_strict()
                .ok()
                .filter(|p| p.is_file())
        })
}

/// Resolve `path` as written in the innermost load in flight — the top of
/// the stack [`module_phrases`] pushes to — falling back to the run's own
/// root source at top level, where the stack is empty.
///
/// A file source reads `path` beside itself
/// ([`crate::path::Resolver::resolve_in_script`]); a virtual one has no
/// directory, so `path` resolves against the cwd.
///
/// # Errors
/// When the source's directory is no working directory, against which no
/// path can be read.
fn resolve_in_current_script(path: &str, shell: &Shell) -> Settled<crate::path::LexicalPath> {
    let origin = shell.context.modules.stack.last().or_else(|| {
        shell
            .session
            .root_file
            .and_then(|file| shell.session.sources.get(file))
            .map(crate::source::Source::origin)
    });
    let resolver = shell.context.resolver();
    match origin.and_then(SourceOrigin::file).and_then(Path::parent) {
        Some(dir) => resolver
            .resolve_in_script(path, dir)
            .map_err(|e| sig(format!("use: {e}"))),
        None => Ok(resolver.resolve(path)),
    }
}
