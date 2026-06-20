//! RC file discovery, parsing, and application.
//!
//! An rc file is ral source whose return value is a map.  Recognised keys
//! map to REPL state: `env`, `prompt`, `bindings`, `aliases`, `edit_mode`,
//! `bell`, `surface`, `recursion_limit`, `plugins`, `startup`, `theme`.
//! Unknown keys are silently ignored so future versions can add knobs without
//! breaking older configs.

use ral_core::{Map, Shell, Value};

use super::frontend::Surface;
use super::theme::{OutputTheme, named_color, set_output_theme};
use rustyline::config::{BellStyle, EditMode};
use std::sync::{Arc, Mutex};

use super::plugin::PluginRuntime;

// ── Mutable REPL state threaded through rc/profile loading ───────────────

/// Bundles the REPL-level state that rc files and profiles are allowed to
/// mutate: environment bindings, line-editing mode, and bell style.  Passed
/// by `&mut` to the loaders so the signature stays stable as new knobs
/// (themes, keymaps, …) grow.
pub(crate) struct RcCtx<'a> {
    pub shell: &'a mut Shell,
    pub edit_mode: &'a mut EditMode,
    pub bell: &'a mut BellStyle,
    pub surface: &'a mut Surface,
    pub runtime: &'a Arc<Mutex<PluginRuntime>>,
}

// ── Default RC skeleton ──────────────────────────────────────────────────

const DEFAULT_RC: &str = "\
# ~/.config/ral/rc — ral shell configuration
#
# This file must return a map; all keys are optional.
# Uncomment any section you want to customise.

return [
    # edit_mode:        vi,          # emacs (default) or vi
    # bell:             false,       # audible bell on readline error (default false)
    # surface:          readline,    # readline (default), minimal, or structural
    # recursion_limit:  1024,        # maximum function-call recursion depth

    # prompt: {
    #     return \"$CWD $ \"
    # },

    # env: [
    #     EDITOR: vim,
    #     PAGER:  less,
    # ],

    # aliases: [
    #     ll: { |args| ls -lh ...$args },
    #     la: { |args| ls -lha ...$args },
    # ],

    # startup: {
    #     fortune
    # },

    # theme: [
    #     value_prefix: \"=> \",
    #     value_color:  yellow,   # black red green yellow blue magenta cyan white none
    # ],
]
";

/// Write the default RC skeleton to the first resolvable config location.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:rc-write] persists the default repl config dir + rc file; not turn-time model I/O"
)]
pub(super) fn create_default_rc() -> Option<String> {
    let (dir, path) = ral_core::path::config::xdg_config_subpath("ral")
        .map(|dir| {
            let file = dir.join("rc");
            (dir, file)
        })
        .or_else(|| {
            let dot = ral_core::path::config::home_dot(".ralrc")?;
            // Legacy single-file layout: the "dir" is `$HOME`, since
            // there is no per-app subdirectory to create before
            // writing `.ralrc` itself.
            let dir = dot.parent()?.to_path_buf();
            Some((dir, dot))
        })?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "ral: warning: could not create config directory {}: {e}",
            dir.display()
        );
        return None;
    }
    if let Err(e) = std::fs::write(&path, DEFAULT_RC) {
        eprintln!("ral: warning: could not write {}: {e}", path.display());
        return None;
    }
    Some(path.to_string_lossy().into_owned())
}

// ── RC config application ────────────────────────────────────────────────

