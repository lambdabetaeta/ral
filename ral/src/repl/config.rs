//! RC file discovery, parsing, and application.
//!
//! An rc file is ral source whose return value is a map.  Recognised keys
//! map to REPL state: `env`, `prompt`, `bindings`, `aliases`, `edit_mode`,
//! `bell`, `surface`, `recursion_limit`, `plugins`, `startup`, `theme`.
//! Unknown keys are silently ignored so future versions can add knobs without
//! breaking older configs.  A recognised key with a malformed value is
//! rejected with an error naming the key, while the rest of the map still
//! applies.

use ral_core::types::{DefaultPolicy, HookName, HookSig, Map, Mooring};
use ral_core::{Shell, Value};

use super::frontend::Surface;
use super::theme::{OutputTheme, set_output_theme};
use rustyline::config::{BellStyle, EditMode};
use std::sync::{Arc, Mutex};

use super::plugin::PluginRuntime;

// ── Frontend knobs resolved by the rc file ───────────────────────────────

/// The frontend knobs an rc file can set: line-editing mode, bell style,
/// and surface.  Everything else the rc configures lands directly on the
/// shell (env, aliases, bindings, hooks, recursion limit) or the plugin
/// runtime, so rc application takes those two and returns this value —
/// there is no mutable context to thread.
#[derive(Clone, Copy)]
pub(crate) struct RcSettings {
    pub edit_mode: EditMode,
    pub bell: BellStyle,
    pub surface: Surface,
}

