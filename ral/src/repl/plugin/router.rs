//! Frontend-neutral keybinding dispatch: one ordered table, one resolution
//! rule, consumed by every editor backend.
//!
//! The table holds plugin keybindings in plugin load order (manifest order
//! within a plugin).  Dispatch is the first entry whose chord matches the
//! key and whose guard allows; when no entry claims it the resolution is
//! [`Resolution::Default`] — the editor's built-in action.  Several guarded
//! bindings on one chord therefore compose as an ordered pattern match with
//! the built-in as the final arm.
//!
//! Each backend realizes `Default` natively: rustyline by returning `None`
//! from its conditional handler (its run-the-default protocol), the
//! structural frontend by falling into its own built-in key arms.
//! Precedence is decided here, once, so it cannot vary by frontend.
//! Resolution is pure host-side work (chord equality, one regex match) —
//! safe inside editor callbacks, where the evaluator must never run.

use super::manifest::LoadedPlugin;

/// A frontend-neutral key name: the subset of keys a plugin keybinding may
/// bind, exactly the notations [`parse_key_notation`] accepts.  Each editor
/// backend adapts these to its own event type at its boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::repl) enum KeyName {
    Char(char),
    Tab,
    Enter,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    Backspace,
    F(u8),
}

/// A frontend-neutral key chord: a [`KeyName`] plus the ctrl/alt modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::repl) struct KeyChord {
    pub(in crate::repl) name: KeyName,
    pub(in crate::repl) ctrl: bool,
    pub(in crate::repl) alt: bool,
}

/// Parse a key notation string ("ctrl-r", "alt-x", "f5", "tab", …) into a
/// frontend-neutral [`KeyChord`].  Returns `None` for unrecognised notations.
pub(in crate::repl) fn parse_key_notation(key: &str) -> Option<KeyChord> {
    const NAMED: &[(&str, KeyName)] = &[
        ("tab", KeyName::Tab),
        ("enter", KeyName::Enter),
        ("escape", KeyName::Escape),
        ("up", KeyName::Up),
        ("down", KeyName::Down),
        ("left", KeyName::Left),
        ("right", KeyName::Right),
        ("home", KeyName::Home),
        ("end", KeyName::End),
        ("delete", KeyName::Delete),
        ("backspace", KeyName::Backspace),
    ];
    let plain = |name| KeyChord {
        name,
        ctrl: false,
        alt: false,
    };
    let key = key.trim();
    if key.len() == 1 {
        return Some(plain(KeyName::Char(key.chars().next()?)));
    }
    if let Some(&(_, name)) = NAMED.iter().find(|(n, _)| *n == key) {
        return Some(plain(name));
    }
    for (prefix, ctrl, alt) in [("ctrl-", true, false), ("alt-", false, true)] {
        if let Some(rest) = key.strip_prefix(prefix) {
            return Some(KeyChord {
                name: KeyName::Char(rest.chars().next()?),
                ctrl,
                alt,
            });
        }
    }
    let num = key.strip_prefix('f').and_then(|s| s.parse::<u8>().ok())?;
    (1..=12).contains(&num).then_some(plain(KeyName::F(num)))
}

/// Why a chord is off-limits to plugins entirely: interrupt and EOF are the
/// session's escape hatches, and a wedged handler on either would leave no
/// way to stop it or leave the shell.  The manifest rejects these at load,
/// guard or not.
pub(in crate::repl) fn reserved_action(chord: KeyChord) -> Option<&'static str> {
    match (chord.name, chord.ctrl, chord.alt) {
        (KeyName::Char('c'), true, false) => Some("interrupt"),
        (KeyName::Char('d'), true, false) => Some("end-of-file"),
        _ => None,
    }
}

