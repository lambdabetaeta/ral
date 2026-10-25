//! `use` — load and evaluate a `.ral` module.
//!
//! `use` (§10.3) runs a file under the session environment — every `Define`
//! of the current run already landed, none of the caller's own block-local
//! names — and returns its bindings as a Map.  It is [`builtin_use`], below,
//! which drives [`crate::shell::load::module_phrases`]'s own cycle-detection stack and
//! depth guard: nothing is cached, so those are what keep repeated loads
//! terminating.

use crate::path::{Judged, LexicalPath, Located, Reading};
use crate::shell::evaluator::Mode;
use crate::shell::load::{ModuleLoad, check_source, module_phrases};
use crate::shell::runtime::{Break, Error, Mooring, Settled, Shell, Value, sig};
use crate::source::{SourceFile, SourceOrigin};
use crate::ty::Site;
use std::sync::Arc;

/// Read and normalise the module `located` holds, which the grant judged for
/// reading; `shown` names it in errors.  The handle ends here, by signature.
fn read_and_normalize(located: Located<Judged<Reading>>, shown: &str) -> Settled<String> {
    let what = format!("use: {shown}");
    let read = located.read();
    drop(located);
    let mut file = read.map_err(|e| Error::io(&what, &e))?;
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
    // The name beside the script is the first candidate; the `RAL_PATH` ones
    // follow, each judged before its kind is asked, a denied one passed over
    // as `execvp` does.  Failing every candidate, the target itself says why.
    let candidates = std::iter::once(target.clone()).chain(ral_path_candidates(&path, shell));
    let located = match shell.locate_first::<Reading>(candidates)? {
        Some(found) => found,
        None => shell.locate::<Reading>(&target)?,
    };

    let file = SourceFile::new(located.real().as_path().to_path_buf())
        .map_err(|e| sig(format!("use: {e}")))?;
    let source = read_and_normalize(located, &crate::path::shown(file.path()))?;
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

/// `$dir/$name` over `RAL_PATH`, relative entries read against the shell's
/// cwd.
fn ral_path_candidates(name: &str, shell: &Shell) -> Vec<LexicalPath> {
    let resolver = shell.context.resolver();
    crate::path::ral_path_entries(shell.context.env_overrides())
        .into_iter()
        .map(|dir| resolver.anchor(&dir.join(name)))
        .collect()
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
    match origin.and_then(SourceOrigin::file) {
        Some(script) => resolver
            .resolve_in_script(path, script)
            .map_err(|e| sig(format!("use: {e}"))),
        None => Ok(resolver.resolve(path)),
    }
}