impl Default for RcSettings {
    fn default() -> Self {
        Self {
            edit_mode: EditMode::Emacs,
            bell: BellStyle::None,
            surface: Surface::default(),
        }
    }
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

/// Apply the RC config map to the shell and plugin runtime.  Returns the
/// resolved frontend settings and the `startup` block, if any, so the
/// caller can execute it in the right context.  The rc's map contract
/// (and the diagnostic for breaking it) lives with the sourcing in
/// `session::boot`; this function only ever sees a map.
pub(crate) fn apply_rc_config(
    pairs: Map,
    shell: &mut Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
) -> (RcSettings, Option<Value>) {
    let mut settings = RcSettings::default();
    let mut startup: Option<Value> = None;
    for (key, val) in pairs {
        if let Err(msg) = apply_rc_key(&key, val, shell, runtime, &mut settings, &mut startup) {
            ral_core::diagnostic::cmd_error("ral", &msg);
        }
    }
    (settings, startup)
}

/// Apply a single rc top-level `key: val` pair.  An `Err` names the
/// offending key and the shape it expected; the caller reports it and moves
/// on to the next key, so one malformed entry does not block the rest of
/// the rc file.
fn apply_rc_key(
    key: &str,
    val: Value,
    shell: &mut Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
    settings: &mut RcSettings,
    startup: &mut Option<Value>,
) -> Result<(), String> {
    match key {
        "env" => {
            let Value::Map(m) = val else {
                return Err(format!("rc 'env' must be a map; got {}", val.type_name()));
            };
            for (k, v) in m {
                // PWD / OLDPWD are shell-cwd-derived: they live on
                // context.cwd, and a copy in env_overrides would shadow
                // the canonical pair and drift on the next `cd`.  An rc
                // that spreads a parent shell's environment carries them,
                // so drop them here rather than feed them to set_env_var.
                if matches!(k.as_str(), "PWD" | "OLDPWD") {
                    continue;
                }
                shell.set_env_var(k.clone(), v.to_string());
                shell.set_var(k, v);
            }
            Ok(())
        }
        "prompt" => {
            let origin = ral_core::source::Span::synthetic();
            if let Err(e) = shell.register_hook(
                HookName::session("prompt"),
                val,
                HookSig::Prompt,
                DefaultPolicy::denied_capture(),
                origin,
            ) {
                eprintln!("ralrc: {e}");
            }
            Ok(())
        }
        "aliases" => {
            let Value::Map(m) = val else {
                return Err(format!(
                    "rc 'aliases' must be a map; got {}",
                    val.type_name()
                ));
            };
            // A function installs as an argv-handler alias; any other
            // value falls through to a plain scope binding so the key
            // still lands somewhere usable.
            for (name, value) in m {
                if matches!(value, Value::Thunk(_)) {
                    if let Err(err) = shell.install_alias(name, value) {
                        match err {
                            ral_core::types::Break::Error(e) => {
                                eprintln!("ralrc: {}", e.message);
                            }
                            ral_core::types::Break::Escape(_) => {
                                eprintln!("ralrc: alias installation escaped");
                            }
                        }
                    }
                } else {
                    shell.set_var(name, value);
                }
            }
            Ok(())
        }
        "bindings" => {
            let Value::Map(m) = val else {
                return Err(format!(
                    "rc 'bindings' must be a map; got {}",
                    val.type_name()
                ));
            };
            // Every value installs as a lexical scope binding,
            // functions included; a function is typed by the checker
            // so it is applyable by function application at the prompt.
            for (name, value) in m {
                shell.bind_value(name, value);
            }
            Ok(())
        }
        "edit_mode" => {
            let Value::String(s) = val else {
                return Err(format!(
                    "rc 'edit_mode' must be a string; got {}",
                    val.type_name()
                ));
            };
            match s.to_ascii_lowercase().as_str() {
                "vi" => settings.edit_mode = EditMode::Vi,
                "emacs" => settings.edit_mode = EditMode::Emacs,
                _ => {
                    return Err(format!("rc 'edit_mode' must be 'emacs' or 'vi'; got '{s}'"));
                }
            }
            Ok(())
        }
        "bell" => {
            let Value::Bool(b) = val else {
                return Err(format!("rc 'bell' must be a bool; got {}", val.type_name()));
            };
            settings.bell = if b {
                BellStyle::Audible
            } else {
                BellStyle::None
            };
            Ok(())
        }
        "surface" => {
            let Value::String(s) = val else {
                return Err(format!(
                    "rc 'surface' must be a string; got {}",
                    val.type_name()
                ));
            };
            match <Surface as clap::ValueEnum>::from_str(&s, true) {
                Ok(surface) => {
                    settings.surface = surface;
                    Ok(())
                }
                Err(_) => Err(format!(
                    "rc 'surface' must be minimal, readline, or structural; got '{s}'"
                )),
            }
        }
        "recursion_limit" => {
            let Some(n) = val.as_int() else {
                return Err(format!(
                    "rc 'recursion_limit' must be a positive int; got {}",
                    val.type_name()
                ));
            };
            if n <= 0 {
                return Err(format!("rc 'recursion_limit' must be positive; got {n}"));
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "filtered > 0; a recursion limit far below usize::MAX on 64-bit"
            )]
            let limit = n as usize;
            shell.set_recursion_limit(limit);
            Ok(())
        }
        "plugins" => {
            let Value::List(entries) = val else {
                return Err(format!(
                    "rc 'plugins' must be a list of plugin entries; got {}",
                    val.type_name()
                ));
            };
            for entry in entries {
                load_rc_plugin(entry, shell, runtime);
            }
            Ok(())
        }
        "startup" => {
            *startup = Some(val);
            Ok(())
        }
        "theme" => match val {
            Value::Map(pairs) => {
                let theme = OutputTheme::from_map(&pairs)?;
                set_output_theme(theme);
                Ok(())
            }
            other => Err(format!(
                "rc 'theme' must be a map; got {}",
                other.type_name()
            )),
        },
        _ => Ok(()),
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
/// `cmd_error` boilerplate per key.
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
    // rc loading runs at session bring-up, with no run in hand, so the
    // plugin file evaluates moored adrift.
    match super::plugin::load::load_plugin(
        &name,
        options.as_ref(),
        &Mooring::adrift(),
        shell,
        runtime,
    ) {
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    /// Evaluate `rc_src`, apply it through `apply_rc_config`, and return
    /// the resulting environment.  Registers the baked prelude so plugin
    /// files can use `get`, `has`, etc. — the same environment they see at
    /// real startup.
    fn apply_rc_inner(rc_src: &str) -> (Shell, RcSettings, Arc<Mutex<PluginRuntime>>) {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        ral_core::builtins::register(&mut shell, crate::PRELUDE.comp());
        let config = match shell.run(ral_core::RunRequest {
            run: ral_core::transport::Run {
                program: ral_core::transport::Program::Source(rc_src.to_string()),
                script_name: "<rc>".to_string(),
                caps: ral_core::types::Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: ral_core::RunIo::Inherit,
                terminal: ral_core::RequestedTerminalAccess::Leased,
                stdin: ral_core::RunStdin::Inherit,
                trail: None,
            },
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
            lifecycle: Box::new(()),
        }) {
            ral_core::RunReport::Ran { ending, .. } => ending.into_result().expect("rc must run"),
            ral_core::RunReport::Static { .. } => panic!("rc source must run: {rc_src:?}"),
        };
        let Value::Map(pairs) = config else {
            panic!(
                "test rc source must return a map; got {}",
                config.type_name()
            );
        };
        let runtime = Arc::new(Mutex::new(PluginRuntime::default()));
        let (settings, _) = apply_rc_config(pairs, &mut shell, &runtime);
        (shell, settings, runtime)
    }

    fn apply_rc(rc_src: &str) -> Shell {
        apply_rc_inner(rc_src).0
    }

    fn apply_rc_with_runtime(rc_src: &str) -> (Shell, Arc<Mutex<PluginRuntime>>) {
        let (shell, _, runtime) = apply_rc_inner(rc_src);
        (shell, runtime)
    }

    /// Typecheck `src` against the baked prelude; return the errors.
    fn typecheck_src(src: &str) -> Vec<ral_core::TypeError> {
        let ast = ral_core::syntax::parser::parse(src).unwrap();
        let comp = ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default(), "")
            .expect("elaborate");
        let schemes = ral_core::SessionSchemes::from_schemes(
            crate::PRELUDE.schemes(),
            ral_core::HostSurface::default().builtin_table(),
        );
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
            r"return { |options|
    let k = get $options key 'default'
    return [
        name: $k,
    ]
}
",
        )
        .unwrap();

