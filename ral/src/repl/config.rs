//! RC file discovery, parsing, and application.
//!
//! An rc file is ral source whose return value is a configuration record.
//! Its eleven keys are declared once, in `Form::Rc`'s table, and the checker
//! holds the file's inferred return row to them — unknown key or wrong
//! field type alike — to a static error, whatever syntax produced it, and
//! the whole file is skipped. A file returning a *map* has no row to check,
//! so `apply_rc_config` meets the same keyset and the same field types
//! itself, before applying anything: either mistake refuses the whole rc
//! there too, agreeing with what the static check already does.
//!
//! All of it runs engine-side, inside the boot door; the host is told only
//! the [`RcSettings`] its frontend needs.

pub(crate) mod source;

use ral_core::record;
use ral_core::serial::FOValue;
use ral_core::serial::datum::Datum;
use ral_core::typecheck::Form;
use ral_core::types::{Break, DefaultPolicy, Error, HookName, HookSig, Map, Mooring};
use ral_core::{Shell, Value};

use super::frontend::Surface;
use super::plugin::Keymap;
use super::theme::OutputTheme;

// ── Frontend knobs resolved by the rc file ───────────────────────────────

/// What the rc settles for the host: its frontend knobs, its output theme,
/// and whether it registered a `startup` block for the host to dispatch.
/// Everything else the rc configures lands on the engine's shell.
#[derive(Clone, Default)]
pub(crate) struct RcSettings {
    pub(super) edit_mode: Keymap,
    pub(super) bell: bool,
    pub(super) surface: Surface,
    pub(super) theme: OutputTheme,
    pub(super) startup: bool,
}

record!(RcSettings {
    edit_mode: "edit_mode",
    bell: "bell",
    surface: "surface",
    theme: "theme",
    startup: "startup",
});

impl Datum for Surface {
    fn encode(self) -> FOValue {
        clap::ValueEnum::to_possible_value(&self)
            .map(|v| v.get_name().to_string())
            .unwrap_or_default()
            .encode()
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        let name = String::decode(v)?;
        <Self as clap::ValueEnum>::from_str(&name, true)
            .map_err(|_| format!("expected minimal, readline, or structural, got '{name}'"))
    }
}

// ── Default RC skeleton ──────────────────────────────────────────────────

const DEFAULT_RC: &str = "\
# ~/.config/ral/rc — ral shell configuration
#
# This file must return a record or a map; all keys are optional.
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

/// The trailer every runtime refusal of a *mapped* rc carries: a bad key or
/// a bad value both mean the whole rc is refused, not applied piecemeal, so
/// the reader is told plainly that defaults are what actually took effect.
fn refuse(reason: impl std::fmt::Display) -> String {
    format!("rc: {reason}. The rc was not applied; the shell started with defaults.")
}

