//! `source` and `use` — load and evaluate `.ral` files.
//!
//! Both go through [`evaluate_source`] and differ only in scope and path
//! policy: `source` runs the file in the caller's scope and yields its
//! terminal value; `use` runs it in a fresh top scope and returns its
//! bindings as a Map.  Nothing is cached, so the cycle and depth guards in
//! [`evaluate_checked`] are what keep repeated loads terminating.
//!
//! Every runtime source load funnels through here: the plugin loader and
//! [`crate::capability::load_capabilities_from_str`] call
//! [`evaluate_source`]; the REPL's rc loader, which renders type errors
//! itself, calls [`evaluate_checked`] with an already-checked `Comp`.

use std::path::Path;

use crate::ir::Comp;
use crate::types::{Break, Mooring, Settled, Shell, Value, sig};

use super::util::arg0_str;

const MAX_SOURCE_DEPTH: usize = 100;

/// Evaluate an already-checked `comp` under the cycle-detection stack and
/// depth guard, registering `source` under `virtual_path`.
///
/// Errors come back raw; each caller prefixes its own surface name
/// (`source:`, `use:`, `capability file <path>:`).
///
/// # Errors
/// Returns `Err` if `virtual_path` is already on the module stack (a
/// circular dependency), if the depth limit is exceeded, or if evaluating
/// `comp` fails.
pub fn evaluate_checked(
    mooring: &Mooring,
    shell: &mut Shell,
    comp: &std::sync::Arc<Comp>,
    source: &str,
    virtual_path: &str,
) -> Settled<Value> {
    let key = virtual_path.to_string();
    if shell.mobile.context.modules.stack.contains(&key) {
        let cycle: Vec<&str> = shell
            .mobile
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
    if shell.mobile.context.modules.stack.len() >= MAX_SOURCE_DEPTH {
        return Err(sig(format!(
            "recursion depth limit ({MAX_SOURCE_DEPTH}) exceeded"
        )));
    }
    shell.install_script_context(&key, source);
    shell.mobile.context.modules.stack.push(key);
    let result = crate::evaluate(comp, mooring, shell);
    shell.mobile.context.modules.stack.pop();
    result
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
    let comp = check_source(source, virtual_path, shell)?;
    evaluate_checked(mooring, shell, &comp, source, virtual_path)
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
) -> Settled<std::sync::Arc<Comp>> {
    let file = shell.session.sources.next_id();
    let comp = crate::compile_and_typecheck(source, shell.session_schemes(), file, virtual_path)
        .into_comp_or_message()
        .map(std::sync::Arc::new)
        .map_err(sig)?;
    if shell.local.bindings.armed() {
        shell
            .local
            .bindings
            .renew(crate::ir::referenced_names(&comp));
    }
    Ok(comp)
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

pub(super) fn builtin_source(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let path = arg0_str(args);
    let resolved = resolve_relative_to_current_script(&path, shell);
    let abs_path = shell
        .resolve(&resolved.to_string_lossy())
        .canonicalise_strict()
        .map_or_else(
            |_| resolved.to_string_lossy().into_owned(),
            |p| p.to_string_lossy().into_owned(),
        );
    let source = read_and_normalize(&resolved, &abs_path, "source", shell)?;
    evaluate_source(mooring, shell, &source, &abs_path).map_err(|e| tag_loader_error("source", e))
}

pub(crate) fn builtin_use(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let path = arg0_str(args);
    let resolved = resolve_relative_to_current_script(&path, shell);
    // Unlike `source`, `use` falls back to a RAL_PATH search for a bare name.
    let abs_path = shell
        .resolve(&resolved.to_string_lossy())
        .canonicalise_strict()
        .ok()
        .or_else(|| crate::path::ral_path::find_file(&path, shell.mobile.context.env_overrides()))
        .map_or_else(|| path.clone(), |p| p.to_string_lossy().into_owned());

    let source = read_and_normalize(abs_path.as_ref(), &abs_path, "use", shell)?;

    shell.mobile.scope.push_scope();
    let evaluated = evaluate_source(mooring, shell, &source, &abs_path);
    let result = evaluated.map(|_| {
        let bindings: Vec<(String, Value)> = shell
            .mobile
            .scope
            .top_scope()
            .iter()
            // A leading underscore marks a name the module keeps private.
            .filter(|(k, _)| !k.starts_with('_'))
            .map(|(k, b)| (k.clone(), b.value.clone()))
            .collect();
        Value::map(bindings)
    });
    shell.mobile.scope.pop_scope();
    result.map_err(|e| tag_loader_error("use", e))
}

/// Resolve `path` against the directory of the innermost load in flight —
/// the top of the stack [`evaluate_checked`] pushes to — falling back to
/// the run's own root source at top level, where the stack is empty.
fn resolve_relative_to_current_script(path: &str, shell: &Shell) -> std::path::PathBuf {
    let script = shell.mobile.context.modules.stack.last().map_or_else(
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
