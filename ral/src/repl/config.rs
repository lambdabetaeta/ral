//! RC file discovery, parsing, and application.
//!
//! An rc file is ral source whose return value is a configuration record, or
//! `()` for a file with nothing to set.  Its eleven keys are declared once, in
//! `Form::Rc`'s table, and the checker ascribes that table to the file's
//! inferred return type, so an unknown key or a wrong field type is a static
//! error and the whole file is skipped.  A file whose return the checker
//! cannot type (decoded JSON) meets the same keyset and field types in
//! [`RcPlan::decode`].
//!
//! The rc is a plan, decoded then committed: `decode` makes every fallible
//! judgement reading the shell only, so a refused rc has changed nothing;
//! `apply` then commits.  All of it runs engine-side, inside the boot door;
//! the host is told only the [`RcSettings`] its frontend needs.

pub(crate) mod source;

use ral_core::errln;
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum;
use ral_core::record;
use ral_core::ty::{Ty, Typed};
use ral_core::typecheck::Form;
use ral_core::types::{
    Break, DefaultPolicy, Error, HandlerEntry, Hook, HookName, HookSig, Map, Mooring, Settled,
};
use ral_core::{Shell, Value};
use std::io::Write as _;
use std::path::PathBuf;

use super::errfmt::report_failed_load;
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

record!(typed RcSettings {
    edit_mode: "edit-mode",
    bell: "bell",
    surface: "surface",
    theme: "theme",
    startup: "startup",
});

impl Typed for Surface {
    fn ty() -> Ty {
        Ty::String
    }
}

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
            .map_err(|_| format!("expected minimal or readline, got '{name}'"))
    }
}

// ── Default RC skeleton ──────────────────────────────────────────────────

const DEFAULT_RC: &str = "\
# ~/.config/ral/rc — ral shell configuration
#
# This file must return a record, or `()` to leave everything at its default;
# all keys are optional.  To customise, uncomment the `return [` … `]` block
# with the sections you want, and delete the final `return ()`.

# return [
    # edit-mode:        vi,          # emacs (default) or vi
    # bell:             false,       # audible bell on readline error (default false)
    # surface:          readline,    # readline (default) or minimal
    # recursion-limit:  100000,      # maximum machine-frame recursion depth

    # prompt: {
    #     return \"!{abbreviate-home !{cwd}} $ \"
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
    #     value-prefix: \"=> \",
    #     value-color:  yellow,   # black red green yellow blue magenta cyan white none
    # ],
# ]

return ()
";

/// Write the default RC skeleton to the first resolvable config location,
/// unless a concurrent boot got there first, in which case that file stands.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:rc-write] persists the default repl config dir + rc file; not turn-time model I/O"
)]
pub(super) fn create_default_rc() -> Option<PathBuf> {
    let (dir, path) = ral_core::path::app_dir(ral_core::path::XdgKind::Config, "ral")
        .map(|dir| {
            let file = dir.join("rc");
            (dir, file)
        })
        .or_else(|| {
            let dot = ral_core::host::home_dot(".ralrc")?;
            // Legacy single-file layout: the "dir" is the home directory, since
            // there is no per-app subdirectory to create before
            // writing `.ralrc` itself.
            let dir = dot.parent()?.to_path_buf();
            Some((dir, dot))
        })?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        errln!(
            "ral: warning: could not create config directory {}: {e}",
            ral_core::path::shown(&dir)
        );
        return None;
    }
    let name = ral_core::path::shown(&path);
    match std::fs::File::create_new(&path).and_then(|mut f| f.write_all(DEFAULT_RC.as_bytes())) {
        Ok(()) => errln!("note: created {name}"),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            errln!("ral: warning: could not write {name}: {e}");
            return None;
        }
    }
    Some(path)
}

// ── RC config: decoded, then committed ───────────────────────────────────

/// The trailer every refusal of a decoded rc carries: a bad key or a bad
/// value refuses the whole rc, and nothing of it was applied.
fn refuse(reason: impl std::fmt::Display) -> String {
    format!("rc: {reason}. The rc was not applied; the shell started with defaults.")
}

