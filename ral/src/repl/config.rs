//! RC file discovery, parsing, and application.
//!
//! An rc file ends in a written record of all twelve keys, checked against
//! `Form::Rc`'s table during generation. Nested tables are written too.
//! [`RcPlan::decode`] checks value shapes, domains and installation gates;
//! the checker has already guaranteed the keysets.
//!
//! The rc is a plan, decoded then committed: `decode` makes every fallible
//! judgement reading the shell only, so a refused rc has changed nothing;
//! `apply` then commits.  All of it runs engine-side, inside the boot builtin;
//! the host is told only the [`RcSettings`] its frontend needs.

pub(crate) mod source;

use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::environ::Reserved;
use ral_core::errln;
use ral_core::error::Error;
use ral_core::fact::{
    DataStep, DecodeCause, DecodeFailure, Expected, Found, InvalidCause, InvalidFailure, Reason,
    ShapeFailure, ValueShape,
};
use ral_core::own::{OwnDir, UserFile};
use ral_core::path::{XdgKind, shown};
use ral_core::record;
use ral_core::shell::runtime::{
    Break, DefaultPolicy, HandlerEntry, Hook, HookName, HookSig, Mooring, Settled,
};
use ral_core::text::code;
use ral_core::ty::{Ty, Typed};
use ral_core::{MapRef, RegisterError, Shell, VClosRef, ValClos, View, io::terminal};
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
        Ty::string()
    }
}

impl Datum for Surface {
    fn encode(self) -> FOValue {
        clap::ValueEnum::to_possible_value(&self)
            .map(|v| v.get_name().to_string())
            .unwrap_or_default()
            .encode()
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let name = String::decode(v)?;
        <Self as clap::ValueEnum>::from_str(&name, true).map_err(|_| {
            DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::StringLiterals(vec!["minimal".into(), "readline".into()]),
                found: Found::of(v),
            }))
        })
    }
}

// ── Default RC skeleton ──────────────────────────────────────────────────

const DEFAULT_RC: &str = "\
# ~/.config/ral/rc: ral shell configuration
# Write every key. `inherit keeps the current value; `set v replaces it.
return [
    umask: `inherit, # Set octal permission bits, e.g. `set '022'.
    limits: [open-files: `inherit, core-bytes: `inherit, stack-bytes: `inherit], # Child soft limits.
    env: [:], # Export variables, e.g. [EDITOR: vim].
    prompt: `inherit, # Set a block returning prompt text.
    handlers: [:], # Command handlers, e.g. [:, ll: { |args| ls -lh ...$args }].
    edit-mode: emacs, # emacs or vi.
    bell: false, # Ring on readline errors.
    surface: readline, # readline or minimal; the command-line flag wins.
    recursion-limit: `inherit, # Set a positive machine-frame limit.
    plugins: [:], # Plugin names or paths mapped to options; [:] uses plugin defaults.
    startup: `inherit, # Set a block to run once before the first prompt.
    theme: [value-prefix: `inherit, value-color: `inherit], # Set text and a colour name or none.
]
";

// ── RC config: decoded, then committed ───────────────────────────────────

/// A bad value refuses the whole rc before anything is applied.
fn refuse(reason: impl std::fmt::Display) -> String {
    format!("rc: {reason}. The rc was not applied; the shell started with defaults.")
}

/// Decode the tag only; each setting checks its own payload.
pub(super) fn decode_setting<'a>(
    value: VClosRef<'a>,
    key: &str,
) -> Result<Option<VClosRef<'a>>, String> {
    match value.view() {
        View::Variant { label, payload } if label.as_ref() == "inherit" && payload.is_unit() => {
            Ok(None)
        }
        View::Variant { label, payload } if label.as_ref() == "set" => Ok(Some(payload)),
        _ => Err(format!(
            "{} must be {} (no payload) or {} (one payload); got {value} ({})",
            code(key),
            code("`inherit"),
            code("`set v"),
            value.type_name()
        )),
    }
}

