//! RC file discovery, parsing, and application.
//!
//! An rc file is ral source whose return value is a map.  Recognised keys
//! map to REPL state: `env`, `prompt`, `bindings`, `aliases`, `edit_mode`,
//! `bell`, `surface`, `recursion_limit`, `plugins`, `startup`, `theme`.
//! Unknown keys are silently ignored so future versions can add knobs without
//! breaking older configs.  A recognised key with a malformed value is
//! rejected with an error naming the key, while the rest of the map still
//! applies.

use ral_core::types::{Break, DefaultPolicy, Error, HookName, HookSig, Map, Mooring};
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
    # recursion_limit:  100000,      # maximum machine-frame recursion depth

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

    # plugins: [
    #     zoxide:         [key: 'alt-z'],  # plugin name (or path) => its options
    #     autosuggestion: [:],             # [:] — no options, the plugin's own defaults
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
    reason = "[silent:rc-write] persists the default repl config dir + rc file; not turn-time model I/O"
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
        if let Err(err) = apply_rc_key(&key, val, shell, runtime, &mut settings, &mut startup) {
            eprint!(
                "{}",
                ral_core::diagnostic::format_runtime_error_auto(shell.sources(), &err, None)
            );
        }
    }
    (settings, startup)
}

/// Schema for the rc top-level map's scalar-typed keys.
///
/// The rc's [`ReturnContract`](ral_core::typecheck::ReturnContract), held
/// against a literal rc return as it is checked — deliberately partial:
/// `prompt:`/`aliases:`/`bindings:`/`plugins:` hold handler values, or one
/// type per key rather than one across the key, and no single `Ty` pins
/// either. `apply_rc_key`'s own per-key check below is what still catches
/// all of them, and every key here besides.
pub(super) fn rc_field_ty(key: &str, u: &mut ral_core::typecheck::Unifier) -> Option<ral_core::typecheck::Ty> {
    use ral_core::typecheck::Ty;
    match key {
        "edit_mode" | "surface" => Some(Ty::String),
        "bell" => Some(Ty::Bool),
        "recursion_limit" => Some(Ty::Int),
        "env" | "theme" => Some(Ty::Map(Box::new(Ty::Var(u.fresh_tyvar())))),
        _ => None,
    }
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
) -> Result<(), Error> {
    match key {
        "env" => {
            let Value::Map(m) = val else {
                return Err(Error::new(
                    format!("rc 'env' must be a map; got {}", val.type_name()),
                    1,
                ));
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
            shell
                .register_hook(
                    HookName::session("prompt"),
                    val,
                    HookSig::Prompt,
                    DefaultPolicy::denied_capture(),
                    origin,
                )
                .map_err(|e| Error::new(e.to_string(), 1))
        }
        "aliases" => {
            let Value::Map(m) = val else {
                return Err(Error::new(
                    format!("rc 'aliases' must be a map; got {}", val.type_name()),
                    1,
                ));
            };
            // A function installs as an argv-handler alias; any other
            // value falls through to a plain scope binding so the key
            // still lands somewhere usable.
            m.into_iter().try_for_each(|(name, value)| {
                if matches!(value, Value::Thunk(_)) {
                    let alias = name.clone();
                    shell.install_alias(name, value).map_err(|err| match err {
                        Break::Error(e) => e.context(format!("ralrc alias '{alias}'")),
                        Break::Escape(_) => {
                            Error::new(format!("ralrc alias '{alias}' installation escaped"), 1)
                        }
                    })
                } else {
                    shell.set_var(name, value);
                    Ok(())
                }
            })
        }
        "bindings" => {
            let Value::Map(m) = val else {
                return Err(Error::new(
                    format!("rc 'bindings' must be a map; got {}", val.type_name()),
                    1,
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
                return Err(Error::new(
                    format!("rc 'edit_mode' must be a string; got {}", val.type_name()),
                    1,
                ));
            };
            match s.to_ascii_lowercase().as_str() {
                "vi" => settings.edit_mode = EditMode::Vi,
                "emacs" => settings.edit_mode = EditMode::Emacs,
                _ => {
                    return Err(Error::new(
                        format!("rc 'edit_mode' must be 'emacs' or 'vi'; got '{s}'"),
                        1,
                    ));
                }
            }
            Ok(())
        }
        "bell" => {
            let Value::Bool(b) = val else {
                return Err(Error::new(
                    format!("rc 'bell' must be a bool; got {}", val.type_name()),
                    1,
                ));
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
                return Err(Error::new(
                    format!("rc 'surface' must be a string; got {}", val.type_name()),
                    1,
                ));
            };
            match <Surface as clap::ValueEnum>::from_str(&s, true) {
                Ok(surface) => {
                    settings.surface = surface;
                    Ok(())
                }
                Err(_) => Err(Error::new(
                    format!("rc 'surface' must be minimal, readline, or structural; got '{s}'"),
                    1,
                )),
            }
        }
        "recursion_limit" => {
            let Some(n) = val.as_int() else {
                return Err(Error::new(
                    format!(
                        "rc 'recursion_limit' must be a positive int; got {}",
                        val.type_name()
                    ),
                    1,
                ));
            };
            if n <= 0 {
                return Err(Error::new(
                    format!("rc 'recursion_limit' must be positive; got {n}"),
                    1,
                ));
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "filtered > 0; a recursion limit far below usize::MAX on 64-bit"
            )]
            let limit = n as usize;
            shell.set_stack_limit(limit);
            Ok(())
        }
        "plugins" => {
            let Value::Map(entries) = val else {
                return Err(Error::new(
                    format!(
                        "rc 'plugins' must be a map from plugin name to its options, \
                         e.g. [zoxide: [key: 'alt-z'], autosuggestion: [:]]; got {}",
                        val.type_name()
                    ),
                    1,
                ));
            };
            for (name, options) in entries {
                if let Err(err) = load_rc_plugin(&name, options, shell, runtime) {
                    eprint!(
                        "{}",
                        ral_core::diagnostic::format_runtime_error_auto(shell.sources(), &err, None)
                    );
                }
            }
            Ok(())
        }
        "startup" => {
            *startup = Some(val);
            Ok(())
        }
        "theme" => match val {
            Value::Map(pairs) => {
                let theme = OutputTheme::from_map(&pairs).map_err(|msg| Error::new(msg, 1))?;
                set_output_theme(theme);
                Ok(())
            }
            other => Err(Error::new(
                format!("rc 'theme' must be a map; got {}", other.type_name()),
                1,
            )),
        },
        _ => Ok(()),
    }
}

/// Load one rc `plugins:` entry: the key names the plugin (or its path),
/// the value is the options map, forwarded verbatim to the plugin's
/// top-level block.
fn load_rc_plugin(
    name: &str,
    options: Value,
    shell: &mut Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
) -> Result<(), Error> {
    let Value::Map(options) = options else {
        return Err(Error::new(
            format!(
                "rc plugin '{name}': the value under a plugin name is its options map; got {}. \
                 Write `{name}: [:]` if it takes no options.",
                options.type_name()
            ),
            1,
        ));
    };
    // rc loading runs at session bring-up, with no run in hand, so the
    // plugin file evaluates moored adrift.
    match super::plugin::load::load_plugin(name, &options, &Mooring::adrift(), shell, runtime) {
        Err(Break::Error(e)) => Err(e.context(format!("plugin '{name}'"))),
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
    reason = "[silent:history-mkdir] ensures the repl config dir exists for the history file; not turn-time model I/O"
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
    reason = "[test] test fs/process scaffolding"
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
            run: ral_core::protocol::Run {
                program: ral_core::protocol::Program::Source(rc_src.to_string()),
                script_name: "<rc>".to_string(),
                caps: ral_core::types::GrantStack::root(),
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

    /// Write `plugin` to a temp file, load it from an rc whose sole
    /// `plugins:` entry gives it `options`, and return the loaded plugin's
    /// manifest name — `None` when the load was rejected.
    fn loaded_plugin_name(plugin: &str, options: &str) -> Option<String> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plugin.ral");
        std::fs::write(&path, plugin).unwrap();
        let rc_src = format!(
            "return [plugins: ['{}': {options}]]\n",
            path.to_string_lossy()
        );
        let (_shell, runtime) = apply_rc_with_runtime(&rc_src);
        let rt = runtime.lock().unwrap();
        rt.plugins.first().map(|p| p.name.clone())
    }

    /// The value under a plugin name is its options map, forwarded verbatim
    /// as the manifest block's sole argument.  `[:]` forwards an empty map,
    /// so the plugin's own defaults stand.
    #[test]
    fn rc_plugin_options_are_forwarded() {
        // Echoes an option back as its manifest name, so the name the
        // runtime records reports what the block received.
        let plugin = r"return { |options|
    let k = get $options key 'fallback'
    return [name: $k]
}
";
        assert_eq!(
            loaded_plugin_name(plugin, "[key: 'from-rc']").as_deref(),
            Some("from-rc")
        );
        assert_eq!(
            loaded_plugin_name(plugin, "[:]").as_deref(),
            Some("fallback")
        );
    }

    /// A plugin manifest's `name:` field with the wrong literal type is a
    /// type error caught before the manifest is ever parsed as a value —
    /// not merely a runtime rejection once the map comes back.
    #[test]
    fn rc_plugin_bad_manifest_name_is_rejected() {
        assert_eq!(loaded_plugin_name("return [name: 42]\n", "[:]"), None);
    }

    /// The value under a plugin name is its options map and nothing else:
    /// anything but a map is reported and that one plugin skipped.
    #[test]
    fn rc_plugin_non_map_options_is_rejected() {
        let (_, runtime) = apply_rc_with_runtime("return [plugins: [zoxide: 'alt-z']]\n");
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
        assert_eq!(shell.stack_limit(), 256);
    }

    /// A non-positive `recursion_limit` is refused with a diagnostic; the
    /// default stays in place rather than letting `0` through to disable
    /// the cap.
    #[test]
    fn rc_recursion_limit_zero_rejected() {
        let shell = apply_rc("return [recursion_limit: 0]\n");
        assert_eq!(shell.stack_limit(), ral_core::types::DEFAULT_STACK_LIMIT);
    }

    /// A wrong-typed `recursion_limit` is rejected; the default stays.
    #[test]
    fn rc_recursion_limit_wrong_type_rejected() {
        let shell = apply_to_fresh_env(Value::map(vec![(
            "recursion_limit".into(),
            Value::String("lots".into()),
        )]));
        assert_eq!(shell.stack_limit(), ral_core::types::DEFAULT_STACK_LIMIT);
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
        assert_eq!(shell.stack_limit(), 256);
        assert_eq!(settings.edit_mode, EditMode::Emacs);
    }

    /// Typecheck `src` against `shell`'s live session schemes — the same
    /// seed a real prompt run uses — and return the errors.
    fn typecheck_against_session(shell: &Shell, src: &str) -> Vec<ral_core::TypeError> {
        let ast = ral_core::syntax::parser::parse(src).unwrap();
        let comp = ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default(), "")
            .expect("elaborate");
        ral_core::typecheck(&comp, shell.session_schemes(), None)
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