/// A fully judged rc: every field already has its final shape and every hook
/// and alias has passed the shell's gate, so [`Self::apply`] refuses only a
/// shell that is not the session, before committing anything.
#[derive(Default)]
pub(crate) struct RcPlan {
    settings: RcSettings,
    stack_limit: Option<usize>,
    env: Map,
    bindings: Map,
    plugins: Map,
    aliases: Vec<HandlerEntry>,
    hooks: Vec<(HookName, Hook)>,
}

impl RcPlan {
    /// Judge the rc's map against `shell`, which it only reads: the keyset,
    /// every field's shape, the theme, the prompt and startup blocks, and each
    /// alias.  The map contract (and the diagnostic for breaking it) lives
    /// with the sourcing in [`source`].
    pub(crate) fn decode(pairs: &Map, shell: &Shell) -> Result<Self, String> {
        let table = ral_core::typecheck::contract::declared(Form::Rc);
        let mut plan = Self::default();
        for (key, val) in pairs {
            match (key, val.into_owned()) {
                ("env", Value::Map(m)) => plan.env = m,
                ("bindings", Value::Map(m)) => plan.bindings = m,
                ("plugins", Value::Map(m)) => plan.plugins = m,
                ("theme", Value::Map(m)) => {
                    plan.settings.theme = OutputTheme::from_map(&m).map_err(refuse)?;
                }
                ("aliases", Value::Map(m)) => {
                    plan.aliases = m
                        .iter()
                        .map(|(name, f)| match f.into_owned() {
                            f @ Value::Thunk(_) => shell.vet_alias(name.to_string(), f).map_err(|b| {
                                refuse(match b {
                                    Break::Error(e) => e.context(format!("alias '{name}'")).message,
                                    Break::Escape(_) => format!("alias '{name}' escaped"),
                                })
                            }),
                            other => Err(refuse(format!(
                                "alias '{name}' is {}, not a function; did you mean to put it under `bindings:`?",
                                other.type_name()
                            ))),
                        })
                        .collect::<Result<_, _>>()?;
                }
                ("edit-mode", Value::String(s)) => {
                    plan.settings.edit_mode = match s.to_ascii_lowercase().as_str() {
                        "vi" => Keymap::Vi,
                        "emacs" => Keymap::Emacs,
                        _ => {
                            return Err(refuse(format!(
                                "'edit-mode' must be 'emacs' or 'vi'; got '{s}'"
                            )));
                        }
                    }
                }
                ("bell", Value::Bool(b)) => plan.settings.bell = b,
                ("surface", Value::String(s)) => {
                    plan.settings.surface = <Surface as clap::ValueEnum>::from_str(&s, true)
                        .map_err(|_| {
                            refuse(format!("'surface' must be minimal or readline; got '{s}'"))
                        })?;
                }
                ("recursion-limit", Value::Int(n)) => {
                    plan.stack_limit =
                        Some(usize::try_from(n).ok().filter(|&n| n > 0).ok_or_else(|| {
                            refuse(format!("'recursion-limit' must be positive; got {n}"))
                        })?);
                }
                (k @ ("prompt" | "startup"), f) => {
                    let name = HookName::session(k);
                    let policy = match k {
                        "prompt" => DefaultPolicy::denied_capture(),
                        _ => DefaultPolicy::denied(),
                    };
                    let hook = shell
                        .vet_hook(&name, f, HookSig::Block, policy)
                        .map_err(|e| refuse(format!("'{k}': {e}")))?;
                    plan.settings.startup |= k == "startup";
                    plan.hooks.push((name, hook));
                }
                (k, other) if table.holds(k).is_some() => {
                    let want = match k {
                        "env" | "bindings" | "aliases" | "theme" => "a map",
                        "plugins" => {
                            "a map from plugin name to its options, e.g. [zoxide: [key: 'alt-z'], autosuggestion: [:]]"
                        }
                        "edit-mode" => "'emacs' or 'vi'",
                        "bell" => "a bool",
                        "surface" => "minimal or readline",
                        _ => "a positive int",
                    };
                    return Err(refuse(format!(
                        "'{k}' must be {want}; got {}",
                        other.type_name()
                    )));
                }
                (k, _) => return Err(refuse(table.unknown_key(k))),
            }
        }
        Ok(plan)
    }

