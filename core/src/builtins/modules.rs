//! `source` and `use` — load and evaluate `.ral` files.
//!
//! Both run the file through [`evaluate_source`], the shared guarded
//! parse + elaborate + evaluate core, and differ only in scope and path
//! policy: `source` runs the file in the caller's scope (its bindings
//! leak) and yields the file's terminal value; `use` runs it under a
//! fresh top scope and projects the resulting bindings into a Map.  Module
//! loads carry no cache — each call re-evaluates the file fresh, so the
//! cycle and depth guards in `evaluate_source` are what keep re-evaluation
//! terminating.
//!
//! `ScriptContextGuard` swaps the script context for the file's path
//! (script name + source-text cache) and restores it on drop — the
//! same primitive used by `_plugin 'load'`.
//!
//! `evaluate_source` is also the single pipeline used by the
//! capability-file loader (`crate::capability::load`) and the REPL plugin
//! loader.  Callers that need to typecheck with their own error surface
//! ahead of evaluation — the REPL's rc-config loader, which renders type
//! errors through ariadne — call [`evaluate_checked`] directly with their
//! own already-checked `Comp`, still sharing the guarded evaluate step.
//! No second evaluate sequence exists in the tree.

use std::path::Path;

use crate::ir::Comp;
use crate::source::{FileId, Source};
use crate::types::{Shell, Settled, Value, sig, Break};

use super::util::arg0_str;

const MAX_SOURCE_DEPTH: usize = 100;

/// Save the script context (script name on both `script` and
/// `call_site.script`, plus the `source`-text cache used for
/// byte-span → (line, col) resolution),
///
/// swap to the loaded file's
/// for the guard's lifetime, and restore on drop.
///
/// `?`-returns from
/// the body roll the swap back automatically.  Shared between
/// `source`, `use`, and the REPL plugin loader.
///
/// Restoring the `source` matters even though `script` swap alone
/// would re-label diagnostics: while the loaded module evaluates,
/// `comp.rs` resolves spans against `location.source`.  Leaving the
/// parent's source in place there would convert the child's byte
/// offsets against the wrong text, producing wrong line/col on any
/// error raised inside the loaded module.
pub struct ScriptContextGuard<'a> {
    shell: &'a mut Shell,
    saved_current_script: String,
    saved_call_site_script: String,
    saved_source: Option<Source>,
    saved_current: FileId,
}

impl<'a> ScriptContextGuard<'a> {
    pub fn enter(shell: &'a mut Shell, script: &str, text: &str) -> Self {
        let saved_current_script = shell.turn.loc.script.clone();
        let saved_call_site_script = shell.turn.loc.call_site.script.clone();
        let saved_source = shell.turn.loc.source.take();
        let saved_current = shell.turn.loc.current;
        shell.install_script_context(script.to_string(), text);
        Self {
            shell,
            saved_current_script,
            saved_call_site_script,
            saved_source,
            saved_current,
        }
    }

    pub fn shell_mut(&mut self) -> &mut Shell {
        self.shell
    }
}

impl Drop for ScriptContextGuard<'_> {
    fn drop(&mut self) {
        self.shell.turn.loc.script = std::mem::take(&mut self.saved_current_script);
        self.shell.turn.loc.call_site.script = std::mem::take(&mut self.saved_call_site_script);
        self.shell.turn.loc.source = self.saved_source.take();
        self.shell.turn.loc.current = self.saved_current;
    }
}

/// Run an already-checked `comp` under a `ScriptContextGuard` keyed on
/// `virtual_path`, sharing the cycle-detection stack and recursion-depth
/// guard with [`evaluate_source`].
///
/// Split out for callers that need their
/// own compile/typecheck error surface ahead of evaluation — the REPL's
/// rc-config loader reports type errors through ariadne before ever
/// reaching this point — but still want the guarded evaluate every other
/// source-evaluation path shares.
///
/// Errors are returned raw — callers add their own surface prefix so the
/// same machinery can serve every loader without baking one caller's
/// identity into the others.
///
/// # Errors
/// Returns `Err` if `virtual_path` is already on the module stack (a
/// circular dependency), if the recursion-depth limit is exceeded, or if
/// evaluating `comp` fails.
pub fn evaluate_checked(
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
    if shell.mobile.context.modules.depth >= MAX_SOURCE_DEPTH {
        return Err(sig(format!(
            "recursion depth limit ({MAX_SOURCE_DEPTH}) exceeded"
        )));
    }
    let mut ctx = ScriptContextGuard::enter(shell, virtual_path, source);
    ctx.shell_mut().mobile.context.modules.stack.push(key);
    ctx.shell_mut().mobile.context.modules.depth += 1;
    let result = crate::evaluate(comp, ctx.shell_mut());
    ctx.shell_mut().mobile.context.modules.depth -= 1;
    ctx.shell_mut().mobile.context.modules.stack.pop();
    result
}