        let rc_src = format!(
            "return [plugins: [[plugin: '{}', options: [key: 'from-rc']]]]\n",
            path.to_string_lossy()
        );
        let (_shell, runtime) = apply_rc_with_runtime(&rc_src);
        let rt = runtime.lock().unwrap();
        let plugin_count = rt.plugins.len();
        let first_name = rt.plugins[0].name.clone();
        drop(rt);
        assert_eq!(plugin_count, 1);
        assert_eq!(first_name, "from-rc");
    }

    /// Omitting `options:` loads with an empty map; the plugin's own
    /// defaults apply.
    #[test]
    fn rc_plugin_entry_without_options_uses_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("echo-key.ral");
        std::fs::write(
            &path,
            r"return { |options|
    let k = get $options key 'fallback'
    return [name: $k]
}
",
        )
        .unwrap();

        let rc_src = format!(
            "return [plugins: [[plugin: '{}']]]\n",
            path.to_string_lossy()
        );
        let (_shell, runtime) = apply_rc_with_runtime(&rc_src);
        let rt = runtime.lock().unwrap();
        let plugin_count = rt.plugins.len();
        let first_name = rt.plugins[0].name.clone();
        drop(rt);
        assert_eq!(plugin_count, 1);
        assert_eq!(first_name, "fallback");
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
        let (shell, _, _) = apply_rc_inner(src);
        assert!(shell.has_alias("greet"));
        assert!(shell.has_alias("ll"));
        // Aliases live in the handler stack, not in scope.
        assert!(shell.scope_lookup("greet").is_none());
        assert!(shell.scope_lookup("ll").is_none());
    }

    /// rc `recursion_limit:` overrides the default on the shell.
    #[test]
    fn rc_recursion_limit_applied() {
        let shell = apply_rc("return [recursion_limit: 256]\n");
        assert_eq!(shell.recursion_limit(), 256);
    }

    /// A non-positive `recursion_limit` is refused with a diagnostic; the
    /// default stays in place rather than letting `0` through to disable
    /// the cap.
    #[test]
    fn rc_recursion_limit_zero_rejected() {
        let shell = apply_rc("return [recursion_limit: 0]\n");
        assert_eq!(
            shell.recursion_limit(),
            ral_core::types::DEFAULT_RECURSION_LIMIT
        );
    }

    /// A wrong-typed `recursion_limit` is rejected; the default stays.
    #[test]
    fn rc_recursion_limit_wrong_type_rejected() {
        let shell = apply_to_fresh_env(Value::map(vec![(
            "recursion_limit".into(),
            Value::String("lots".into()),
        )]));
        assert_eq!(
            shell.recursion_limit(),
            ral_core::types::DEFAULT_RECURSION_LIMIT
        );
    }

    /// Both an unrecognised string and a wrong-typed `edit_mode` are
    /// rejected; the default `EditMode::Emacs` stays in place.
    #[test]
    fn rc_edit_mode_invalid_rejected() {
        let (_, settings, _) = apply_to_fresh_env_full(Value::map(vec![(
            "edit_mode".into(),
            Value::String("typo".into()),
        )]));
        assert_eq!(settings.edit_mode, EditMode::Emacs);

        let (_, settings, _) =
            apply_to_fresh_env_full(Value::map(vec![("edit_mode".into(), Value::Int(3))]));
        assert_eq!(settings.edit_mode, EditMode::Emacs);
    }

    /// A wrong-typed `bell` is rejected; the default `BellStyle::None` stays.
    #[test]
    fn rc_bell_wrong_type_rejected() {
        let (_, settings, _) = apply_to_fresh_env_full(Value::map(vec![(
            "bell".into(),
            Value::String("yes".into()),
        )]));
        assert_eq!(settings.bell, BellStyle::None);
    }

    // ── apply_rc_config: bindings / aliases routing ───────────────────────

    /// Apply `config` to a fresh shell via `apply_rc_config` and return the shell.
    fn apply_to_fresh_env(config: Value) -> Shell {
        apply_to_fresh_env_full(config).0
    }

    /// Apply `config` to a fresh shell via `apply_rc_config` and return the
    /// full post-application state: shell, resolved settings, and plugin
    /// runtime.
    fn apply_to_fresh_env_full(config: Value) -> (Shell, RcSettings, Arc<Mutex<PluginRuntime>>) {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let runtime = Arc::new(Mutex::new(PluginRuntime::default()));
        let Value::Map(pairs) = config else {
            panic!("test rc config must be a map; got {}", config.type_name());
        };
        let (settings, _) = apply_rc_config(pairs, &mut shell, &runtime);
        (shell, settings, runtime)
    }

    /// Apply an rc map and return the resolved [`Surface`] (the default
    /// when the rc does not set one).
    fn apply_rc_surface(rc_src: &str) -> Surface {
        apply_rc_inner(rc_src).1.surface
    }

    /// rc `surface:` selects the frontend, case-insensitively.
    #[test]
    fn rc_surface_selects_frontend() {
        assert_eq!(
            apply_rc_surface("return [surface: 'structural']\n"),
            Surface::Structural
        );
        assert_eq!(
            apply_rc_surface("return [surface: 'minimal']\n"),
            Surface::Minimal
        );
        assert_eq!(
            apply_rc_surface("return [surface: 'Readline']\n"),
            Surface::Readline
        );
    }

    /// An unset `surface:` leaves the default in place; an unrecognised
    /// name is rejected loudly rather than silently ignored, and the
    /// default is retained either way.
    #[test]
    fn rc_surface_unknown_rejected_default_retained() {
        assert_eq!(
            apply_rc_surface("return [env: [X: 'y']]\n"),
            Surface::default()
        );
        assert_eq!(
            apply_rc_surface("return [surface: 'bogus']\n"),
            Surface::default()
        );
    }

    /// A wrong-typed `surface:` is rejected; the default stays.
    #[test]
    fn rc_surface_wrong_type_rejected() {
        let (_, settings, _) =
            apply_to_fresh_env_full(Value::map(vec![("surface".into(), Value::Int(7))]));
        assert_eq!(settings.surface, Surface::default());
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
            shell.scope_lookup("greeting"),
            Some(&Value::String("hello".into()))
        );
        assert_eq!(shell.scope_lookup("n"), Some(&Value::Int(42)));
    }

    /// Under `aliases:` a function installs as an alias handler frame; a
    /// non-function value falls through to a plain scope binding.
    #[test]
    fn rc_aliases_route_by_value_shape() {
        let plain = Value::Bool(true);
        let shell = apply_to_fresh_env(Value::map(vec![(
            "aliases".into(),
            Value::map(vec![("ll".into(), plain.clone())]),
        )]));
        assert_eq!(shell.scope_lookup("ll"), Some(&plain));
        assert!(!shell.has_alias("ll"));
    }

    /// A wrong-typed `plugins:` value is rejected; the runtime plugin list
    /// stays empty.
    #[test]
    fn rc_plugins_wrong_type_rejected() {
        let (_, _, runtime) =
            apply_to_fresh_env_full(Value::map(vec![("plugins".into(), Value::Int(7))]));
        assert!(runtime.lock().unwrap().plugins.is_empty());
    }

    /// Wrong-typed `env:`, `aliases:`, and `bindings:` values are each
    /// rejected; no alias installs and neither scope lookup resolves.
    #[test]
    fn rc_env_aliases_bindings_wrong_type_rejected() {
        let shell = apply_to_fresh_env(Value::map(vec![
            ("env".into(), Value::Int(7)),
            ("aliases".into(), Value::String("x".into())),
            ("bindings".into(), Value::Bool(true)),
        ]));
        assert!(!shell.has_alias("x"));
        assert!(shell.scope_lookup("x").is_none());
    }

    /// A malformed key in the rc map does not block the other keys in the
    /// same map from applying.
    #[test]
    fn rc_bad_key_does_not_block_other_keys() {
        let (shell, settings, _) = apply_to_fresh_env_full(Value::map(vec![
            ("edit_mode".into(), Value::Int(42)),
            ("recursion_limit".into(), Value::Int(256)),
        ]));
        assert_eq!(shell.recursion_limit(), 256);
        assert_eq!(settings.edit_mode, EditMode::Emacs);
    }

    /// Typecheck `src` against `shell`'s live session schemes — the same
    /// seed a real prompt run uses — and return the errors.
    fn typecheck_against_session(shell: &Shell, src: &str) -> Vec<ral_core::TypeError> {
        let ast = ral_core::syntax::parser::parse(src).unwrap();
        let comp = ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default(), "")
            .expect("elaborate");
        ral_core::typecheck(&comp, shell.session_schemes())
            .err()
            .unwrap_or_default()
    }

    /// A function under `bindings:` is a lexical binding, not an alias: it
    /// lands in scope and carries a session scheme, so a heterogeneous
    /// call `ws 'x' { ... }` (a String and a Block — two independently
    /// typed parameters) typechecks at the prompt.  As an argv alias the
    /// same call would force the argv element type to be both String and
    /// Block, a [T0010] mismatch.
    #[test]
    fn rc_bindings_function_typechecks_heterogeneous_call() {
        let src = "return [\n    bindings: [\n        ws: { |name body| echo $name; !$body },\n    ],\n]\n";
        let (shell, _, _) = apply_rc_inner(src);
        // Lexical binding, not an alias handler frame.
        assert!(shell.scope_lookup("ws").is_some());
        assert!(!shell.has_alias("ws"));
        let errs = typecheck_against_session(&shell, "ws 'x' { echo lol }\n");
        assert!(
            errs.is_empty(),
            "heterogeneous call to a `bindings:` function should typecheck: {errs:?}"
        );
    }

    /// A function under `aliases:` installs as an argv-handler alias (a
    /// handler frame, not a scope binding).  An alias is a unary lambda whose
    /// single parameter binds the argv, and an argv is a list of strings every
    /// element crosses rendered — so where the `bindings:` function above has
    /// two typed parameters, this one has no arity at all: the same
    /// heterogeneous call lands, and so does a call with nothing to pass.
    #[test]
    fn rc_aliases_function_is_argv_alias() {
        let src = "return [\n    aliases: [\n        ws: { |args| echo $args },\n    ],\n]\n";
        let (shell, _, _) = apply_rc_inner(src);
        // Alias handler frame, not a scope binding.
        assert!(shell.has_alias("ws"));
        assert!(shell.scope_lookup("ws").is_none());
        for call in ["ws 'x' { echo lol }\n", "ws\n"] {
            let errs = typecheck_against_session(&shell, call);
            assert!(
                errs.is_empty(),
                "an argv alias takes any argv, rendered: `{call}` gave {errs:?}"
            );
        }
    }
}