    /// Commit the plan, loading its plugins under `mooring`.  The one refusal
    /// (a non-session shell) comes before anything is committed; a plugin that
    /// fails to load is reported and skipped, and only an `exit` escape propagates.
    pub(crate) fn apply(self, mooring: &Mooring, shell: &mut Shell) -> Settled<RcSettings> {
        shell
            .set_hooks(self.hooks)
            .map_err(|e| Break::Error(Error::new(e.to_string())))?;
        // PWD lives on context.cwd and a copy in env_overrides would shadow it;
        // ral keeps no OLDPWD.  An rc spreading a parent's environment carries both.
        for (k, v) in self
            .env
            .iter()
            .filter(|(k, _)| !matches!(*k, "PWD" | "OLDPWD"))
        {
            let v = v.into_owned();
            shell.set_env_var(k.to_string(), v.to_string());
            shell.set_var(k.to_string(), v);
        }
        // Bindings, functions included, are lexical: applyable at the prompt.
        for (k, v) in &self.bindings {
            shell.bind_value(k.to_string(), v.into_owned());
        }
        for entry in self.aliases {
            shell.commit_alias(entry);
        }
        if let Some(n) = self.stack_limit {
            shell.set_stack_limit(n);
        }
        for (name, options) in &self.plugins {
            let loaded = match options.into_owned() {
                Value::Map(o) => super::plugin::load::load_plugin(name, &o, mooring, shell),
                other => Err(Break::Error(Error::new(format!(
                    "the value under a plugin name is its options map; got {}. \
                     Write `{name}: [:]` if it takes no options.",
                    other.type_name()
                )))),
            };
            match loaded {
                Err(Break::Error(e)) => {
                    let what = format!("plugin '{name}'");
                    report_failed_load(shell, &what, &e.context(&what), None);
                }
                Err(escape) => return Err(escape),
                Ok(()) => {}
            }
        }
        Ok(self.settings)
    }
}

// ── Config file locations ────────────────────────────────────────────────

/// Search for an existing RC file in the standard locations.
pub(super) fn find_ralrc() -> Option<PathBuf> {
    let candidates = [
        ral_core::path::app_dir(ral_core::path::XdgKind::Config, "ral").map(|d| d.join("rc")),
        ral_core::host::home_dot(".ralrc"),
    ];
    candidates.into_iter().flatten().find(|path| path.exists())
}

