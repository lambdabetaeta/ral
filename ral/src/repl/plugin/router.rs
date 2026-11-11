//! Frontend-neutral keybinding dispatch: one ordered table, one resolution
//! rule, consumed by every editor backend.
//!
//! The table holds plugin keybindings in plugin load order (manifest order
//! within a plugin).  Dispatch is the first entry whose chord sequence matches
//! the keys pressed and whose guard allows; when no entry claims it the
//! resolution is [`Resolution::Default`] — the editor's built-in action.
//! Several guarded bindings on one sequence therefore compose as an ordered
//! pattern match with the built-in as the final arm.
//!
//! Each backend realizes `Default` natively: rustyline by returning `None`
//! from its conditional handler (its run-the-default protocol).
//! Precedence is decided here, once, so it cannot vary by frontend.
//! Resolution is pure host-side work (chord equality, one regex match) —
//! safe inside editor callbacks, where the evaluator must never run.

use super::manifest::LoadedPlugin;
use ral_core::fact::{
    BuiltinKeyAction, KeyModifier, KeyNotationFailure, KeyNotationFailureControlCharacter,
    KeyNotationFailureDuplicateModifier, KeyNotationFailureShiftedCharacter,
    KeyNotationFailureTerminalAlias, KeyNotationFailureUnknownKey,
    KeyNotationFailureUppercaseControl, ReservedKeyAction, TerminalKeyAlias,
};

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

/// A frontend-neutral key chord: a [`KeyName`] plus the ctrl/alt/shift
/// modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::repl) struct KeyChord {
    pub(in crate::repl) name: KeyName,
    pub(in crate::repl) ctrl: bool,
    pub(in crate::repl) alt: bool,
    pub(in crate::repl) shift: bool,
}

/// Plain ctrl-x, the prefix rustyline leaves free for key sequences.
pub(in crate::repl) const CTRL_X: KeyChord = KeyChord {
    name: KeyName::Char('x'),
    ctrl: true,
    alt: false,
    shift: false,
};

/// Parse a key notation into frontend-neutral [`KeyChord`]s, one per
/// space-separated chord:
///
/// ```text
/// notation := chord (' '+ chord)*
/// chord    := (modifier '-')* key        each modifier at most once, any order
/// modifier := ctrl | alt | shift
/// key      := one character | f1…f12 | tab | enter | escape | up | down
///           | left | right | home | end | delete | backspace
/// ```
///
/// A modifier is stripped only while something follows its dash, so `ctrl--`
/// is ctrl with `-`.  Spellings a terminal cannot tell from another key are
/// refused, with the one to use: `shift` with a character, `ctrl` with an
/// uppercase letter, `ctrl-h`, `ctrl-i`, `ctrl-m` and `ctrl-[` (backspace,
/// tab, enter, escape), and a control character as the key.  The error names
/// the offending text, not the notation it came from.
pub(in crate::repl) fn parse_key_notation(key: &str) -> Result<Vec<KeyChord>, KeyNotationFailure> {
    const MODIFIERS: [&str; 3] = ["ctrl", "alt", "shift"];
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
    // A terminal sends the same byte for these `ctrl-` letters as for the named key.
    const ALIASED: [(char, TerminalKeyAlias); 4] = [
        ('h', TerminalKeyAlias::Backspace),
        ('i', TerminalKeyAlias::Tab),
        ('m', TerminalKeyAlias::Enter),
        ('[', TerminalKeyAlias::Escape),
    ];
    if key.trim().is_empty() {
        return Err(KeyNotationFailure::Empty);
    }
    key.split_whitespace()
        .map(|chord| {
            let (mut rest, mut mods) = (chord, [false; 3]);
            while let Some((i, tail)) = MODIFIERS.iter().enumerate().find_map(|(i, m)| {
                let tail = rest.strip_prefix(m)?.strip_prefix('-')?;
                (!tail.is_empty()).then_some((i, tail))
            }) {
                if std::mem::replace(&mut mods[i], true) {
                    return Err(KeyNotationFailure::DuplicateModifier(
                        KeyNotationFailureDuplicateModifier {
                            chord: chord.into(),
                            modifier: [KeyModifier::Ctrl, KeyModifier::Alt, KeyModifier::Shift][i],
                        },
                    ));
                }
                rest = tail;
            }
            let [ctrl, alt, shift] = mods;
            let mut chars = rest.chars();
            let name = match (chars.next(), chars.next()) {
                (Some(c), None) => KeyName::Char(c),
                _ => NAMED
                    .iter()
                    .find_map(|&(n, name)| (n == rest).then_some(name))
                    .or_else(|| {
                        let n = rest.strip_prefix('f')?.parse().ok()?;
                        (1..=12).contains(&n).then_some(KeyName::F(n))
                    })
                    .ok_or_else(|| {
                        KeyNotationFailure::UnknownKey(KeyNotationFailureUnknownKey {
                            token: rest.into(),
                        })
                    })?,
            };
            if let KeyName::Char(c) = name {
                if c.is_control() {
                    return Err(KeyNotationFailure::ControlCharacter(
                        KeyNotationFailureControlCharacter {
                            character: c.to_string(),
                        },
                    ));
                }
                if shift {
                    return Err(KeyNotationFailure::ShiftedCharacter(
                        KeyNotationFailureShiftedCharacter {
                            chord: chord.into(),
                            character: c.to_string(),
                            control: ctrl,
                            alt,
                        },
                    ));
                }
                if ctrl && c.is_ascii_uppercase() {
                    return Err(KeyNotationFailure::UppercaseControl(
                        KeyNotationFailureUppercaseControl {
                            chord: chord.into(),
                        },
                    ));
                }
                if ctrl && let Some(&(_, alias)) = ALIASED.iter().find(|&&(a, _)| a == c) {
                    return Err(KeyNotationFailure::TerminalAlias(
                        KeyNotationFailureTerminalAlias {
                            chord: chord.into(),
                            alt,
                            alias,
                        },
                    ));
                }
            }
            Ok(KeyChord {
                name,
                ctrl,
                alt,
                shift,
            })
        })
        .collect()
}