/// Apply the RC config map to the shell, loading its plugins under
/// `mooring`.  Returns the resolved settings and the `startup` block, if
/// any, for the caller to register.  The rc's map contract
/// (and the diagnostic for breaking it) lives with the sourcing in
/// [`source`]; this function only ever sees a map.
///
/// A map has no row for the checker to hold to the rc's keyset or its
/// fields' types, so both are met here instead, agreeing with what a
/// record return is held to statically: an unknown key, or a known key
/// whose value is the wrong shape, refuses the whole rc rather than the
/// keys around it landing first.
///
/// Refusing *before* mutating anything is only fully honest for the seven
/// keys [`rc_value_shape_error`] can judge from the value alone; `prompt`,
/// `aliases`, and `theme` each judge more than that (a block's arity, a
/// thunk's route, a nested key `apply_rc_key`'s own decoder warns about),
/// so those three run first — if any refuses, nothing else has been
/// touched yet — and everything left, already shape-checked or (`startup`)
/// unconditional, cannot fail behind them.
pub(crate) fn apply_rc_config(
    pairs: Map,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Result<(RcSettings, Option<Value>), String> {
    let table = ral_core::typecheck::contract::declared(Form::Rc);
    if let Some(key) = pairs.keys().find(|k| table.holds(k).is_none()) {
        return Err(refuse(table.unknown_key(key)));
    }
    if let Some(err) = pairs.iter().find_map(|(k, v)| rc_value_shape_error(k, v)) {
        return Err(refuse(err.message));
    }

    let mut settings = RcSettings::default();
    let mut startup: Option<Value> = None;
    for key in ["prompt", "aliases", "theme"] {
        if let Some(val) = pairs.get(key) {
            apply_rc_key(
                key,
                val.clone(),
                mooring,
                shell,
                &mut settings,
                &mut startup,
            )
            .map_err(|err| refuse(err.message))?;
        }
    }
    for (key, val) in pairs {
        if matches!(key.as_str(), "prompt" | "aliases" | "theme") {
            continue;
        }
        // Every other key was shape-checked above, so this cannot fail.
        apply_rc_key(&key, val, mooring, shell, &mut settings, &mut startup)
            .map_err(|err| refuse(err.message))?;
    }
    Ok((settings, startup))
}

/// What [`apply_rc_key`] would reject about `val` before it applies
/// anything: the same shape checks, read-only.  `prompt` and `aliases`
/// judge more than a value's shape (a block's arity, a thunk's route), and
/// `theme`'s own decoder warns on an unknown nested key as a side effect of
/// judging it — a second, pure call here would print that warning twice —
/// so all three are left to `apply_rc_key` itself, run first in
/// `apply_rc_config` so nothing else has landed if they refuse. Every other
/// key's failure is exactly a shape mismatch, so checking it here first is
/// what lets `apply_rc_config` refuse a bad value without touching the shell.
fn rc_value_shape_error(key: &str, val: &Value) -> Option<Error> {
    let err = |msg: String| Some(Error::new(msg, 1));
    match key {
        "env" | "bindings" if !matches!(val, Value::Map(_)) => {
            err(format!("rc '{key}' must be a map; got {}", val.type_name()))
        }
        "edit_mode" => match val {
            Value::String(s) if matches!(s.to_ascii_lowercase().as_str(), "vi" | "emacs") => None,
            Value::String(s) => err(format!("rc 'edit_mode' must be 'emacs' or 'vi'; got '{s}'")),
            other => err(format!(
                "rc 'edit_mode' must be a string; got {}",
                other.type_name()
            )),
        },
        "bell" if !matches!(val, Value::Bool(_)) => {
            err(format!("rc 'bell' must be a bool; got {}", val.type_name()))
        }
        "surface" => match val {
            Value::String(s) if <Surface as clap::ValueEnum>::from_str(s, true).is_ok() => None,
            Value::String(s) => err(format!(
                "rc 'surface' must be minimal, readline, or structural; got '{s}'"
            )),
            other => err(format!(
                "rc 'surface' must be a string; got {}",
                other.type_name()
            )),
        },
        "recursion_limit" => match val.as_int() {
            Some(n) if n > 0 => None,
            Some(n) => err(format!("rc 'recursion_limit' must be positive; got {n}")),
            None => err(format!(
                "rc 'recursion_limit' must be a positive int; got {}",
                val.type_name()
            )),
        },
        "plugins" if !matches!(val, Value::Map(_)) => err(format!(
            "rc 'plugins' must be a map from plugin name to its options, \
             e.g. [zoxide: [key: 'alt-z'], autosuggestion: [:]]; got {}",
            val.type_name()
        )),
        _ => None,
    }
}

/// Apply a single rc top-level `key: val` pair.  Called only for a key
/// `apply_rc_config` has already found in the keyset; an `Err` here now
/// refuses the whole rc at the caller, same as an unknown key, so this
/// function's only job is to judge and apply — not to soften a refusal into
/// "reported, the rest still applies".
///
/// The keyset is [`Form::Rc`]'s declared table, which is also what the
/// checker holds an rc file's returned row to; the shapes it leaves to the
/// decoder — a map of hooks, a map of aliases, a theme — are the per-key
/// checks below, mirrored read-only in [`rc_value_shape_error`] so the
/// caller can refuse most of them before this function ever mutates
/// anything. The catch-all arm is unreachable in practice: it exists only
/// because `key: &str` is not the enum the caller's keyset pre-check reads.
fn apply_rc_key(
    key: &str,
    val: Value,
    mooring: &Mooring,
    shell: &mut Shell,
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
                "vi" => settings.edit_mode = Keymap::Vi,
                "emacs" => settings.edit_mode = Keymap::Emacs,
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
            settings.bell = b;
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
                if let Err(err) = load_rc_plugin(&name, options, mooring, shell) {
                    eprint!(
                        "{}",
                        ral_core::diagnostic::format_runtime_error_auto(
                            shell.sources(),
                            &err,
                            None
                        )
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
                settings.theme = OutputTheme::from_map(&pairs).map_err(|msg| Error::new(msg, 1))?;
                Ok(())
            }
            other => Err(Error::new(
                format!("rc 'theme' must be a map; got {}", other.type_name()),
                1,
            )),
        },
        other => Err(Error::new(
            ral_core::typecheck::contract::declared(Form::Rc).unknown_key(other),
            1,
        )),
    }
}

