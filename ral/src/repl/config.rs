//! RC file discovery, parsing, and application.
//!
//! An rc file is ral source whose return value is a map.  Recognised keys
//! map to REPL state: `env`, `prompt`, `bindings`, `aliases`, `edit_mode`,
//! `bell`, `recursion_limit`, `plugins`, `startup`, `theme`.  Unknown keys
//! are silently ignored so future versions can add knobs without breaking
//! older configs.

use ral_core::ansi::{OutputTheme, named_color};
use ral_core::io::InteractiveMode;
use ral_core::{AliasEntry, Shell, EvalSignal, Value};
use rustyline::config::{BellStyle, EditMode};

// ── Mutable REPL state threaded through rc/profile loading ───────────────

/// Bundles the REPL-level state that rc files and profiles are allowed to
/// mutate: environment bindings, line-editing mode, and bell style.  Passed
/// by `&mut` to the loaders so the signature stays stable as new knobs
/// (themes, keymaps, …) grow.
pub(crate) struct RcCtx<'a> {
    pub shell: &'a mut Shell,
    pub edit_mode: &'a mut EditMode,
    pub bell: &'a mut BellStyle,
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
    # recursion_limit:  1024,        # maximum function-call recursion depth

    # prompt: {
    #     return \"$CWD $ \"
    # },

    # shell: [
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
pub(super) fn create_default_rc() -> Option<String> {
    use std::path::PathBuf;
    let (dir, path) = if let Some(base) = crate::platform::config_base() {
        let dir = PathBuf::from(base).join("ral");
        let path = dir.join("rc");
        (dir, path)
    } else {
        let home = crate::platform::home_dir();
        if home == "." {
            return None;
        }
        let home = PathBuf::from(home);
        (home.clone(), home.join(".ralrc"))
    };
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

/// Build the `$TERMINAL` map exposed to RC and plugin code.
pub(super) fn terminal_capability_map(t: &ral_core::io::TerminalState) -> Value {
    let mode = match t.mode {
        InteractiveMode::Auto => "auto",
        InteractiveMode::Minimal => "minimal",
        InteractiveMode::Full => "full",
    };
    Value::Map(vec![
        ("stdin_tty".into(), Value::Bool(t.stdin_tty)),
        ("stdout_tty".into(), Value::Bool(t.stdout_tty)),
        ("stderr_tty".into(), Value::Bool(t.stderr_tty)),
        ("supports_ansi".into(), Value::Bool(t.supports_ansi)),
        ("no_color".into(), Value::Bool(t.no_color)),
        ("is_tmux".into(), Value::Bool(t.is_tmux)),
        ("is_asciinema".into(), Value::Bool(t.is_asciinema)),
        ("is_ci".into(), Value::Bool(t.is_ci)),
        ("ui_ansi_ok".into(), Value::Bool(t.ui_ansi_ok())),
        ("mode".into(), Value::String(mode.into())),
    ])
}

// ── RC config application ────────────────────────────────────────────────

/// Scan the lexed rc source for `aliases: [ name: { ... }, ... ]` entries
/// and build a map from alias name to its verbatim `{ ... }` source slice.
///
/// Bodies that are not `{ ... }` blocks (rare, but e.g. a shared thunk
/// identifier) are skipped — `which` falls back to the rendered value.
fn extract_alias_sources(rc_text: &str) -> std::collections::HashMap<String, String> {
    use ral_core::ast::Word;
    use ral_core::lexer::{self, Token};
    let mut out = std::collections::HashMap::new();
    let Ok(tokens) = lexer::lex(rc_text) else {
        return out;
    };
    // Locate `aliases : [` at any depth; once inside, collect top-level entries.
    let mut i = 0;
    while i + 2 < tokens.len() {
        let aliases_key = matches!(&tokens[i].0, Token::Word(Word::Plain(w)) if w == "aliases");
        if aliases_key
            && matches!(tokens[i + 1].0, Token::Colon)
            && matches!(tokens[i + 2].0, Token::LBracket)
        {
            collect_alias_entries(&tokens[i + 3..], rc_text, &mut out);
            return out;
        }
        i += 1;
    }
    out
}

/// Walk the token slice starting just inside the `aliases` map's `[`, and
/// capture the source of each `name: { ... }` entry at the top level.
fn collect_alias_entries(
    tokens: &[(ral_core::lexer::Token, ral_core::lexer::Span)],
    rc_text: &str,
    out: &mut std::collections::HashMap<String, String>,
) {
    use ral_core::ast::Word;
    use ral_core::lexer::Token;
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i].0 {
            Token::RBracket if depth == 0 => return,
            Token::LBracket | Token::LBrace => depth += 1,
            Token::RBracket | Token::RBrace => depth -= 1,
            Token::Word(Word::Plain(name)) if depth == 0 => {
                if !matches!(tokens.get(i + 1), Some((Token::Colon, _))) {
                    i += 1;
                    continue;
                }
                let Some((Token::LBrace, open)) = tokens.get(i + 2) else {
                    i += 1;
                    continue;
                };
                // Brace-match forward from the `{` after `name :`.
                let mut d: i32 = 1;
                let mut j = i + 3;
                while j < tokens.len() && d > 0 {
                    match &tokens[j].0 {
                        Token::LBrace => d += 1,
                        Token::RBrace => d -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                if d == 0 {
                    let start = open.byte.start as usize;
                    let end = tokens[j - 1].1.byte.end as usize;
                    if start <= end
                        && end <= rc_text.len()
                        && rc_text.is_char_boundary(start)
                        && rc_text.is_char_boundary(end)
                    {
                        out.insert(name.clone(), rc_text[start..end].to_string());
                    }
                    i = j;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Apply the RC config map to `ctx`. Returns the `startup` block, if any,
/// so the caller can execute it in the right context.  When `rc_text` is
/// supplied, alias definitions are stored verbatim for `which`.
pub(crate) fn apply_rc_config(
    config: Value,
    ctx: &mut RcCtx<'_>,
    rc_text: Option<&str>,
) -> Option<Value> {
    let Value::Map(pairs) = config else {
        return None;
    };
    let alias_sources = rc_text.map(extract_alias_sources).unwrap_or_default();
    let mut startup: Option<Value> = None;
    for (key, val) in pairs {
        match key.as_str() {
            "env" => for (k, v) in into_map(val) {
                ctx.shell.dynamic.env_vars.insert(k.clone(), v.to_string());
                ctx.shell.set(k, v);
            },
            "prompt" => ctx.shell.set("RAL_PROMPT".into(), val),
            "bindings" => for (name, value) in into_map(val) {
                ctx.shell.set(name, value);
            },
            "aliases" => for (name, func) in into_map(val) {
                let entry = match alias_sources.get(&name) {
                    Some(src) => AliasEntry::with_source(func, src.clone()),
                    None => AliasEntry::new(func),
                };
                ctx.shell.registry.aliases.insert(name, entry);
            },
            "edit_mode" => if let Value::String(s) = val {
                match s.to_ascii_lowercase().as_str() {
                    "vi" => *ctx.edit_mode = EditMode::Vi,
                    "emacs" => *ctx.edit_mode = EditMode::Emacs,
                    _ => {}
                }
            },
            "bell" => if let Value::Bool(b) = val {
                *ctx.bell = if b { BellStyle::Audible } else { BellStyle::None };
            },
            "recursion_limit" => if let Some(n) = val.as_int().filter(|n| *n > 0) {
                ctx.shell.control.recursion_limit = n as usize;
            },
            "plugins" => for entry in into_list(val) {
                load_rc_plugin(entry, ctx.shell);
            },
            "startup" => startup = Some(val),
            "theme" => if let Value::Map(pairs) = val {
                let mut theme = OutputTheme::default();
                for (k, v) in pairs {
                    match k.as_str() {
                        "value_prefix" => theme.value_prefix = v.to_string(),
                        "value_color" => theme.value_color = named_color(&v.to_string()),
                        _ => {}
                    }
                }
                ral_core::ansi::set_output_theme(theme);
            },
            _ => {}
        }
    }
    startup
}

fn into_map(v: Value) -> Vec<(String, Value)> {
    match v { Value::Map(p) => p, _ => Vec::new() }
}

fn into_list(v: Value) -> Vec<Value> {
    match v { Value::List(l) => l, _ => Vec::new() }
}

/// Load a single plugin entry from the RC `plugins` list.
///
/// Each entry is a map `[plugin: Str, options?: Map]`.  Unknown top-level
/// keys are warned and ignored so future extensions (enabled, when, …) can
/// slot in without breaking parsers.
fn load_rc_plugin(entry: Value, shell: &mut Shell) {
    if let Err(msg) = parse_and_load_rc_plugin(entry, shell) {
        ral_core::diagnostic::cmd_error("ral", &msg);
    }
}

/// Shape-check an rc plugin entry, dispatch to `_plugin 'load'`, and return
/// the load error (if any) as a formatted string — the Result lets each
/// field check short-circuit with `?` instead of repeating the cmd_error
/// boilerplate per key.
fn parse_and_load_rc_plugin(entry: Value, shell: &mut Shell) -> Result<(), String> {
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
    let mut load_args = vec![Value::String("load".into()), Value::String(name.clone())];
    load_args.extend(options);
    match ral_core::builtins::plugin::builtin_plugin(&load_args, shell) {
        Err(EvalSignal::Error(e)) => Err(format!("plugin '{name}': {}", e.message)),
        _ => Ok(()),
    }
}

// ── Config file locations ────────────────────────────────────────────────

/// Search for an existing RC file in the standard locations.
pub(super) fn find_ralrc() -> Option<String> {
    if let Some(dir) = crate::platform::config_base() {
        let p = format!("{dir}/ral/rc");
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    let home = crate::platform::home_dir();
    if home != "." {
        let p = format!("{home}/.ralrc");
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    None
}

/// Resolve the history file path, creating the config directory if needed.
pub(super) fn dirs_history() -> Option<String> {
    if let Some(dir) = crate::platform::config_base() {
        let d = format!("{dir}/ral");
        let _ = std::fs::create_dir_all(&d);
        return Some(format!("{d}/history"));
    }
    let home = crate::platform::home_dir();
    if home == "." {
        None
    } else {
        Some(format!("{home}/.ral_history"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluate `rc_src`, apply it through `apply_rc_config`, and return
    /// the resulting environment.  Registers the baked prelude so plugin
    /// files can use `get`, `has`, etc. — the same environment they see at
    /// real startup.  When `pass_source` is set, the rc text is forwarded
    /// to `apply_rc_config` so alias source extraction is exercised.
    fn apply_rc_inner(rc_src: &str, pass_source: bool) -> Shell {
        let mut shell = Shell::new(Default::default());
        ral_core::builtins::register(&mut shell, super::super::super::baked_prelude_comp());
        let ast = ral_core::parse(rc_src).unwrap();
        let comp = ral_core::elaborator::elaborate(&ast, Default::default());
        let config = ral_core::evaluate(&comp, &mut shell).unwrap();
        let mut mode = EditMode::Emacs;
        let mut bell = BellStyle::None;
        apply_rc_config(
            config,
            &mut RcCtx { shell: &mut shell, edit_mode: &mut mode, bell: &mut bell },
            if pass_source { Some(rc_src) } else { None },
        );
        shell
    }

    fn apply_rc(rc_src: &str) -> Shell { apply_rc_inner(rc_src, false) }

    /// Typecheck `src` against the baked prelude; return the errors.
    fn typecheck_src(src: &str) -> Vec<ral_core::TypeError> {
        let ast = ral_core::parse(src).unwrap();
        let comp = ral_core::elaborator::elaborate(&ast, Default::default());
        ral_core::typecheck(&comp, super::super::super::baked_prelude_schemes())
    }

    /// `_plugin 'load' <name> <opts>` expects `opts` to be a `Map`;
    /// passing a `String` through `load-plugin` triggers a type error.
    #[test]
    fn plugin_load_string_options_fails_typecheck() {
        let errs = typecheck_src("load-plugin 'fzf-files' 'ctrl-t'\n");
        assert!(
            !errs.is_empty(),
            "expected a type error for string options, got none"
        );
    }

    /// A well-typed rc map — string name, map options — typechecks cleanly.
    #[test]
    fn plugin_load_map_options_typechecks() {
        let errs = typecheck_src("load-plugin 'fzf-files' [key: 'ctrl-t']\n");
        assert!(errs.is_empty(), "unexpected type errors: {errs:?}");
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
        capabilities: [editor: [read: true]],
    ]
}
"#,
        )
        .unwrap();

        let rc_src = format!(
            "return [plugins: [[plugin: '{}', options: [key: 'from-rc']]]]\n",
            path.to_string_lossy()
        );
        let shell = apply_rc(&rc_src);
        assert_eq!(shell.registry.plugins.len(), 1);
        assert_eq!(shell.registry.plugins[0].name, "from-rc");
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
    return [name: $k, capabilities: [editor: [read: true]]]
}
"#,
        )
        .unwrap();

        let rc_src = format!(
            "return [plugins: [[plugin: '{}']]]\n",
            path.to_string_lossy()
        );
        let shell = apply_rc(&rc_src);
        assert_eq!(shell.registry.plugins.len(), 1);
        assert_eq!(shell.registry.plugins[0].name, "fallback");
    }

    /// Malformed rc plugin entries (non-map, missing `plugin:`) emit a
    /// diagnostic but do not panic and leave `shell.registry.plugins` empty.
    #[test]
    fn rc_plugin_entry_malformed_is_rejected() {
        // Old list form — rejected wholesale.
        let shell = apply_rc("return [plugins: [['foo', 'bar']]]\n");
        assert!(shell.registry.plugins.is_empty());

        // Map missing 'plugin:' — rejected.
        let shell = apply_rc("return [plugins: [[options: [key: 'x']]]]\n");
        assert!(shell.registry.plugins.is_empty());
    }

    /// Parse an rc-shaped source and check the alias was registered with
    /// its original source text captured verbatim for `which`.
    #[test]
    fn alias_captures_source_text() {
        let src = "return [\n    aliases: [\n        greet: { |args| echo hello ...$args },\n        ll: { |args| ls -lh ...$args },\n    ],\n]\n";
        let shell = apply_rc_inner(src, true);
        let greet = shell.registry.aliases.get("greet").expect("greet registered");
        assert_eq!(greet.source.as_deref(), Some("{ |args| echo hello ...$args }"));
        let ll = shell.registry.aliases.get("ll").expect("ll registered");
        assert_eq!(ll.source.as_deref(), Some("{ |args| ls -lh ...$args }"));
    }

    /// rc `recursion_limit:` overrides the default on the shell.
    #[test]
    fn rc_recursion_limit_applied() {
        let shell = apply_rc("return [recursion_limit: 256]\n");
        assert_eq!(shell.control.recursion_limit, 256);
    }

    /// A non-positive `recursion_limit` is silently ignored (the default
    /// stays in place rather than letting `0` through to disable the cap).
    #[test]
    fn rc_recursion_limit_zero_ignored() {
        let shell = apply_rc("return [recursion_limit: 0]\n");
        assert_eq!(shell.control.recursion_limit, ral_core::types::DEFAULT_RECURSION_LIMIT);
    }
}