/// A fully judged rc: every field already has its final shape and every hook
/// and handler has passed the shell's gate, so [`Self::apply`] refuses only a
/// shell that is not the session, before committing anything.
#[derive(Default)]
pub(crate) struct RcPlan {
    settings: RcSettings,
    stack_limit: Option<usize>,
    #[cfg(unix)]
    umask: Option<rustix::fs::Mode>,
    child_limits: ral_core::process::ChildLimits,
    env: Vec<(String, ValClos)>,
    plugins: Vec<(String, ValClos)>,
    handlers: Vec<HandlerEntry>,
    hooks: Vec<(HookName, Hook)>,
}

impl RcPlan {
    /// Judge values against `shell`, which it only reads. The map's keyset
    /// comes from a file checked against the rc table (SPEC §15.3).
    pub(crate) fn decode(pairs: MapRef<'_>, shell: &Shell) -> Result<Self, String> {
        let mut plan = Self::default();
        for (key, value) in pairs.iter() {
            let value = match key {
                "umask" | "recursion-limit" | "prompt" | "startup" => {
                    let Some(value) = decode_setting(value, key).map_err(refuse)? else {
                        continue;
                    };
                    value
                }
                _ => value,
            };
            match (key, value.view()) {
                #[cfg(windows)]
                ("umask", _) => return Err(refuse("'umask' is not supported on Windows")),
                #[cfg(unix)]
                ("umask", View::String(s)) => plan.umask = Some(decode_umask(s)?),
                ("limits", View::Map(m)) => plan.child_limits = decode_limits(m)?,
                ("env", View::Map(m)) => plan.env = owned_pairs(m),
                ("plugins", View::Map(m)) => plan.plugins = owned_pairs(m),
                ("theme", View::Map(m)) => {
                    plan.settings.theme = OutputTheme::from_map(m).map_err(refuse)?;
                }
                ("handlers", View::Map(m)) => {
                    plan.handlers = m
                        .iter()
                        .map(|(name, f)| {
                            shell
                                .vet_handler(name.to_string(), f.into_owned())
                                .map_err(|b| {
                                    refuse(match b {
                                        Break::Error(e) => {
                                            format!("handler '{name}': {}", e.describe())
                                        }
                                        Break::Escape(_) => format!("handler '{name}' escaped"),
                                    })
                                })
                        })
                        .collect::<Result<_, _>>()?;
                }
                ("edit-mode", View::String(s)) => {
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
                ("bell", View::Bool(b)) => plan.settings.bell = b,
                ("surface", View::String(s)) => {
                    plan.settings.surface = <Surface as clap::ValueEnum>::from_str(s, true)
                        .map_err(|_| {
                            refuse(format!("'surface' must be minimal or readline; got '{s}'"))
                        })?;
                }
                ("recursion-limit", View::Int(n)) => {
                    plan.stack_limit =
                        Some(usize::try_from(n).ok().filter(|&n| n > 0).ok_or_else(|| {
                            refuse(format!("'recursion-limit' must be positive; got {n}"))
                        })?);
                }
                (k @ ("prompt" | "startup"), _) => {
                    let name = HookName::session(k);
                    let policy = match k {
                        "prompt" => DefaultPolicy::denied_capture(),
                        _ => DefaultPolicy::denied(),
                    };
                    let hook = shell
                        .vet_hook(&name, value.into_owned(), HookSig::Block, policy)
                        .map_err(|e| refuse(format!("'{k}': {e}")))?;
                    plan.settings.startup |= k == "startup";
                    plan.hooks.push((name, hook));
                }
                (k, _) => {
                    let want = match k {
                        "limits" | "theme" => {
                            unreachable!("nested rc tables are written records (SPEC §15.3)")
                        }
                        "env" | "handlers" => "a map",
                        "plugins" => {
                            "a map from plugin name to its options, e.g. [zoxide: [key: 'alt-z'], autosuggestion: [:]]"
                        }
                        "edit-mode" => "'emacs' or 'vi'",
                        "bell" => "a bool",
                        "umask" => "a String of octal digits, e.g. '077'",
                        "surface" => "minimal or readline",
                        "recursion-limit" => "a positive Int",
                        _ => unreachable!("the checker admits only rc's keys (SPEC §15.3)"),
                    };
                    return Err(refuse(format!(
                        "'{k}' must be {want}; got {}",
                        value.type_name()
                    )));
                }
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
            .map_err(RegisterError::into_error)?;
        #[cfg(unix)]
        if let Some(mask) = self.umask {
            rustix::process::umask(mask);
        }
        shell.set_child_limits(self.child_limits);
        for (k, v) in &self.env {
            match shell.set_env_var(k.clone(), v.to_string()) {
                // Ignored: an rc copying a parent's environment carries both.
                Ok(()) | Err(Reserved::Pwd | Reserved::Oldpwd) => {}
            }
        }
        for entry in self.handlers {
            shell.commit_handler(entry);
        }
        if let Some(n) = self.stack_limit {
            shell.set_stack_limit(n);
        }
        for (name, options) in &self.plugins {
            let loaded = load_configured_plugin(name, options, mooring, shell);
            match loaded {
                Err(Break::Error(e)) => {
                    let what = format!("plugin '{name}'");
                    report_failed_load(shell, &what, &e.with_context(&what), None);
                }
                Err(escape) => return Err(escape),
                Ok(()) => {}
            }
        }
        Ok(self.settings)
    }
}

fn load_configured_plugin(
    name: &str,
    options: &ValClos,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<()> {
    match options.view() {
        View::Map(options) => super::plugin::load::load_plugin(name, options, mooring, shell),
        _ => Err(Error::new(Reason::Invalid(Box::new(InvalidFailure {
            operation: "load-plugin".into(),
            path: vec![DataStep::Field(name.into())],
            cause: InvalidCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Map),
                found: options.found(),
            }),
        })))
        .into()),
    }
}

#[cfg(unix)]
fn decode_umask(s: &str) -> Result<rustix::fs::Mode, String> {
    let invalid = || {
        refuse(format!(
            "'umask' must be octal digits such as '077' or '0022'; got '{s}'"
        ))
    };
    ((1..=4).contains(&s.len()) && s.bytes().all(|b| (b'0'..=b'7').contains(&b)))
        .then_some(s)
        .and_then(|s| rustix::fs::RawMode::from_str_radix(s, 8).ok())
        .filter(|&mask| mask <= 0o777)
        .map(rustix::fs::Mode::from_raw_mode)
        .ok_or_else(invalid)
}

fn owned_pairs(map: MapRef<'_>) -> Vec<(String, ValClos)> {
    map.iter()
        .map(|(key, value)| (key.to_owned(), value.into_owned()))
        .collect()
}

fn decode_limits(map: MapRef<'_>) -> Result<ral_core::process::ChildLimits, String> {
    let mut limits = ral_core::process::ChildLimits::default();
    for (key, value) in map.iter() {
        let Some(value) = decode_setting(value, &format!("limits.{key}")).map_err(refuse)? else {
            continue;
        };
        let View::Int(limit) = value.view() else {
            return Err(refuse(format!(
                "{} payload for {} must be an Int; got {}",
                code("`set"),
                code(&format!("limits.{key}")),
                value.type_name()
            )));
        };
        set_limit(&mut limits, key, limit)?;
    }
    Ok(limits)
}

#[cfg(unix)]
fn set_limit(
    limits: &mut ral_core::process::ChildLimits,
    key: &str,
    value: i64,
) -> Result<(), String> {
    let limit = ral_core::process::ChildLimit::ALL
        .into_iter()
        .find(|limit| limit.name == key)
        .expect("the checker admits only limits' keys (SPEC §15.3)");
    let value = u64::try_from(value).map_err(|_| {
        refuse(format!(
            "{} must be non-negative; got {value}",
            code(&format!("limits.{key}"))
        ))
    })?;
    if let Some(hard) = rustix::process::getrlimit(limit.resource).maximum
        && value > hard
    {
        return Err(refuse(format!(
            "{} exceeds ral's hard limit of {hard}; got {value}",
            code(&format!("limits.{key}"))
        )));
    }
    *(limit.slot)(limits) = Some(value);
    Ok(())
}

#[cfg(windows)]
fn set_limit(
    _limits: &mut ral_core::process::ChildLimits,
    key: &str,
    value: i64,
) -> Result<(), String> {
    Err(refuse(format!(
        "{} is not supported on Windows (got {value})",
        code(&format!("limits.{key}"))
    )))
}

// ── Config file locations ────────────────────────────────────────────────

/// The rc's text and where it is: the first candidate that holds one, else the
/// skeleton, written where nothing stands at the XDG name.  `None` where
/// neither: no place for the skeleton, or something at its name that is the
/// user's.
pub(super) fn rc() -> Option<(UserFile, String)> {
    // No config base is no config directory: the rc is then `~/.ralrc` or none.
    let xdg = UserFile::xdg("ral", XdgKind::Config, "rc").ok();
    for candidate in xdg.iter().cloned().chain(UserFile::home(".ralrc")) {
        match candidate.read() {
            Ok(Some(text)) => return Some((candidate, text)),
            Ok(None) => {}
            // A dangling link, say: the next candidate is tried.
            Err(e) => terminal::cmd_error("ral", &e.to_string()),
        }
    }
    let xdg = xdg?;
    match xdg.seed(DEFAULT_RC) {
        Ok(true) => {
            errln!("note: created {}", shown(xdg.path()));
            Some((xdg, DEFAULT_RC.into()))
        }
        Ok(false) => None,
        Err(e) => {
            errln!("ral: warning: {e}");
            None
        }
    }
}

/// The history file, `history` in ral's state directory; `None` where the host
/// has no state base, and a session without one keeps no history.
pub(super) fn dirs_history() -> Option<PathBuf> {
    let state = OwnDir::xdg("ral", XdgKind::State).ok()?;
    Some(state.path().join("history"))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    use crate::repl::host::ReplHost;
    use ral_core::builtin::{BuiltinBody, BuiltinEntry};
    use ral_core::test_helper::rc_source;
    use ral_core::typecheck::Form;
    use std::borrow::Cow;
    use std::sync::{Arc, Mutex};

    fn prelude_shell() -> Shell {
        Shell::root(
            crate::repl::prelude_runtime().into(),
            ral_core::test_helper::process_cwd(),
        )
    }

    fn rc_map(shell: &mut Shell, rc_src: &str) -> ValClos {
        let value = ral_core::shell::load::extend_session(
            &Mooring::adrift(),
            shell,
            rc_src,
            &ral_core::source::SourceOrigin::named("rc.ral"),
            Some(ral_core::typecheck::table::declared(Form::Rc)),
        )
        .expect("test rc checks and evaluates");
        assert!(matches!(value.view(), View::Map(_)), "a written record");
        value
    }

    fn decode(pairs: &ValClos, shell: &Shell) -> Result<RcPlan, String> {
        RcPlan::decode(pairs.as_map("test rc").expect("a written record"), shell)
    }

    /// Decode then commit, as the boot builtin does.
    fn commit(pairs: &ValClos, mooring: &Mooring, shell: &mut Shell) -> Result<RcSettings, String> {
        let plan = decode(pairs, shell)?;
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

    fn unit_thunk(_: &mut ral_core::ty::Binders) -> ral_core::ty::Ty {
        use ral_core::typecheck::builtins::{pure, thunk};
        thunk(pure(ral_core::ty::Ty::unit()))
    }

    /// Apply `rc_src` inside a dispatch with the REPL host, as the boot builtin
    /// does, so a plugin load has a desk to tell; the host's plugins after.
    fn loaded_plugins(rc_src: &str) -> Vec<String> {
        let rc_src = rc_src.to_owned();
        let pairs: Arc<Mutex<Option<ValClos>>> = Arc::default();
        let entry = BuiltinEntry::new(
            Cow::Borrowed("_apply-rc"),
            unit_thunk,
            "_apply-rc  — test builtin applying one rc map.",
            BuiltinBody::Captured(Arc::new({
                let pairs = pairs.clone();
                move |_, mooring, shell| {
                    let pairs = pairs.lock().unwrap().take().expect("applied once");
                    commit(&pairs, mooring, shell).expect("test rc must satisfy the keyset");
                    Ok(ValClos::UNIT)
                }
            })),
        );
        let mut runtime = crate::repl::prelude_runtime();
        runtime.install_captured_builtins(&vec![entry].into());
        let t = crate::repl::in_temp::engine(runtime, move |shell| {
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
        let rc_src = rc_source(&[(
            "plugins",
            &format!("['{}': {options}]", path.to_string_lossy()),
        )]);
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
            loaded_plugins(&rc_source(&[("plugins", "[zoxide: 'alt-z']")])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn default_rc_writes_every_declared_blank_and_checks_clean() {
        let fields = DEFAULT_RC
            .lines()
            .filter_map(|line| {
                line.split('#')
                    .next()?
                    .trim()
                    .strip_suffix(',')?
                    .split_once(": ")
            })
            .collect::<Vec<_>>();
        let table = ral_core::typecheck::table::declared(Form::Rc);
        assert_eq!(fields.len(), table.keys.len());
        for key in table.keys {
            let (_, value) = fields
                .iter()
                .find(|(label, _)| *label == key.label)
                .expect("the skeleton writes every rc key");
            assert_eq!(Some(*value), key.blank, "{}", key.label);
        }
        let mut shell = prelude_shell();
        let skeleton = rc_map(&mut shell, DEFAULT_RC);
        let blanks = rc_map(&mut shell, &rc_source(&[]));
        assert_eq!(skeleton, blanks);
        assert!(decode(&skeleton, &shell).is_ok());
    }

    #[test]
    fn inherit_keeps_recursion_limit_and_builtin_hooks() {
        let (shell, settings) = apply_rc_inner(&rc_source(&[]));
        assert_eq!(
            shell.stack_limit(),
            ral_core::shell::runtime::DEFAULT_STACK_LIMIT
        );
        assert!(!settings.startup);
        assert!(!shell.has_hook(&HookName::session("prompt")));
        assert!(!shell.has_hook(&HookName::session("startup")));

        let mut shell = prelude_shell();
        shell.set_stack_limit(321);
        let pairs = rc_map(&mut shell, &rc_source(&[]));
        commit(&pairs, &Mooring::adrift(), &mut shell).unwrap();
        assert_eq!(
            shell.stack_limit(),
            321,
            "inherit keeps the command-line limit"
        );
    }

    #[test]
    fn prompt_and_startup_set_install_hooks() {
        let (shell, settings) = apply_rc_inner(&rc_source(&[
            ("prompt", "`set { return 'custom> ' }"),
            ("startup", "`set { return () }"),
        ]));
        assert!(settings.startup);
        assert!(shell.has_hook(&HookName::session("prompt")));
        assert!(shell.has_hook(&HookName::session("startup")));
    }

    #[test]
    fn prompt_and_startup_refuse_malformed_tags_and_payloads() {
        for key in ["prompt", "startup"] {
            for value in [
                "{ return () }",
                "`other",
                "`inherit 1",
                "`set",
                "`set 1",
                "`set { |arg| return () }",
            ] {
                let (_, error) = apply_to_fresh_env_rejected(&[(key, value)]);
                assert!(
                    error.contains(key) && error.contains("not applied"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn limits_inherit_leaves_child_limits_unchanged() {
        let mut shell = prelude_shell();
        let pairs = rc_map(&mut shell, &rc_source(&[]));
        let plan = decode(&pairs, &shell).unwrap();
        assert_eq!(plan.child_limits, ral_core::process::ChildLimits::default());
    }

    #[cfg(unix)]
    #[test]
    fn umask_inherit_leaves_the_process_mask_and_set_decodes_it() {
        let mut shell = prelude_shell();
        let pairs = rc_map(&mut shell, &rc_source(&[]));
        assert!(decode(&pairs, &shell).unwrap().umask.is_none());
        let pairs = rc_map(&mut shell, &rc_source(&[("umask", "`set '022'")]));
        let plan = decode(&pairs, &shell).unwrap();
        assert_eq!(plan.umask.unwrap().as_raw_mode(), 0o22);
        let (_, error) = apply_to_fresh_env_rejected(&[("umask", "`set '888'")]);
        assert!(error.contains("octal digits"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_only_explicit_umask_and_limits() {
        for (key, value) in [
            ("umask", "`set '022'"),
            (
                "limits",
                "[open-files: `set 64, core-bytes: `inherit, stack-bytes: `inherit]",
            ),
        ] {
            let (_, error) = apply_to_fresh_env_rejected(&[(key, value)]);
            assert!(
                error.contains(key) && error.contains("not supported on Windows"),
                "{error}"
            );
        }
    }

    /// rc `recursion-limit:` overrides the default on the shell.
    #[test]
    fn rc_recursion_limit_applied() {
        let shell = apply_rc(&rc_source(&[("recursion-limit", "`set 256")]));
        assert_eq!(shell.stack_limit(), 256);
    }

    /// Zero refuses the whole rc rather than disabling the cap.
    #[test]
    fn rc_recursion_limit_zero_rejected() {
        let (shell, err) = apply_to_fresh_env_rejected(&[("recursion-limit", "`set 0")]);
        assert_eq!(
            shell.stack_limit(),
            ral_core::shell::runtime::DEFAULT_STACK_LIMIT
        );
        assert!(err.contains("recursion-limit") && err.contains("not applied"));
    }

    // ── RcPlan: values and handlers ───────────────────────────────

    /// A checked rc can still fail value validation without changing the shell.
    fn apply_to_fresh_env_rejected(overrides: &[(&str, &str)]) -> (Shell, String) {
        let mut shell = prelude_shell();
        let pairs = rc_map(&mut shell, &rc_source(overrides));
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
            apply_rc_surface(&rc_source(&[("surface", "'minimal'")])),
            Surface::Minimal
        );
        assert_eq!(
            apply_rc_surface(&rc_source(&[("surface", "'Readline'")])),
            Surface::Readline
        );
    }

    #[cfg(unix)]
    #[test]
    fn umask_accepts_only_permission_bits_as_octal_digits() {
        for (text, mask) in [("0", 0), ("077", 0o77), ("0022", 0o22), ("777", 0o777)] {
            assert_eq!(decode_umask(text).unwrap().as_raw_mode(), mask);
        }
        for text in [
            "", "+077", "-1", "8", " 077", "077 ", "00000", "1000", "7777",
        ] {
            let error = decode_umask(text).unwrap_err();
            assert!(error.contains("octal digits"), "{text}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn limits_refuse_negative_and_above_hard_values() {
        let (_, error) = apply_to_fresh_env_rejected(&[(
            "limits",
            "[open-files: `set -1, core-bytes: `inherit, stack-bytes: `inherit]",
        )]);
        assert!(error.contains("non-negative"), "{error}");
        if let Some(hard) = rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum
            && let Ok(above) = i64::try_from(hard).and_then(|n| i64::try_from(i128::from(n) + 1))
        {
            let (_, error) = apply_to_fresh_env_rejected(&[(
                "limits",
                &format!("[open-files: `set {above}, core-bytes: `inherit, stack-bytes: `inherit]"),
            )]);
            assert!(error.contains(&format!("hard limit of {hard}")), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn limits_set_each_child_ceiling() {
        let mut shell = prelude_shell();
        let pairs = rc_map(
            &mut shell,
            &rc_source(&[(
                "limits",
                "[open-files: `set 0, core-bytes: `set 0, stack-bytes: `set 0]",
            )]),
        );
        let plan = decode(&pairs, &shell).unwrap();
        assert_eq!(
            plan.child_limits,
            ral_core::process::ChildLimits {
                open_files: Some(0),
                core_bytes: Some(0),
                stack_bytes: Some(0),
            }
        );
    }

    #[test]
    fn declared_limit_keys_reject_wrong_runtime_shapes() {
        for key in ral_core::typecheck::table::LIMITS.keys {
            for value in [
                ValClos::UNIT,
                ValClos::variant("set", Some(ValClos::string("64"))),
            ] {
                let map = ValClos::map(vec![(key.label.to_owned(), value)]);
                let error = decode_limits(map.as_map("limits").unwrap()).unwrap_err();
                assert!(error.contains(key.label), "{error}");
            }
        }
    }

    #[test]
    fn rc_env_sets_only_the_environment() {
        let mut shell = apply_rc(&rc_source(&[("env", "[GREETING: 'hello']")]));
        assert_eq!(
            crate::repl::eval(&mut shell, "!{env}[GREETING]"),
            ValClos::string("hello")
        );
        assert!(shell.scope_lookup("GREETING").is_none());
        assert!(!typecheck_against_session(&shell, "$GREETING").is_empty());
    }

    /// A refused rc leaves no prompt hook behind: valid prompt, bad theme colour.
    #[test]
    fn refused_rc_registers_no_prompt() {
        let mut shell = prelude_shell();
        let pairs = rc_map(
            &mut shell,
            &rc_source(&[
                ("prompt", "`set { return 'x' }"),
                (
                    "theme",
                    "[value-prefix: `inherit, value-color: `set 'purple']",
                ),
            ]),
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

    #[test]
    fn a_record_of_handlers_refuses_the_rc_statically() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-run");
        let source = format!(
            "echo ran > {}\n{}",
            ral_core::syntax::quote_word(&ral_core::path::shown(&marker)),
            rc_source(&[("handlers", "[ll: { |args| echo hi }]")])
        );
        let mut shell = prelude_shell();
        let Err(Break::Error(error)) = ral_core::shell::load::extend_session(
            &Mooring::adrift(),
            &mut shell,
            &source,
            &ral_core::source::SourceOrigin::named("rc.ral"),
            Some(ral_core::typecheck::table::declared(Form::Rc)),
        ) else {
            panic!("a record under handlers must refuse the rc");
        };
        assert!(matches!(&error.reason, Reason::Compile(_)), "{error:?}");
        let rendered = error.rejection().expect("static refusal").render();
        assert!(
            rendered.contains("write `[:, ll: { |args| … }]`"),
            "{rendered}"
        );
        assert!(!marker.exists(), "no rc phrase may run before refusal");
        assert!(!shell.has_handler("ll"));
    }

    /// A non-block written arm refuses the rc before installation.
    #[test]
    fn refused_rc_installs_no_handlers() {
        let mut shell = prelude_shell();
        let Err(Break::Error(error)) = ral_core::shell::load::extend_session(
            &Mooring::adrift(),
            &mut shell,
            &rc_source(&[("handlers", "[:, ll: { |args| ls ...$args }, bad: 3]")]),
            &ral_core::source::SourceOrigin::named("rc.ral"),
            Some(ral_core::typecheck::table::declared(Form::Rc)),
        ) else {
            panic!("a non-block member must refuse the whole rc");
        };
        assert!(matches!(&error.reason, Reason::Compile(_)), "{error:?}");
        let rendered = error.rejection().expect("static refusal").render();
        assert!(
            rendered.contains("a handler installed under a name stands in for that command"),
            "{rendered}"
        );
        assert!(!shell.has_handler("ll") && !shell.has_handler("bad"));
    }

    /// Typecheck `src` against `shell`'s live session schemes — the same
    /// seed a real prompt run uses — and return the errors.
    fn typecheck_against_session(shell: &Shell, src: &str) -> Vec<ral_core::TypeError> {
        use ral_core::compile::{CompileError, UseRefusal, compile_and_typecheck};

        match compile_and_typecheck(
            src,
            shell.session_schemes(),
            ral_core::source::FileId::DUMMY,
            None,
            ral_core::typecheck::Unit::Script,
            &mut |_, _, _| {
                Err(UseRefusal::Unavailable {
                    reason: "this typechecking helper has no module loader".into(),
                })
            },
        ) {
            Ok(_) => Vec::new(),
            Err(CompileError::Types(errors)) => errors,
            Err(error) => panic!("expected valid test syntax: {error}"),
        }
    }

    /// An rc `let` carries its checked scheme into the session: a function
    /// takes independently typed arguments, unlike an external handler.
    #[test]
    fn rc_let_function_typechecks_heterogeneous_call() {
        let src = format!(
            "let ws = {{ |name body| echo $name; !$body }}\n{}",
            rc_source(&[])
        );
        let (shell, _) = apply_rc_inner(&src);
        // Lexical binding, not a persistent handler frame.
        assert!(shell.scope_lookup("ws").is_some());
        assert!(!shell.has_handler("ws"));
        let errs = typecheck_against_session(&shell, "ws 'x' { echo lol }\n");
        assert!(
            errs.is_empty(),
            "heterogeneous call to an rc `let` function should typecheck: {errs:?}"
        );
    }

    /// A `handlers:` handler for an external name keeps the external text gate:
    /// a nullary call succeeds, but a block argument is refused at check time.
    #[test]
    fn rc_handler_keeps_the_external_argument_gate() {
        let src = rc_source(&[("handlers", "[:, ws: { |args| echo $args }]")]);
        let (shell, _) = apply_rc_inner(&src);
        // Persistent handler frame, not a scope binding.
        assert!(shell.has_handler("ws"));
        assert!(shell.scope_lookup("ws").is_none());
        let errs = typecheck_against_session(&shell, "ws\n");
        assert!(
            errs.is_empty(),
            "a nullary call to an external handler should typecheck: {errs:?}"
        );

        let errs = typecheck_against_session(&shell, "ws 'x' { echo lol }\n");
        assert!(
            !errs.is_empty(),
            "an external handler must not admit a block argument"
        );
        assert!(
            errs.iter()
                .filter_map(ral_core::TypeError::hint)
                .any(|hint| {
                    hint.contains("`ws` has a handler here")
                        && hint.contains("takes only text")
                        && hint.contains("...$xs")
                        && hint.contains("bind `ws` as a function")
                }),
            "the refusal should explain the handled head's text gate and alternatives: {errs:?}"
        );
    }

    #[test]
    fn rc_contract_admits_decoded_fields_before_later_effects() {
        use ral_core::source::SourceOrigin;

        for (key, value, use_text) in [
            ("handlers", "[:, foo: { |args| return $d }]", "return $d"),
            ("umask", "$d", "$d"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("after-decode");
            let source = format!(
                "let d = from-json << '5'\necho ran > {}\n{}",
                ral_core::syntax::quote_word(&ral_core::path::shown(&marker)),
                rc_source(&[(key, value)])
            );
            let mut shell = prelude_shell();
            let Err(Break::Error(error)) = ral_core::shell::load::extend_session(
                &Mooring::adrift(),
                &mut shell,
                &source,
                &SourceOrigin::named("rc.ral"),
                Some(ral_core::typecheck::table::declared(Form::Rc)),
            ) else {
                panic!("the live rc contract must refuse decoded 5: {source}");
            };
            assert!(
                matches!(&error.reason, Reason::Invalid(failure)
                    if failure.operation == "from-json"
                        && matches!(failure.cause, InvalidCause::Admission(_))),
                "{error:?}"
            );
            let fact = error.fact(&shell);
            assert_eq!(fact.site().script.as_ref(), "rc.ral");
            assert_eq!(fact.site().line, 1);
            let witness = error.witness().expect("the contract must name the use");
            assert!(
                source[witness.start as usize..witness.end as usize].contains(use_text),
                "the witness must identify {key}: {error:?}"
            );
            assert!(!marker.exists(), "nothing after the decoder may run");
            assert!(!shell.has_handler("foo"));
        }
    }

    /// Malformed values on declared keys must name the offending key.
    #[test]
    fn every_declared_rc_key_is_handled_by_decode() {
        let table = ral_core::typecheck::table::declared(Form::Rc);
        let shell = ral_core::test_helper::core_shell();
        for key in table.keys {
            let value = match &key.holds {
                ral_core::typecheck::table::Holds::Table(sub) => {
                    ValClos::map(sub.keys.iter().enumerate().map(|(i, key)| {
                        let value = if i == 0 {
                            ValClos::variant("set", Some(ValClos::int(-1)))
                        } else {
                            ValClos::variant("inherit", None)
                        };
                        (key.label.to_owned(), value)
                    }))
                }
                _ => ValClos::UNIT,
            };
            let pairs = ValClos::map(vec![(key.label.to_string(), value)]);
            let err = decode(&pairs, &shell)
                .err()
                .expect("the malformed value satisfies no key");
            assert!(
                err.contains(key.label),
                "key '{}' fell through: {err}",
                key.label
            );
        }
    }
}