/// The ral-owned built-in action at the dispatch table's tail for `chord`,
/// if any.  Every unmodified key except F1–F12 carries one — typing,
/// movement, deletion, completion, history, accept-line — so an unguarded
/// binding there would shadow it on every press and the manifest requires a
/// guard.  Modified chords and function keys carry none: they hold at most
/// an underlying editor default, which a plugin may replace outright.
pub(in crate::repl) fn builtin_action(chord: KeyChord) -> Option<&'static str> {
    if chord.ctrl || chord.alt {
        return None;
    }
    match chord.name {
        KeyName::Char(_) => Some("text insertion"),
        KeyName::Tab => Some("completion"),
        KeyName::Enter => Some("accept-line"),
        KeyName::Escape => Some("keymap escape"),
        KeyName::Up | KeyName::Down => Some("history navigation"),
        KeyName::Left | KeyName::Right | KeyName::Home | KeyName::End => Some("cursor movement"),
        KeyName::Backspace | KeyName::Delete => Some("deletion"),
        KeyName::F(_) => None,
    }
}

/// True when `guard` permits the binding to claim the key: no guard, or
/// the regex matches the text left of the cursor.  `pos` is a byte offset
/// on a char boundary; a malformed offset declines defensively.
fn guard_allows(guard: Option<&regex::Regex>, line: &str, pos: usize) -> bool {
    match guard {
        None => true,
        Some(g) => line.get(..pos).is_some_and(|l| g.is_match(l)),
    }
}

/// One dispatch-table entry.  `key` retains the manifest notation for lint
/// messages; `(plugin, binding_idx)` is the same identity
/// [`PluginRuntime::resolve_keybinding`](super::PluginRuntime::resolve_keybinding)
/// re-resolves by name at dispatch time, stale-safe across unloads.
#[derive(Clone)]
pub(in crate::repl) struct RouterEntry {
    pub(in crate::repl) plugin: String,
    pub(in crate::repl) binding_idx: usize,
    pub(in crate::repl) key: String,
    pub(in crate::repl) chord: KeyChord,
    pub(in crate::repl) guard: Option<regex::Regex>,
}

/// What a key press resolves to.
#[derive(Debug, PartialEq)]
pub(in crate::repl) enum Resolution {
    /// A plugin binding claimed the chord; dispatch its handler after the
    /// editor returns.
    Claimed { plugin: String, binding_idx: usize },
    /// No entry claimed it: run the editor's built-in action.
    Default,
}

/// The ordered dispatch table, rebuilt whenever the plugin list changes
/// ([`PluginRuntime::keybindings_changed`](super::PluginRuntime::keybindings_changed))
/// and snapshot (`Clone`) by a frontend that matches keys itself.
#[derive(Clone, Default)]
pub(in crate::repl) struct KeyRouter {
    entries: Vec<RouterEntry>,
}

impl KeyRouter {
    /// The table for the current plugin list: every binding, load order
    /// across plugins, manifest order within one.
    pub(in crate::repl) fn build(plugins: &[LoadedPlugin]) -> Self {
        let entries = plugins
            .iter()
            .flat_map(|p| {
                p.keybindings
                    .iter()
                    .enumerate()
                    .map(|(bi, kb)| RouterEntry {
                        plugin: p.name.clone(),
                        binding_idx: bi,
                        key: kb.key.clone(),
                        chord: kb.chord,
                        guard: kb.guard.clone(),
                    })
            })
            .collect();
        Self { entries }
    }

    /// First entry whose chord matches and whose guard allows; `Default`
    /// otherwise.  `pos` is a byte offset into `line`.
    pub(in crate::repl) fn resolve(&self, chord: KeyChord, line: &str, pos: usize) -> Resolution {
        self.entries
            .iter()
            .find(|e| e.chord == chord && guard_allows(e.guard.as_ref(), line, pos))
            .map_or(Resolution::Default, |e| Resolution::Claimed {
                plugin: e.plugin.clone(),
                binding_idx: e.binding_idx,
            })
    }

    /// The distinct bound chords in table order — rustyline registers one
    /// conditional handler per chord, so same-chord entries never fight
    /// over a binding.
    pub(in crate::repl) fn bound_chords(&self) -> Vec<KeyChord> {
        let mut out: Vec<KeyChord> = Vec::new();
        for e in &self.entries {
            if !out.contains(&e.chord) {
                out.push(e.chord);
            }
        }
        out
    }