/// Apply the RC config map to `ctx`. Returns the `startup` block, if any,
/// so the caller can execute it in the right context.  `rc_text` is
/// reserved for future source-text capture (e.g. for `which`).
pub(crate) fn apply_rc_config(
    config: Value,
    ctx: &mut RcCtx<'_>,
    _rc_text: Option<&str>,
) -> Option<Value> {
    let Value::Map(pairs) = config else {
        return None;
    };
    let mut startup: Option<Value> = None;
    for (key, val) in pairs {
        match key.as_str() {
            "env" => {
                for (k, v) in into_map(val) {
                    // PWD / OLDPWD are shell-cwd-derived: they live on
                    // context.cwd, and a copy in env_overrides would shadow
                    // the canonical pair and drift on the next `cd`.  An rc
                    // that spreads a parent shell's environment carries them,
                    // so drop them here rather than feed them to set_env_var.
                    if matches!(k.as_str(), "PWD" | "OLDPWD") {
                        continue;
                    }
                    ctx.shell
                        .mobile
                        .context
                        .set_env_var(k.clone(), v.to_string());
                    ctx.shell.mobile.scope.set(k, v);
                }
            }
            "prompt" => ctx.shell.mobile.scope.set("RAL_PROMPT".into(), val),
            "aliases" => {
                // A callable installs as an argv-handler alias; any other
                // value falls through to a plain scope binding so the key
                // still lands somewhere usable.
                for (name, value) in into_map(val) {
                    if matches!(value, Value::Lambda { .. } | Value::Block { .. }) {
                        if let Err(err) = ctx.shell.install_alias(name, value) {
                            match err {
                                ral_core::types::Break::Error(e) => {
                                    eprintln!("ralrc: {}", e.message)
                                }
                                ral_core::types::Break::Escape(_) => {
                                    eprintln!("ralrc: alias installation escaped")
                                }
                            }
                        }
                    } else {
                        ctx.shell.mobile.scope.set(name, value);
                    }
                }
            }
            "bindings" => {
                // Every value installs as a lexical scope binding,
                // callables included; a callable is typed by the checker
                // so it is callable by function application at the prompt.
                for (name, value) in into_map(val) {
                    ctx.shell.bind_value(name, value);
                }
            }
            "edit_mode" => {
                if let Value::String(s) = val {
                    match s.to_ascii_lowercase().as_str() {
                        "vi" => *ctx.edit_mode = EditMode::Vi,
                        "emacs" => *ctx.edit_mode = EditMode::Emacs,
                        _ => {}
                    }
                }
            }
            "bell" => {
                if let Value::Bool(b) = val {
                    *ctx.bell = if b {
                        BellStyle::Audible
                    } else {
                        BellStyle::None
                    };
                }
            }
            "surface" => {
                if let Value::String(s) = val {
                    match <Surface as clap::ValueEnum>::from_str(&s, true) {
                        Ok(surface) => *ctx.surface = surface,
                        Err(_) => ral_core::diagnostic::shell_warning(&format!(
                            "ralrc: unknown surface '{s}'; expected minimal, readline, or structural"
                        )),
                    }
                }
            }
            "recursion_limit" => {
                if let Some(n) = val.as_int().filter(|n| *n > 0) {
                    ctx.shell.mobile.control.recursion_limit = n as usize;
                }
            }
            "plugins" => {
                for entry in into_list(val) {
                    load_rc_plugin(entry, ctx.shell, ctx.runtime);
                }
            }
            "startup" => startup = Some(val),
            "theme" => {
                if let Value::Map(pairs) = val {
                    let mut theme = OutputTheme::default();
                    for (k, v) in pairs {
                        match k.as_str() {
                            "value_prefix" => theme.value_prefix = v.to_string(),
                            "value_color" => theme.value_color = named_color(&v.to_string()),
                            _ => {}
                        }
                    }
                    set_output_theme(theme);
                }
            }
            _ => {}
        }
    }
    startup
}

fn into_map(v: Value) -> Map {
    match v {
        Value::Map(m) => m,
        _ => Map::new(),
    }
}