/// Parse + elaborate + evaluate `source` under a `ScriptContextGuard`
/// keyed on `virtual_path`.
///
/// The path is virtual in the sense that the
/// caller is responsible for any filesystem read; this function only
/// uses it to label `shell.turn.loc.script` (so error messages and
/// nested `source`/`use` resolutions point back to the right file) and
/// to participate in the cycle-detection stack.
///
/// Errors are returned raw — callers add their own surface prefix
/// (`source:`, `use:`, `capability file <path>:`) so the same machinery
/// can serve every loader without baking one caller's identity into
/// the others.
///
/// # Errors
/// Returns `Err` if parsing, elaboration, or typechecking `source` fails,
/// or if the subsequent guarded evaluation fails (see
/// [`evaluate_checked`]).
pub fn evaluate_source(shell: &mut Shell, source: &str, virtual_path: &str) -> Settled<Value> {
    let comp = check_source(source, shell)?;
    evaluate_checked(shell, &comp, source, virtual_path)
}

/// Parse, elaborate, and check `source` against the live session before
/// it evaluates.  The inference pass is what writes the evaluator's mode
/// wires, so a loaded file passes through the checker exactly as a REPL
/// turn does.  A type error is fatal, surfaced as a `Break::Error` so the
/// file is reported and not run.  The schemes seed the check from the
/// caller's scope, so a `source`d file sees the names already installed.
///
/// Peeks the [`FileId`] the module's own registration
/// ([`ScriptContextGuard::enter`], right after this returns) will mint, so
/// the compiled program's spans carry the module's real file identity —
/// nothing else registers a source into the session between the peek and
/// that registration.
///
/// Also the shared harvest seam for the binding-lease ledger
/// (`decisions/260629_agent-binding-reaping`): every runtime-compiled load —
/// `source`, `use`, capability files, plugin loads — funnels through here,
/// so renewing the compiled program's referenced names once, right here,
/// covers all of them. The load is executing inside an already-committed
/// turn, so its references are real uses, unlike the turn-boundary tick
/// which this door does not touch.
fn check_source(source: &str, shell: &mut Shell) -> Settled<std::sync::Arc<Comp>> {
    let file = shell.session.sources.next_id();
    let comp = crate::compile_and_typecheck(source, shell.session_schemes(), file)
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

/// Read `resolved` from disk and normalise its line endings, the byte
/// preparation both `source` and `use` need before [`evaluate_source`].
/// `check_fs_read` and the error messages are keyed on `abs_path` (the
/// path as the caller resolved it); `who` (`"source"` / `"use"`) prefixes
/// the surfaced errors so each verb keeps its own identity.
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

/// Tag a loader failure with `who` while preserving its status, location,
/// and hint — status is the failure-propagation currency, so a sourced
/// `fail [status: 7]` must reach the caller's handler as status 7, not the
/// `1` a fresh `sig` would impose.  Skips the prefix when the message
/// already carries it, so a `source` inside a `source` does not stack
/// `source: source:`.
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

pub(super) fn builtin_source(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let path = arg0_str(args, "source")?;
    let resolved = resolve_relative_to_current_script(&path, shell);
    let abs_path = shell
        .resolve(&resolved.to_string_lossy())
        .canonicalise_strict()
        .map_or_else(
            |_| resolved.to_string_lossy().into_owned(),
            |p| p.to_string_lossy().into_owned(),
        );
    let source = read_and_normalize(&resolved, &abs_path, "source", shell)?;
    evaluate_source(shell, &source, &abs_path).map_err(|e| tag_loader_error("source", e))
}

pub(crate) fn builtin_use(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let path = arg0_str(args, "use")?;
    let resolved = resolve_relative_to_current_script(&path, shell);
    let abs_path = shell
        .resolve(&resolved.to_string_lossy())
        .canonicalise_strict()
        .ok()
        .or_else(|| crate::path::ral_path::find_file(&path))
        .map_or_else(|| path.clone(), |p| p.to_string_lossy().into_owned());

    let source = read_and_normalize(abs_path.as_ref(), &abs_path, "use", shell)?;

    shell.mobile.scope.push_scope();
    let evaluated = evaluate_source(shell, &source, &abs_path);
    let result = evaluated.map(|_| {
        let bindings: Vec<(String, Value)> = shell
            .mobile
            .scope
            .top_scope()
            .iter()
            .filter(|(k, _)| !k.starts_with('_'))
            .map(|(k, b)| (k.clone(), b.value.clone()))
            .collect();
        Value::map(bindings)
    });
    shell.mobile.scope.pop_scope();
    result.map_err(|e| tag_loader_error("use", e))
}

fn resolve_relative_to_current_script(path: &str, shell: &Shell) -> std::path::PathBuf {
    crate::path::resolve_relative_to_script(path, shell.turn.loc.script.as_str())
}