/// Resolve the history file path, creating the config directory if needed.
pub(super) fn dirs_history() -> Option<String> {
    #[allow(
        clippy::disallowed_methods,
        reason = "native: the history path is handed to the line editor, which opens it"
    )]
    let native = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
    if let Some(dir) = ral_core::path::app_dir(ral_core::path::XdgKind::Config, "ral") {
        #[allow(
            clippy::disallowed_methods,
            reason = "[silent:history-mkdir] ensures the repl config dir exists for the history file; not turn-time model I/O"
        )]
        let _ = std::fs::create_dir_all(&dir);
        return Some(native(dir.join("history")));
    }
    ral_core::host::home_dot(".ral_history").map(native)
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
        Shell::root(
            crate::repl::prelude_runtime().into(),
            ral_core::test_helper::process_cwd(),
        )
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

    /// Decode then commit, as the boot door does.
    fn commit(pairs: &Map, mooring: &Mooring, shell: &mut Shell) -> Result<RcSettings, String> {
        let plan = RcPlan::decode(pairs, shell)?;
        Ok(plan
            .apply(mooring, shell)
            .expect("a test shell is a session"))
    }

    /// Evaluate `rc_src`, apply it through `RcPlan`, and return the
    /// resulting shell and settings.  Registers the baked prelude so the rc
    /// sees the environment it sees at real startup.
    fn apply_rc_inner(rc_src: &str) -> (Shell, RcSettings) {
        let mut shell = prelude_shell();
        let pairs = rc_map(&mut shell, rc_src);
        let settings = commit(&pairs, &Mooring::adrift(), &mut shell)
            .expect("test rc must satisfy the keyset");
        (shell, settings)
    }

    fn apply_rc(rc_src: &str) -> Shell {
        apply_rc_inner(rc_src).0
    }

    fn unit_thunk(_u: &mut ral_core::typecheck::Unifier) -> ral_core::Scheme {
        use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
        mk_scheme(&[], &[], thunk(pure(ral_core::ty::Ty::Unit)))
    }

    /// Apply `rc_src` inside a dispatch with the REPL host, as the boot door
    /// does, so a plugin load has a desk to tell; the host's plugins after.
    fn loaded_plugins(rc_src: &str) -> Vec<String> {
        let rc_src = rc_src.to_owned();
        let pairs: Arc<Mutex<Option<Map>>> = Arc::default();
        let door = BuiltinEntry::new(
            Cow::Borrowed("_apply-rc"),
            unit_thunk,
            "_apply-rc  — test door applying one rc map.",
            BuiltinBody::Captured(Arc::new({
                let pairs = pairs.clone();
                move |_, mooring, shell| {
                    let pairs = pairs.lock().unwrap().take().expect("applied once");
                    commit(&pairs, mooring, shell).expect("test rc must satisfy the keyset");
                    Ok(Value::Unit)
                }
            })),
        );
        let mut runtime = crate::repl::prelude_runtime();
        runtime.install_captured_builtins(&vec![door].into());
        let t = crate::repl::engine(runtime, move |shell| {
            *pairs.lock().unwrap() = Some(rc_map(shell, &rc_src));
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
        assert_eq!(
            loaded_plugins("return [plugins: [zoxide: 'alt-z']]\n"),
            Vec::<String>::new()
        );
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

    /// rc `recursion-limit:` overrides the default on the shell.
    #[test]
    fn rc_recursion_limit_applied() {
        let shell = apply_rc("return [recursion-limit: 256]\n");
        assert_eq!(shell.stack_limit(), 256);
    }

    /// A non-positive `recursion-limit` refuses the whole map — agreeing
    /// with the record spelling, which fails the same way statically —
    /// naming the field rather than letting `0` through to disable the cap.
    #[test]
    fn rc_recursion_limit_zero_rejected() {
        let (shell, err) = apply_to_fresh_env_rejected(Value::map(vec![(
            "recursion-limit".into(),
            Value::Int(0),
        )]));
        assert_eq!(shell.stack_limit(), ral_core::types::DEFAULT_STACK_LIMIT);
        assert!(err.contains("recursion-limit") && err.contains("not applied"));
    }

    /// A wrong-typed `recursion-limit` refuses the whole map; the default
    /// stays untouched.
    #[test]
    fn rc_recursion_limit_wrong_type_rejected() {
        let (shell, err) = apply_to_fresh_env_rejected(Value::map(vec![(
            "recursion-limit".into(),
            Value::string("lots"),
        )]));
        assert_eq!(shell.stack_limit(), ral_core::types::DEFAULT_STACK_LIMIT);
        assert!(err.contains("recursion-limit"));
    }

    /// Both an unrecognised string and a wrong-typed `edit-mode` refuse the
    /// whole map, naming the field.
    #[test]
    fn rc_edit_mode_invalid_rejected() {
        let (_, err) = apply_to_fresh_env_rejected(Value::map(vec![(
            "edit-mode".into(),
            Value::string("typo"),
        )]));
        assert!(err.contains("edit-mode"));

        let (_, err) =
            apply_to_fresh_env_rejected(Value::map(vec![("edit-mode".into(), Value::Int(3))]));
        assert!(err.contains("edit-mode"));
    }

    /// A wrong-typed `bell` refuses the whole map, naming the field.
    #[test]
    fn rc_bell_wrong_type_rejected() {
        let (_, err) =
            apply_to_fresh_env_rejected(Value::map(vec![("bell".into(), Value::string("yes"))]));
        assert!(err.contains("bell"));
    }

    // ── RcPlan: bindings / aliases routing ───────────────────────

    /// Apply `config` to a fresh shell via `RcPlan` and return the shell.
    fn apply_to_fresh_env(config: Value) -> Shell {
        apply_to_fresh_env_full(config).0
    }

    /// Apply `config` to a fresh shell via `RcPlan` and return the
    /// full post-application state: shell and resolved settings.
    fn apply_to_fresh_env_full(config: Value) -> (Shell, RcSettings) {
        let mut shell = ral_core::test_helper::core_shell();
        let Value::Map(pairs) = config else {
            panic!("test rc config must be a map; got {}", config.type_name());
        };
        let settings = commit(&pairs, &Mooring::adrift(), &mut shell)
            .expect("test rc must satisfy the keyset");
        (shell, settings)
    }

    /// Apply `config` to a fresh shell, expecting `RcPlan::decode` to
    /// refuse it — the decoded spelling's contract door, exercised the way
    /// `apply_to_fresh_env_full` exercises success.  Returns the shell
    /// (untouched by the refused keys) and the refusal text.
    fn apply_to_fresh_env_rejected(config: Value) -> (Shell, String) {
        let mut shell = ral_core::test_helper::core_shell();
        let Value::Map(pairs) = config else {
            panic!("test rc config must be a map; got {}", config.type_name());
        };
        let Err(err) = commit(&pairs, &Mooring::adrift(), &mut shell) else {
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

    /// A non-function under `aliases:` refuses the rc, pointing at `bindings:`.
    #[test]
    fn rc_non_thunk_alias_points_at_bindings() {
        let (shell, err) = apply_to_fresh_env_rejected(Value::map(vec![(
            "aliases".into(),
            Value::map(vec![("ll".into(), Value::Bool(true))]),
        )]));
        assert!(
            err.contains("`bindings:`") && err.contains("not applied"),
            "{err}"
        );
        assert!(shell.scope_lookup("ll").is_none() && !shell.has_alias("ll"));
    }

    /// A refused rc leaves no prompt hook behind: valid prompt, bad theme colour.
    #[test]
    fn refused_rc_registers_no_prompt() {
        let mut shell = prelude_shell();
        let pairs = rc_map(
            &mut shell,
            "return [prompt: { return 'x' }, theme: [value-color: 'purple']]\n",
        );
        let Err(err) = commit(&pairs, &Mooring::adrift(), &mut shell) else {
            panic!("theme refuses");
        };
        assert!(
            err.contains("value-color") && err.contains("not applied"),
            "{err}"
        );
        assert!(!shell.has_hook(&HookName::session("prompt")));
    }

    /// A refused rc installs none of its aliases: one good, one non-thunk.
    #[test]
    fn refused_rc_installs_no_aliases() {
        let mut shell = prelude_shell();
        let pairs = rc_map(
            &mut shell,
            "return [aliases: [ll: { |args| ls ...$args }, bad: 3]]\n",
        );
        assert!(commit(&pairs, &Mooring::adrift(), &mut shell).is_err());
        assert!(!shell.has_alias("ll") && !shell.has_alias("bad"));
    }

    /// An unknown `theme:` key refuses the rc rather than warning.
    #[test]
    fn rc_unknown_theme_key_rejected() {
        let (_, err) = apply_to_fresh_env_rejected(Value::map(vec![(
            "theme".into(),
            Value::map(vec![("wat".into(), Value::Int(1))]),
        )]));
        assert!(err.contains("wat") && err.contains("not applied"), "{err}");
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
            ("edit-mode".into(), Value::Int(42)),
            ("recursion-limit".into(), Value::Int(256)),
        ]));
        assert_eq!(shell.stack_limit(), ral_core::types::DEFAULT_STACK_LIMIT);
        assert!(err.contains("edit-mode") && err.contains("not applied"));
    }

    /// Typecheck `src` against `shell`'s live session schemes — the same
    /// seed a real prompt run uses — and return the errors.
    fn typecheck_against_session(shell: &Shell, src: &str) -> Vec<ral_core::TypeError> {
        let ast = ral_core::syntax::parser::parse(src).unwrap();
        let comp = ral_core::elaborator::elaborate(&ast, [], None).expect("elaborate");
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
    /// `RcPlan::decode`: fed a value of the wrong shape (`Unit`), it refuses
    /// with its own message, never the table's `unknown_key` wording.
    #[test]
    fn every_declared_rc_key_is_handled_by_decode() {
        let table = ral_core::typecheck::contract::declared(Form::Rc);
        let shell = ral_core::test_helper::core_shell();
        for key in table.keys {
            let pairs = Map::from_iter([(key.label.to_string(), Value::Unit)]);
            let err = RcPlan::decode(&pairs, &shell)
                .err()
                .expect("Unit satisfies no key");
            assert!(
                !err.contains(&table.unknown_key(key.label)),
                "key '{}' fell through",
                key.label
            );
        }
    }
}