fn into_list(v: Value) -> Vec<Value> {
    match v {
        Value::List(l) => l.into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Load a single plugin entry from the RC `plugins` list.
///
/// Each entry is a map `[plugin: Str, options?: Map]`.  Unknown top-level
/// keys are warned and ignored so future extensions (enabled, when, …) can
/// slot in without breaking parsers.
fn load_rc_plugin(entry: Value, shell: &mut Shell, runtime: &Arc<Mutex<PluginRuntime>>) {
    if let Err(msg) = parse_and_load_rc_plugin(entry, shell, runtime) {
        ral_core::diagnostic::cmd_error("ral", &msg);
    }
}

/// Shape-check an rc plugin entry, dispatch to the plugin loader, and
/// return the load error (if any) as a formatted string — the Result lets
/// each field check short-circuit with `?` instead of repeating the
/// cmd_error boilerplate per key.
fn parse_and_load_rc_plugin(
    entry: Value,
    shell: &mut Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
) -> Result<(), String> {
    let Value::Map(pairs) = entry else {
        return Err(format!(
            "plugin entry must be a map [plugin: 'name', options: [...]]; got {}",
            entry.type_name()
        ));
    };
    let mut name: Option<String> = None;
    let mut options: Option<Value> = None;
    for (k, v) in pairs {
        match (k.as_str(), v) {
            ("plugin", Value::String(s)) => name = Some(s),
            ("plugin", v) => {
                return Err(format!(
                    "plugin entry 'plugin' must be a string; got {}",
                    v.type_name()
                ));
            }
            ("options", v @ Value::Map(_)) => options = Some(v),
            ("options", v) => {
                return Err(format!(
                    "plugin entry 'options' must be a map; got {}",
                    v.type_name()
                ));
            }
            (other, _) => ral_core::diagnostic::shell_warning(&format!(
                "ral: plugin entry: unknown key '{other}', ignoring"
            )),
        }
    }
    let name = name.ok_or_else(|| {
        "plugin entry missing required 'plugin' key; \
         expected [plugin: 'name', options: [...]]"
            .to_string()
    })?;
    match super::plugin::load::load_plugin(&name, options.as_ref(), shell, runtime) {
        Err(ral_core::types::Break::Error(e)) => Err(format!("plugin '{name}': {}", e.message)),
        _ => Ok(()),
    }
}

// ── Config file locations ────────────────────────────────────────────────

/// Search for an existing RC file in the standard locations.
pub(super) fn find_ralrc() -> Option<String> {
    let candidates = [
        ral_core::path::config::xdg_config_subpath("ral/rc"),
        ral_core::path::config::home_dot(".ralrc"),
    ];
    for cand in candidates.into_iter().flatten() {
        let s = cand.to_string_lossy();
        if ral_core::path::exists(&s) {
            return Some(s.into_owned());
        }
    }
    None
}

/// Resolve the history file path, creating the config directory if needed.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:history-mkdir] ensures the repl config dir exists for the history file; not turn-time model I/O"
)]
pub(super) fn dirs_history() -> Option<String> {
    if let Some(dir) = ral_core::path::config::xdg_config_subpath("ral") {
        let _ = std::fs::create_dir_all(&dir);
        return Some(dir.join("history").to_string_lossy().into_owned());
    }
    ral_core::path::config::home_dot(".ral_history").map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[io-door:test] test fs/process scaffolding")]
mod tests {
    use super::*;

    /// Evaluate `rc_src`, apply it through `apply_rc_config`, and return
    /// the resulting environment.  Registers the baked prelude so plugin
    /// files can use `get`, `has`, etc. — the same environment they see at
    /// real startup.  When `pass_source` is set, the rc text is forwarded
    /// to `apply_rc_config` so alias source extraction is exercised.
    fn apply_rc_inner(rc_src: &str, pass_source: bool) -> (Shell, Arc<Mutex<PluginRuntime>>) {
        let mut shell = Shell::new(Default::default());
        ral_core::builtins::register(&mut shell, crate::PRELUDE.comp());
        let ast = ral_core::syntax::parser::parse(rc_src).unwrap();
        let comp = std::sync::Arc::new(ral_core::elaborator::elaborate(&ast, Default::default()));
        let config = ral_core::evaluator::evaluate(&comp, &mut shell).unwrap();
        let mut mode = EditMode::Emacs;
        let mut bell = BellStyle::None;
        let mut surface = Surface::default();
        let runtime = Arc::new(Mutex::new(PluginRuntime::default()));
        apply_rc_config(
            config,
            &mut RcCtx {
                shell: &mut shell,
                edit_mode: &mut mode,
                bell: &mut bell,
                surface: &mut surface,
                runtime: &runtime,
            },
            if pass_source { Some(rc_src) } else { None },
        );
        (shell, runtime)
    }

    fn apply_rc(rc_src: &str) -> Shell {
        apply_rc_inner(rc_src, false).0
    }

    fn apply_rc_with_runtime(rc_src: &str) -> (Shell, Arc<Mutex<PluginRuntime>>) {
        apply_rc_inner(rc_src, false)
    }

    /// Typecheck `src` against the baked prelude; return the errors.
    fn typecheck_src(src: &str) -> Vec<ral_core::TypeError> {
        let ast = ral_core::syntax::parser::parse(src).unwrap();
        let comp = ral_core::elaborator::elaborate(&ast, Default::default());
        let schemes = ral_core::SessionSchemes::from_schemes(crate::PRELUDE.schemes());
        ral_core::typecheck(&comp, schemes)
            .err()
            .unwrap_or_default()
    }

    /// Mixed-shape `plugins:` entries (some with `options:`, some without)
    /// typecheck cleanly thanks to the per-entry validation hook.
    #[test]
    fn mixed_shape_plugins_list_typechecks() {
        let src = "return [plugins: [\n\
            [plugin: 'autosuggestion'],\n\
            [plugin: 'fzf-files', options: [key: 'ctrl-t']],\n\
            [plugin: 'fzf-history', options: [key: 'ctrl-r']],\n\
        ]]\n";
        let errs = typecheck_src(src);
        assert!(errs.is_empty(), "unexpected type errors: {errs:?}");
    }

    /// `plugin:` value must be a String.
    #[test]
    fn plugin_entry_bad_plugin_field_fails_typecheck() {
        let errs = typecheck_src("return [plugins: [[plugin: 42]]]\n");
        assert!(
            !errs.is_empty(),
            "expected type error for non-String plugin field"
        );
    }

    /// `options:` value must be a Map.
    #[test]
    fn plugin_entry_bad_options_field_fails_typecheck() {
        let errs = typecheck_src("return [plugins: [[plugin: 'x', options: 'not-a-map']]]\n");
        assert!(
            !errs.is_empty(),
            "expected type error for non-Map options field"
        );
    }

    /// A `plugins` entry of the form `[plugin: <path>, options: [key: val]]`
    /// loads the file and forwards the options map as the block's sole arg.
    #[test]
    fn rc_plugin_entry_forwards_options() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("echo-key.ral");
        // Plugin that echoes an option back as its manifest name.  Using
        // a no-op keybinding keeps the manifest minimal and valid.
        std::fs::write(
            &path,
            r#"return { |options|
    let k = get $options key 'default'
    return [
        name: $k,
    ]
}
"#,
        )
        .unwrap();

        let rc_src = format!(
            "return [plugins: [[plugin: '{}', options: [key: 'from-rc']]]]\n",
            path.to_string_lossy()
        );
        let (_shell, runtime) = apply_rc_with_runtime(&rc_src);
        let rt = runtime.lock().unwrap();
        assert_eq!(rt.plugins.len(), 1);
        assert_eq!(rt.plugins[0].name, "from-rc");
    }

    /// Omitting `options:` loads with an empty map; the plugin's own
    /// defaults apply.
    #[test]
    fn rc_plugin_entry_without_options_uses_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("echo-key.ral");
        std::fs::write(
            &path,
            r#"return { |options|
    let k = get $options key 'fallback'
    return [name: $k]
}
"#,
        )
        .unwrap();

        let rc_src = format!(
            "return [plugins: [[plugin: '{}']]]\n",
            path.to_string_lossy()
        );
        let (_shell, runtime) = apply_rc_with_runtime(&rc_src);
        let rt = runtime.lock().unwrap();
        assert_eq!(rt.plugins.len(), 1);
        assert_eq!(rt.plugins[0].name, "fallback");
    }

    /// Malformed rc plugin entries (non-map, missing `plugin:`) emit a
    /// diagnostic but do not panic and leave the runtime plugin list empty.
    #[test]
    fn rc_plugin_entry_malformed_is_rejected() {
        // Old list form — rejected wholesale.
        let (_, runtime) = apply_rc_with_runtime("return [plugins: [['foo', 'bar']]]\n");
        assert!(runtime.lock().unwrap().plugins.is_empty());

        // Map missing 'plugin:' — rejected.
        let (_, runtime) = apply_rc_with_runtime("return [plugins: [[options: [key: 'x']]]]\n");
        assert!(runtime.lock().unwrap().plugins.is_empty());
    }

    /// Aliases declared in rc install as alias-origin handler frames.
    #[test]
    fn aliases_install_as_handler_frames() {
        let src = "return [\n    aliases: [\n        greet: { |args| echo hello ...$args },\n        ll: { |args| ls -lh ...$args },\n    ],\n]\n";
        let (shell, _) = apply_rc_inner(src, true);
        assert!(shell.has_alias("greet"));
        assert!(shell.has_alias("ll"));
        // Aliases live in the handler stack, not in scope.
        assert!(shell.mobile.scope.get("greet").is_none());
        assert!(shell.mobile.scope.get("ll").is_none());
    }

    /// rc `recursion_limit:` overrides the default on the shell.
    #[test]
    fn rc_recursion_limit_applied() {
        let shell = apply_rc("return [recursion_limit: 256]\n");
        assert_eq!(shell.mobile.control.recursion_limit, 256);
    }

    /// A non-positive `recursion_limit` is silently ignored (the default
    /// stays in place rather than letting `0` through to disable the cap).
    #[test]
    fn rc_recursion_limit_zero_ignored() {
        let shell = apply_rc("return [recursion_limit: 0]\n");
        assert_eq!(
            shell.mobile.control.recursion_limit,
            ral_core::types::DEFAULT_RECURSION_LIMIT
        );
    }

    // ── apply_rc_config: bindings / aliases routing ───────────────────────

    /// Apply `config` to a fresh shell via `apply_rc_config` and return the shell.
    fn apply_to_fresh_env(config: Value) -> Shell {
        let mut shell = Shell::new(Default::default());
        let mut mode = EditMode::Emacs;
        let mut bell = BellStyle::None;
        let mut surface = Surface::default();
        let runtime = Arc::new(Mutex::new(PluginRuntime::default()));
        apply_rc_config(
            config,
            &mut RcCtx {
                shell: &mut shell,
                edit_mode: &mut mode,
                bell: &mut bell,
                surface: &mut surface,
                runtime: &runtime,
            },
            None,
        );
        shell
    }

    /// Apply an rc map and return the resolved [`Surface`] (the default
    /// when the rc does not set one).
    fn apply_rc_surface(rc_src: &str) -> Surface {
        let mut shell = Shell::new(Default::default());
        ral_core::builtins::register(&mut shell, crate::PRELUDE.comp());
        let ast = ral_core::syntax::parser::parse(rc_src).unwrap();
        let comp = std::sync::Arc::new(ral_core::elaborator::elaborate(&ast, Default::default()));
        let config = ral_core::evaluator::evaluate(&comp, &mut shell).unwrap();
        let mut mode = EditMode::Emacs;
        let mut bell = BellStyle::None;
        let mut surface = Surface::default();
        let runtime = Arc::new(Mutex::new(PluginRuntime::default()));
        apply_rc_config(
            config,
            &mut RcCtx {
                shell: &mut shell,
                edit_mode: &mut mode,
                bell: &mut bell,
                surface: &mut surface,
                runtime: &runtime,
            },
            None,
        );
        surface
    }

    /// rc `surface:` selects the frontend, case-insensitively.
    #[test]
    fn rc_surface_selects_frontend() {
        assert_eq!(apply_rc_surface("return [surface: 'structural']\n"), Surface::Structural);
        assert_eq!(apply_rc_surface("return [surface: 'minimal']\n"), Surface::Minimal);
        assert_eq!(apply_rc_surface("return [surface: 'Readline']\n"), Surface::Readline);
    }

    /// An unset or unknown `surface:` leaves the default in place.
    #[test]
    fn rc_surface_defaults_and_ignores_unknown() {
        assert_eq!(apply_rc_surface("return [env: [X: 'y']]\n"), Surface::default());
        assert_eq!(apply_rc_surface("return [surface: 'bogus']\n"), Surface::default());
    }

    #[test]
    fn rc_bindings_populate_value_namespace() {
        let shell = apply_to_fresh_env(Value::map(vec![(
            "bindings".into(),
            Value::map(vec![
                ("greeting".into(), Value::String("hello".into())),
                ("n".into(), Value::Int(42)),
            ]),
        )]));
        assert_eq!(
            shell.mobile.scope.get("greeting"),
            Some(&Value::String("hello".into()))
        );
        assert_eq!(shell.mobile.scope.get("n"), Some(&Value::Int(42)));
    }

    /// Under `aliases:` a callable installs as an alias handler frame; a
    /// non-callable falls through to a plain scope binding.
    #[test]
    fn rc_aliases_route_by_value_shape() {
        let plain = Value::Bool(true);
        let shell = apply_to_fresh_env(Value::map(vec![(
            "aliases".into(),
            Value::map(vec![("ll".into(), plain.clone())]),
        )]));
        assert_eq!(shell.mobile.scope.get("ll"), Some(&plain));
        assert!(!shell.has_alias("ll"));
    }

    /// Typecheck `src` against `shell`'s live session schemes — the same
    /// seed a real prompt turn uses — and return the errors.
    fn typecheck_against_session(shell: &Shell, src: &str) -> Vec<ral_core::TypeError> {
        let ast = ral_core::syntax::parser::parse(src).unwrap();
        let comp = ral_core::elaborator::elaborate(&ast, Default::default());
        ral_core::typecheck(&comp, shell.session_schemes())
            .err()
            .unwrap_or_default()
    }

    /// A callable under `bindings:` is a lexical binding, not an alias: it
    /// lands in scope and carries a session scheme, so a heterogeneous
    /// call `ws 'x' { ... }` (a String and a Block — two independently
    /// typed parameters) typechecks at the prompt.  As an argv alias the
    /// same call would force the argv element type to be both String and
    /// Block, a [T0010] mismatch.
    #[test]
    fn rc_bindings_callable_typechecks_heterogeneous_call() {
        let src = "return [\n    bindings: [\n        ws: { |name body| echo $name; !$body },\n    ],\n]\n";
        let (shell, _) = apply_rc_inner(src, true);
        // Lexical binding, not an alias handler frame.
        assert!(shell.mobile.scope.get("ws").is_some());
        assert!(!shell.has_alias("ws"));
        let errs = typecheck_against_session(&shell, "ws 'x' { echo lol }\n");
        assert!(
            errs.is_empty(),
            "heterogeneous call to a `bindings:` callable should typecheck: {errs:?}"
        );
    }

    /// A callable under `aliases:` installs as an argv-handler alias (a
    /// handler frame, not a scope binding).  An alias is a unary lambda
    /// whose single parameter binds the argv list, so the heterogeneous
    /// call `ws 'x' { echo lol }` forces the homogeneous argv element type
    /// to be both String and Block — a [T0010] mismatch, confirming the
    /// by-key split actually changes behaviour.
    #[test]
    fn rc_aliases_callable_is_argv_alias() {
        let src = "return [\n    aliases: [\n        ws: { |args| echo $args },\n    ],\n]\n";
        let (shell, _) = apply_rc_inner(src, true);
        // Alias handler frame, not a scope binding.
        assert!(shell.has_alias("ws"));
        assert!(shell.mobile.scope.get("ws").is_none());
        let errs = typecheck_against_session(&shell, "ws 'x' { echo lol }\n");
        assert!(
            errs.iter().any(|e| e.kind.code() == "T0010"),
            "heterogeneous call to an argv alias should fail [T0010]: {errs:?}"
        );
    }
}