    /// Entries that can never fire — an earlier unguarded entry on the same
    /// chord claims every press first — paired with their blocker, for the
    /// load-time shadow lint.
    pub(in crate::repl) fn dead_entries(&self) -> Vec<(&RouterEntry, &RouterEntry)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                self.entries[..i]
                    .iter()
                    .find(|b| b.chord == e.chord && b.guard.is_none())
                    .map(|b| (e, b))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::HookHealth;
    use super::super::manifest::KeyBinding;
    use super::*;
    use ral_core::Value;

    fn entry(plugin: &str, bi: usize, key: &str, guard: Option<&str>) -> RouterEntry {
        RouterEntry {
            plugin: plugin.into(),
            binding_idx: bi,
            key: key.into(),
            chord: parse_key_notation(key).expect("test key parses"),
            guard: guard.map(|g| regex::Regex::new(g).expect("test guard compiles")),
        }
    }

    fn claimed(plugin: &str, bi: usize) -> Resolution {
        Resolution::Claimed {
            plugin: plugin.into(),
            binding_idx: bi,
        }
    }

    /// `parse_key_notation` reduces every supported notation to a
    /// frontend-neutral [`KeyChord`] — the modifiers and the named keys the
    /// fzf/zoxide plugins bind, plus the bare-char and function-key forms.
    #[test]
    fn parse_key_notation_yields_neutral_chords() {
        let chord = |name, ctrl, alt| Some(KeyChord { name, ctrl, alt });
        assert_eq!(
            parse_key_notation("ctrl-r"),
            chord(KeyName::Char('r'), true, false)
        );
        assert_eq!(
            parse_key_notation("alt-c"),
            chord(KeyName::Char('c'), false, true)
        );
        assert_eq!(parse_key_notation("tab"), chord(KeyName::Tab, false, false));
        assert_eq!(
            parse_key_notation("t"),
            chord(KeyName::Char('t'), false, false)
        );
        assert_eq!(parse_key_notation("f5"), chord(KeyName::F(5), false, false));
        // Unrecognised notations and out-of-range function keys are rejected.
        assert_eq!(parse_key_notation("hyper-x"), None);
        assert_eq!(parse_key_notation("f13"), None);
    }

    /// `guard_allows`: no guard always allows; a guard matches against the
    /// text left of the cursor only, and an out-of-range offset declines
    /// defensively rather than panicking.
    #[test]
    fn guard_allows_matches_left_of_cursor() {
        assert!(guard_allows(None, "anything", 0));

        let re = regex::Regex::new(r"\S\s+\S*\*\*$").unwrap();
        assert!(guard_allows(Some(&re), "cd **", "cd **".len()));
        assert!(!guard_allows(Some(&re), "**", "**".len()));
        assert!(!guard_allows(Some(&re), "foo**", "foo**".len()));
        assert!(!guard_allows(Some(&re), "cd ", "cd ".len()));

        assert!(!guard_allows(Some(&re), "cd **", 100));
    }

    /// Dispatch is the first entry whose chord matches and whose guard
    /// allows: guarded same-chord entries compose as an ordered pattern
    /// match, and no match resolves to the built-in tail.
    #[test]
    fn resolve_walks_entries_in_order() {
        let router = KeyRouter {
            entries: vec![
                entry("a", 0, "tab", Some(r"^git ")),
                entry("b", 0, "tab", Some(r"\*\*$")),
            ],
        };
        let tab = parse_key_notation("tab").unwrap();
        assert_eq!(router.resolve(tab, "git st", 6), claimed("a", 0));
        assert_eq!(router.resolve(tab, "cd **", 5), claimed("b", 0));
        assert_eq!(router.resolve(tab, "ls ", 3), Resolution::Default);
    }