/// Why a chord is off-limits to plugins entirely: interrupt and EOF are the
/// session's escape hatches, and a wedged handler on either would leave no
/// way to stop it or leave the shell.  The manifest rejects these at load,
/// guard or not.
pub(in crate::repl) fn reserved_action(chord: KeyChord) -> Option<ReservedKeyAction> {
    match (chord.name, chord.ctrl && !(chord.alt || chord.shift)) {
        (KeyName::Char('c'), true) => Some(ReservedKeyAction::Interrupt),
        (KeyName::Char('d'), true) => Some(ReservedKeyAction::EndOfFile),
        _ => None,
    }
}

/// The ral-owned built-in action at the dispatch table's tail for `chord`,
/// if any.  Every unmodified key except F1–F12 carries one — typing,
/// movement, deletion, completion, history, accept-line — so an unguarded
/// binding there would shadow it on every press and the manifest requires a
/// guard.  Modified chords and function keys carry none: they hold at most
/// an underlying editor default, which a plugin may replace outright.
pub(in crate::repl) fn builtin_action(chord: KeyChord) -> Option<BuiltinKeyAction> {
    if chord.ctrl || chord.alt || chord.shift {
        return None;
    }
    match chord.name {
        KeyName::Char(_) => Some(BuiltinKeyAction::TextInsertion),
        KeyName::Tab => Some(BuiltinKeyAction::Completion),
        KeyName::Enter => Some(BuiltinKeyAction::AcceptLine),
        KeyName::Escape => Some(BuiltinKeyAction::KeymapEscape),
        KeyName::Up | KeyName::Down => Some(BuiltinKeyAction::HistoryNavigation),
        KeyName::Left | KeyName::Right | KeyName::Home | KeyName::End => {
            Some(BuiltinKeyAction::CursorMovement)
        }
        KeyName::Backspace | KeyName::Delete => Some(BuiltinKeyAction::Deletion),
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
/// re-resolves by name at dispatch time.
pub(in crate::repl) struct RouterEntry {
    pub(in crate::repl) plugin: String,
    pub(in crate::repl) binding_idx: usize,
    pub(in crate::repl) key: String,
    pub(in crate::repl) chords: Vec<KeyChord>,
    pub(in crate::repl) guard: Option<regex::Regex>,
}

/// What a key press resolves to.
#[derive(Debug, PartialEq)]
pub(in crate::repl) enum Resolution {
    /// A plugin binding claimed the sequence; dispatch its handler after the
    /// editor returns.
    Claimed { plugin: String, binding_idx: usize },
    /// No entry claimed it: run the editor's built-in action.
    Default,
}

/// The ordered dispatch table, rebuilt whenever the plugin list changes
/// ([`PluginRuntime::keybindings_changed`](super::PluginRuntime::keybindings_changed)).
#[derive(Default)]
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
                        chords: kb.chords.clone(),
                        guard: kb.guard.clone(),
                    })
            })
            .collect();
        Self { entries }
    }

    /// First entry whose chord sequence is `chords` and whose guard allows;
    /// `Default` otherwise.  `pos` is a byte offset into `line`.
    pub(in crate::repl) fn resolve(
        &self,
        chords: &[KeyChord],
        line: &str,
        pos: usize,
    ) -> Resolution {
        self.entries
            .iter()
            .find(|e| e.chords == chords && guard_allows(e.guard.as_ref(), line, pos))
            .map_or(Resolution::Default, |e| Resolution::Claimed {
                plugin: e.plugin.clone(),
                binding_idx: e.binding_idx,
            })
    }

    /// The distinct bound sequences in table order — rustyline registers one
    /// conditional handler per sequence, so same-sequence entries never fight
    /// over a binding.
    pub(in crate::repl) fn bound_keys(&self) -> Vec<Vec<KeyChord>> {
        let mut out: Vec<Vec<KeyChord>> = Vec::new();
        for e in &self.entries {
            if !out.contains(&e.chords) {
                out.push(e.chords.clone());
            }
        }
        out
    }

    /// Entries that can never fire, paired with their blocker, for the
    /// load-time shadow lint.  An unguarded entry blocks every later entry on
    /// its sequence, and every entry whose sequence it is a proper prefix of
    /// wherever it sits: rustyline consults a bound prefix before reading on.
    pub(in crate::repl) fn dead_entries(&self) -> Vec<(&RouterEntry, &RouterEntry)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                self.entries
                    .iter()
                    .enumerate()
                    .find(|&(j, b)| {
                        b.guard.is_none()
                            && e.chords.starts_with(&b.chords)
                            && (j < i || b.chords.len() < e.chords.len())
                    })
                    .map(|(_, b)| (e, b))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(plugin: &str, bi: usize, key: &str, guard: Option<&str>) -> RouterEntry {
        RouterEntry {
            plugin: plugin.into(),
            binding_idx: bi,
            key: key.into(),
            chords: parse_key_notation(key).expect("test key parses"),
            guard: guard.map(|g| regex::Regex::new(g).expect("test guard compiles")),
        }
    }

    fn claimed(plugin: &str, bi: usize) -> Resolution {
        Resolution::Claimed {
            plugin: plugin.into(),
            binding_idx: bi,
        }
    }

    /// `parse_key_notation` reduces every supported notation to
    /// frontend-neutral [`KeyChord`]s — the modifier sets and the named keys
    /// the fzf/zoxide plugins bind, plus the bare-char and function-key forms.
    #[test]
    fn parse_key_notation_yields_neutral_chords() {
        use KeyName::*;
        for (key, name, ctrl, alt, shift) in [
            ("ctrl-r", Char('r'), true, false, false),
            ("alt-c", Char('c'), false, true, false),
            ("tab", Tab, false, false, false),
            ("t", Char('t'), false, false, false),
            ("f5", F(5), false, false, false),
            ("ctrl-alt-x", Char('x'), true, true, false),
            ("alt-ctrl-x", Char('x'), true, true, false),
            ("shift-f5", F(5), false, false, true),
            ("ctrl-left", Left, true, false, false),
            ("alt-enter", Enter, false, true, false),
            ("ctrl--", Char('-'), true, false, false),
        ] {
            assert_eq!(
                parse_key_notation(key),
                Ok(vec![KeyChord {
                    name,
                    ctrl,
                    alt,
                    shift
                }]),
                "{key}"
            );
        }
    }

    /// A notation is chords separated by spaces; runs of spaces are one
    /// separator.
    #[test]
    fn parse_key_notation_splits_sequences() {
        let ctrl = |c| KeyChord {
            name: KeyName::Char(c),
            ctrl: true,
            alt: false,
            shift: false,
        };
        assert_eq!(
            parse_key_notation("ctrl-x  ctrl-e"),
            Ok(vec![ctrl('x'), ctrl('e')])
        );
    }

    /// Each refusal names the offending text, and says what is wanted.
    #[test]
    fn parse_key_notation_refuses_with_the_offending_text() {
        for (key, expected) in [
            ("", KeyNotationFailure::Empty),
            ("   ", KeyNotationFailure::Empty),
            (
                "hyper-x",
                KeyNotationFailure::UnknownKey(KeyNotationFailureUnknownKey {
                    token: "hyper-x".into(),
                }),
            ),
            (
                "f13",
                KeyNotationFailure::UnknownKey(KeyNotationFailureUnknownKey {
                    token: "f13".into(),
                }),
            ),
            (
                "ctrl-",
                KeyNotationFailure::UnknownKey(KeyNotationFailureUnknownKey {
                    token: "ctrl-".into(),
                }),
            ),
            (
                "ctrl-x alt-ctrl-alt-y",
                KeyNotationFailure::DuplicateModifier(KeyNotationFailureDuplicateModifier {
                    chord: "alt-ctrl-alt-y".into(),
                    modifier: KeyModifier::Alt,
                }),
            ),
        ] {
            assert_eq!(parse_key_notation(key).unwrap_err(), expected, "{key:?}");
        }
        for key in ["shift-a", "shift-1", "alt-shift-a", "ctrl-shift-a"] {
            assert!(
                matches!(parse_key_notation(key), Err(KeyNotationFailure::ShiftedCharacter(fact)) if fact.chord == key && fact.character == key.chars().last().unwrap().to_string() && fact.control == key.starts_with("ctrl-") && fact.alt == key.starts_with("alt-"))
            );
        }
        for key in ["ctrl-C", "ctrl-alt-C"] {
            assert_eq!(
                parse_key_notation(key).unwrap_err(),
                KeyNotationFailure::UppercaseControl(KeyNotationFailureUppercaseControl {
                    chord: key.into()
                })
            );
        }
        for (key, alias, alt) in [
            ("ctrl-h", TerminalKeyAlias::Backspace, false),
            ("ctrl-i", TerminalKeyAlias::Tab, false),
            ("ctrl-m", TerminalKeyAlias::Enter, false),
            ("ctrl-[", TerminalKeyAlias::Escape, false),
            ("alt-ctrl-h", TerminalKeyAlias::Backspace, true),
        ] {
            assert_eq!(
                parse_key_notation(key).unwrap_err(),
                KeyNotationFailure::TerminalAlias(KeyNotationFailureTerminalAlias {
                    chord: key.into(),
                    alias,
                    alt
                })
            );
        }
        for (key, character) in [("\u{3}", "\u{3}"), ("ctrl-\u{1b}", "\u{1b}")] {
            assert_eq!(
                parse_key_notation(key).unwrap_err(),
                KeyNotationFailure::ControlCharacter(KeyNotationFailureControlCharacter {
                    character: character.into()
                })
            );
        }
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

    /// Dispatch is the first entry whose chords match and whose guard
    /// allows: guarded same-sequence entries compose as an ordered pattern
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
        assert_eq!(router.resolve(&tab, "git st", 6), claimed("a", 0));
        assert_eq!(router.resolve(&tab, "cd **", 5), claimed("b", 0));
        assert_eq!(router.resolve(&tab, "ls ", 3), Resolution::Default);
    }

    /// A sequence resolves as a whole: neither its prefix nor a longer
    /// sequence reaches its entry.
    #[test]
    fn resolve_matches_whole_sequences() {
        let router = KeyRouter {
            entries: vec![
                entry("a", 0, "ctrl-x ctrl-e", None),
                entry("b", 0, "ctrl-x ctrl-f", None),
            ],
        };
        let at = |s| router.resolve(&parse_key_notation(s).unwrap(), "", 0);
        assert_eq!(at("ctrl-x ctrl-e"), claimed("a", 0));
        assert_eq!(at("ctrl-x ctrl-f"), claimed("b", 0));
        assert_eq!(at("ctrl-x"), Resolution::Default);
        assert_eq!(at("ctrl-x ctrl-e ctrl-f"), Resolution::Default);
    }

    /// `bound_keys` yields each sequence once, in first-appearance order —
    /// rustyline registers exactly one handler per sequence.
    #[test]
    fn bound_keys_dedupes_in_order() {
        let router = KeyRouter {
            entries: vec![
                entry("a", 0, "tab", Some("x")),
                entry("b", 0, "ctrl-t", None),
                entry("c", 0, "tab", Some("y")),
                entry("d", 0, "ctrl-x ctrl-e", None),
            ],
        };
        assert_eq!(
            router.bound_keys(),
            ["tab", "ctrl-t", "ctrl-x ctrl-e"].map(|k| parse_key_notation(k).unwrap())
        );
    }

    /// A binding behind an earlier unguarded entry on the same sequence is
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

    /// An unguarded entry on a proper prefix kills every longer sequence,
    /// whether it loads before or after; a guarded prefix kills nothing.
    #[test]
    fn dead_entries_flags_sequences_behind_an_unguarded_prefix() {
        let dead = |router: &KeyRouter| -> Vec<(String, String)> {
            router
                .dead_entries()
                .into_iter()
                .map(|(dead, blocker)| (dead.plugin.clone(), blocker.plugin.clone()))
                .collect()
        };
        let router = KeyRouter {
            entries: vec![
                entry("a", 0, "ctrl-x ctrl-e", None),
                entry("b", 0, "ctrl-x", None),
                entry("c", 0, "ctrl-x ctrl-f", Some("y")),
            ],
        };
        assert_eq!(
            dead(&router),
            vec![
                ("a".to_string(), "b".to_string()),
                ("c".to_string(), "b".to_string())
            ]
        );
        let router = KeyRouter {
            entries: vec![
                entry("a", 0, "ctrl-x", Some("x")),
                entry("b", 0, "ctrl-x ctrl-e", None),
            ],
        };
        assert_eq!(dead(&router), vec![]);
    }
}