/// Load one rc `plugins:` entry: the key names the plugin (or its path),
/// the value is the options map, forwarded verbatim to the plugin's
/// top-level block.
fn load_rc_plugin(
    name: &str,
    options: Value,
    mooring: &Mooring,
    shell: &mut Shell,
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
    match super::plugin::load::load_plugin(name, &options, mooring, shell) {
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

    use crate::repl::host::ReplHost;
    use ral_core::types::{BuiltinBody, BuiltinEntry};
    use std::borrow::Cow;
    use std::sync::{Arc, Mutex};

    fn prelude_shell() -> Shell {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        ral_core::builtins::register(&mut shell, crate::PRELUDE.comp());
        shell
    }

    fn rc_map(shell: &mut Shell, rc_src: &str) -> Map {
        match crate::repl::eval(shell, rc_src) {
            Value::Map(pairs) => pairs,
            other => panic!(
                "test rc source must return a record or a map; got {}",
                other.type_name()
            ),
        }
    }

    /// Evaluate `rc_src`, apply it through `apply_rc_config`, and return the
    /// resulting shell and settings.  Registers the baked prelude so the rc
    /// sees the environment it sees at real startup.
    fn apply_rc_inner(rc_src: &str) -> (Shell, RcSettings) {
        let mut shell = prelude_shell();
        let pairs = rc_map(&mut shell, rc_src);
        let (settings, _) = apply_rc_config(pairs, &Mooring::adrift(), &mut shell)
            .expect("test rc must satisfy the keyset");
        (shell, settings)
    }

    fn apply_rc(rc_src: &str) -> Shell {
        apply_rc_inner(rc_src).0
    }

    fn unit_thunk(_u: &mut ral_core::typecheck::Unifier) -> ral_core::Scheme {
        use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
        mk_scheme(&[], &[], &[], thunk(pure(ral_core::typecheck::Ty::Unit)))
    }

    /// Apply `rc_src` inside a dispatch with the REPL host, as the boot door
    /// does, so a plugin load has a desk to tell; the host's plugins after.
    fn loaded_plugins(rc_src: &str) -> Vec<String> {
        let rc_src = rc_src.to_owned();
        let t = crate::repl::engine(move |shell| {
            ral_core::builtins::register(shell, crate::PRELUDE.comp());
            let pairs = Mutex::new(Some(rc_map(shell, &rc_src)));
            let door = BuiltinEntry::new(
                Cow::Borrowed("_apply-rc"),
                unit_thunk,
                "_apply-rc  — test door applying one rc map.",
                BuiltinBody::Captured(Arc::new(move |_, mooring, shell| {
                    let pairs = pairs.lock().unwrap().take().expect("applied once");
                    apply_rc_config(pairs, mooring, shell)
                        .expect("test rc must satisfy the keyset");
                    Ok(Value::Unit)
                })),
            );
            shell.install_captured_builtins(&vec![door].into());
        });
        let host = ReplHost::new(Arc::default());
        let _ = host.dispatch(&t, crate::repl::exec::line_run("_apply-rc"), None);
        crate::repl::plugin::lock(&host.runtime)
            .plugins
            .iter()
            .map(|p| p.name.clone())
            .collect()
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
        loaded_plugins(&rc_src).into_iter().next()
    }

    /// The value under a plugin name is its options map, forwarded verbatim
    /// as the manifest block's sole argument.  `[:]` forwards an empty map,
    /// so the plugin's own defaults stand.
    #[test]
    fn rc_plugin_options_are_forwarded() {
        // Echoes an option back as its manifest name, so the name the
        // host records reports what the block received.
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
        assert!(loaded_plugins("return [plugins: [zoxide: 'alt-z']]\n").is_empty());
    }

    /// Aliases declared in rc install as alias-origin handler frames.
    #[test]
    fn aliases_install_as_handler_frames() {
        let src = "return [\n    aliases: [\n        greet: { |args| echo hello ...$args },\n        ll: { |args| ls -lh ...$args },\n    ],\n]\n";
        let (shell, _) = apply_rc_inner(src);
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

    /// A non-positive `recursion_limit` refuses the whole map — agreeing
    /// with the record spelling, which fails the same way statically —
    /// naming the field rather than letting `0` through to disable the cap.
    #[test]
    fn rc_recursion_limit_zero_rejected() {
        let (shell, err) = apply_to_fresh_env_rejected(Value::map(vec![(
            "recursion_limit".into(),
            Value::Int(0),
        )]));
        assert_eq!(shell.stack_limit(), ral_core::types::DEFAULT_STACK_LIMIT);
        assert!(err.contains("recursion_limit") && err.contains("not applied"));
    }

    /// A wrong-typed `recursion_limit` refuses the whole map; the default
    /// stays untouched.
    #[test]
    fn rc_recursion_limit_wrong_type_rejected() {
        let (shell, err) = apply_to_fresh_env_rejected(Value::map(vec![(
            "recursion_limit".into(),
            Value::string("lots"),
        )]));
        assert_eq!(shell.stack_limit(), ral_core::types::DEFAULT_STACK_LIMIT);
        assert!(err.contains("recursion_limit"));
    }

    /// Both an unrecognised string and a wrong-typed `edit_mode` refuse the
    /// whole map, naming the field.
    #[test]
    fn rc_edit_mode_invalid_rejected() {
        let (_, err) = apply_to_fresh_env_rejected(Value::map(vec![(
            "edit_mode".into(),
            Value::string("typo"),
        )]));
        assert!(err.contains("edit_mode"));

        let (_, err) =
            apply_to_fresh_env_rejected(Value::map(vec![("edit_mode".into(), Value::Int(3))]));
        assert!(err.contains("edit_mode"));
    }

    /// A wrong-typed `bell` refuses the whole map, naming the field.
    #[test]
    fn rc_bell_wrong_type_rejected() {
        let (_, err) =
            apply_to_fresh_env_rejected(Value::map(vec![("bell".into(), Value::string("yes"))]));
        assert!(err.contains("bell"));
    }

    // ── apply_rc_config: bindings / aliases routing ───────────────────────

    /// Apply `config` to a fresh shell via `apply_rc_config` and return the shell.
    fn apply_to_fresh_env(config: Value) -> Shell {
        apply_to_fresh_env_full(config).0
    }

    /// Apply `config` to a fresh shell via `apply_rc_config` and return the
    /// full post-application state: shell and resolved settings.
    fn apply_to_fresh_env_full(config: Value) -> (Shell, RcSettings) {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let Value::Map(pairs) = config else {
            panic!("test rc config must be a map; got {}", config.type_name());
        };
        let (settings, _) = apply_rc_config(pairs, &Mooring::adrift(), &mut shell)
            .expect("test rc must satisfy the keyset");
        (shell, settings)
    }

    /// Apply `config` to a fresh shell, expecting `apply_rc_config` to
    /// refuse it — the map spelling's contract door, exercised the way
    /// `apply_to_fresh_env_full` exercises success.  Returns the shell
    /// (untouched by the refused keys) and the refusal text.
    fn apply_to_fresh_env_rejected(config: Value) -> (Shell, String) {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let Value::Map(pairs) = config else {
            panic!("test rc config must be a map; got {}", config.type_name());
        };
        let Err(err) = apply_rc_config(pairs, &Mooring::adrift(), &mut shell) else {
            panic!("test rc must fail its contract");
        };
        (shell, err)
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
    /// name refuses the whole map rather than being silently ignored.
    #[test]
    fn rc_surface_unknown_rejected_default_retained() {
        assert_eq!(
            apply_rc_surface("return [env: [X: 'y']]\n"),
            Surface::default()
        );
        let (_, err) = apply_to_fresh_env_rejected(Value::map(vec![(
            "surface".into(),
            Value::string("bogus"),
        )]));
        assert!(err.contains("surface"));
    }

    /// A wrong-typed `surface:` refuses the whole map, naming the field.
    #[test]
    fn rc_surface_wrong_type_rejected() {
        let (_, err) =
            apply_to_fresh_env_rejected(Value::map(vec![("surface".into(), Value::Int(7))]));
        assert!(err.contains("surface"));
    }

    #[test]
    fn rc_bindings_populate_value_namespace() {
        let shell = apply_to_fresh_env(Value::map(vec![(
            "bindings".into(),
            Value::map(vec![
                ("greeting".into(), Value::string("hello")),
                ("n".into(), Value::Int(42)),
            ]),
        )]));
        assert_eq!(
            shell.scope_lookup("greeting"),
            Some(&Value::string("hello"))
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

    /// A wrong-typed `plugins:` value refuses the whole map before any
    /// plugin load is even attempted.
    #[test]
    fn rc_plugins_wrong_type_rejected() {
        let (_, err) =
            apply_to_fresh_env_rejected(Value::map(vec![("plugins".into(), Value::Int(7))]));
        assert!(err.contains("plugins"));
    }

    /// Wrong-typed `env:`, `aliases:`, and `bindings:` values each refuse
    /// the whole map; no alias installs and neither scope lookup resolves.
    #[test]
    fn rc_env_aliases_bindings_wrong_type_rejected() {
        let (shell, _) = apply_to_fresh_env_rejected(Value::map(vec![
            ("env".into(), Value::Int(7)),
            ("aliases".into(), Value::string("x")),
            ("bindings".into(), Value::Bool(true)),
        ]));
        assert!(!shell.has_alias("x"));
        assert!(shell.scope_lookup("x").is_none());
    }

    /// A malformed *value* on a known key refuses the whole map, exactly as
    /// an unknown key does: a key around it does not survive either.
    #[test]
    fn rc_bad_value_fails_the_whole_map() {
        let (shell, err) = apply_to_fresh_env_rejected(Value::map(vec![
            ("edit_mode".into(), Value::Int(42)),
            ("recursion_limit".into(), Value::Int(256)),
        ]));
        assert_eq!(shell.stack_limit(), ral_core::types::DEFAULT_STACK_LIMIT);
        assert!(err.contains("edit_mode") && err.contains("not applied"));
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
        let (shell, _) = apply_rc_inner(src);
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
        let (shell, _) = apply_rc_inner(src);
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

    /// Every key `Form::Rc`'s table declares reaches its own arm in
    /// `apply_rc_key`: fed a value of the wrong shape (`Unit`), a handled
    /// key refuses — or, like `startup`, accepts anything — with its own
    /// message, never the table's `unknown_key` wording.  A `Holds::Refused`
    /// key is not reachable in `Form::Rc` today, but the loop honours it the
    /// same way the other two doors' drift tests do, so adding one here
    /// needs no new test.
    #[test]
    fn every_declared_rc_key_is_handled_by_apply_rc_key() {
        let table = ral_core::typecheck::contract::declared(Form::Rc);
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        for key in table.keys {
            let mut settings = RcSettings::default();
            let mut startup = None;
            let result = apply_rc_key(
                key.label,
                Value::Unit,
                &Mooring::adrift(),
                &mut shell,
                &mut settings,
                &mut startup,
            );
            let unknown = table.unknown_key(key.label);
            match &key.holds {
                ral_core::typecheck::contract::Holds::Refused(advice) => {
                    let err = result.expect_err("refused key must error");
                    assert_eq!(&err.message, *advice);
                }
                _ => {
                    if let Err(err) = &result {
                        assert_ne!(
                            err.message, unknown,
                            "key '{}' fell through to apply_rc_key's unknown arm",
                            key.label
                        );
                    }
                }
            }
        }
    }
}