    /// An unguarded entry claims every press of its chord; other chords
    /// still reach their own entries or the tail.
    #[test]
    fn unguarded_entry_claims_unconditionally() {
        let router = KeyRouter {
            entries: vec![entry("files", 0, "ctrl-t", None)],
        };
        let ctrl_t = parse_key_notation("ctrl-t").unwrap();
        let ctrl_r = parse_key_notation("ctrl-r").unwrap();
        assert_eq!(router.resolve(ctrl_t, "", 0), claimed("files", 0));
        assert_eq!(router.resolve(ctrl_r, "", 0), Resolution::Default);
    }

    /// `bound_chords` yields each chord once, in first-appearance order —
    /// rustyline registers exactly one handler per chord.
    #[test]
    fn bound_chords_dedupes_in_order() {
        let router = KeyRouter {
            entries: vec![
                entry("a", 0, "tab", Some("x")),
                entry("b", 0, "ctrl-t", None),
                entry("c", 0, "tab", Some("y")),
            ],
        };
        assert_eq!(
            router.bound_chords(),
            vec![
                parse_key_notation("tab").unwrap(),
                parse_key_notation("ctrl-t").unwrap()
            ]
        );
    }

    /// A binding behind an earlier unguarded entry on the same chord is
    /// dead; guarded predecessors kill nothing.
    #[test]
    fn dead_entries_flags_bindings_behind_unguarded() {
        let router = KeyRouter {
            entries: vec![
                entry("a", 0, "ctrl-t", None),
                entry("b", 0, "ctrl-t", Some("x")),
                entry("c", 0, "tab", Some("y")),
                entry("d", 0, "tab", Some("z")),
            ],
        };
        let dead: Vec<(&str, &str)> = router
            .dead_entries()
            .into_iter()
            .map(|(dead, blocker)| (dead.plugin.as_str(), blocker.plugin.as_str()))
            .collect();
        assert_eq!(dead, vec![("b", "a")]);
    }

    /// The reserved set is exactly the session's escape hatches; the
    /// built-in-action table covers every unmodified key except F1–F12 and
    /// no modified chord.
    #[test]
    fn reserved_and_builtin_tables() {
        let chord = |s| parse_key_notation(s).unwrap();
        assert_eq!(reserved_action(chord("ctrl-c")), Some("interrupt"));
        assert_eq!(reserved_action(chord("ctrl-d")), Some("end-of-file"));
        assert_eq!(reserved_action(chord("ctrl-t")), None);
        assert_eq!(reserved_action(chord("tab")), None);

        assert_eq!(builtin_action(chord("tab")), Some("completion"));
        assert_eq!(builtin_action(chord("enter")), Some("accept-line"));
        assert_eq!(builtin_action(chord("up")), Some("history navigation"));
        assert_eq!(builtin_action(chord("t")), Some("text insertion"));
        assert_eq!(builtin_action(chord("backspace")), Some("deletion"));
        assert_eq!(builtin_action(chord("f5")), None);
        assert_eq!(builtin_action(chord("ctrl-t")), None);
        assert_eq!(builtin_action(chord("alt-c")), None);
    }

    /// `build` flattens the plugin list in load order, manifest order
    /// within a plugin.
    #[test]
    fn build_orders_entries_by_load_then_manifest() {
        let plugin = |name: &str, keys: &[&str]| LoadedPlugin {
            name: name.into(),
            hooks: std::collections::HashMap::new(),
            keybindings: keys
                .iter()
                .map(|k| KeyBinding {
                    key: (*k).into(),
                    chord: parse_key_notation(k).unwrap(),
                    handler: Value::Int(0),
                    guard: None,
                })
                .collect(),
            bindings: Vec::new(),
            state_cell: None,
            source: std::sync::Arc::from(""),
            buffer_change_health: HookHealth::default(),
        };
        let router =
            KeyRouter::build(&[plugin("p", &["ctrl-t", "ctrl-r"]), plugin("q", &["alt-c"])]);
        let order: Vec<(&str, usize, &str)> = router
            .entries
            .iter()
            .map(|e| (e.plugin.as_str(), e.binding_idx, e.key.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![("p", 0, "ctrl-t"), ("p", 1, "ctrl-r"), ("q", 0, "alt-c")]
        );
    }
}
