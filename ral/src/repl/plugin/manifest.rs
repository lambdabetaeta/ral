//! Plugin manifest types and parsing.
//!
//! A plugin manifest is a record returned by a plugin's top-level block.  The
//! engine's load door parses it into the first-order [`Manifest`] the host is
//! told, plus the [`ManifestHandlers`] it registers itself; the host
//! re-validates the manifest into a [`LoadedPlugin`].

use super::HookHealth;
use super::router::{CTRL_X, KeyChord, builtin_action, parse_key_notation, reserved_action};
use ral_core::data::FOValue;
use ral_core::error::Error;
use ral_core::fact::{
    DataStep, Expected, GrantFailure, GrantFailureRefusedKey, GrantForm, HostFailure, InvalidCause,
    InvalidFailure, KeybindingFailure, KeybindingFailureGuardRegex, KeybindingFailureNotation,
    KeybindingFailureReservedAction, KeybindingFailureSequencePrefix,
    KeybindingFailureUnguardedBuiltinShadow, PluginFailure, PluginFailureUnknownHookEvent,
    RalFailure, Reason, ShapeFailure, ValueShape,
};
use ral_core::shell::runtime::settings_map;
use ral_core::{Map, Value, label, record};
use std::collections::HashMap;
use strum::{IntoStaticStr, VariantArray};

/// Hook events recognised in manifests.  Typos are load errors so a
/// plugin cannot silently register a handler that never fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub(crate) enum HookEvent {
    BufferChange,
    Complete,
    PreExec,
    PostExec,
    Chpwd,
    Prompt,
}

label!(HookEvent);

/// A manifest's first-order part: what the host needs to route keys and
/// fire hooks.  Handlers never cross; they stay registered engine-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) name: String,
    pub(crate) hooks: Vec<HookEvent>,
    pub(crate) keybindings: Vec<KeySpec>,
}

record!(Manifest {
    name: "name",
    hooks: "hooks",
    keybindings: "keybindings",
});

/// One keybinding as the manifest spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeySpec {
    pub(crate) key: String,
    pub(crate) guard: Option<String>,
}

record!(KeySpec {
    key: "key",
    guard: "guard",
});

/// One keybinding, validated.  `chords` is the parsed form of `key`: one
/// chord, or a sequence beginning with ctrl-x; `guard`, when present, is a
/// regex the binding claims its keys under only when it matches the text left
/// of the cursor, otherwise dispatch falls to the next table entry or the
/// editor's built-in action.
#[derive(Debug, Clone)]
pub(crate) struct KeyBinding {
    pub(crate) key: String,
    pub(crate) chords: Vec<KeyChord>,
    pub(crate) guard: Option<regex::Regex>,
}

impl KeySpec {
    /// The one validation both ends run: the door to refuse early, the host
    /// because its router is what the binding must fit.
    pub(crate) fn bind(&self) -> Result<KeyBinding, KeybindingFailure> {
        let key = &self.key;
        let chords = parse_key_notation(key).map_err(|cause| {
            KeybindingFailure::Notation(KeybindingFailureNotation {
                key: key.clone(),
                cause,
            })
        })?;
        if let Some(action) = chords.iter().copied().find_map(reserved_action) {
            return Err(KeybindingFailure::ReservedAction(
                KeybindingFailureReservedAction {
                    key: key.clone(),
                    action,
                },
            ));
        }
        if chords.len() > 1 && chords[0] != CTRL_X {
            return Err(KeybindingFailure::SequencePrefix(
                KeybindingFailureSequencePrefix { key: key.clone() },
            ));
        }
        let guard = self
            .guard
            .as_deref()
            .map(|pattern| {
                regex::Regex::new(pattern).map_err(|error| {
                    KeybindingFailure::GuardRegex(KeybindingFailureGuardRegex {
                        key: key.clone(),
                        pattern: pattern.into(),
                        cause: ral_core::shell::builtins::util::regex_failure(pattern, &error)
                            .cause,
                    })
                })
            })
            .transpose()?;
        if guard.is_none()
            && let [chord] = chords.as_slice()
            && let Some(action) = builtin_action(*chord)
        {
            return Err(KeybindingFailure::UnguardedBuiltinShadow(
                KeybindingFailureUnguardedBuiltinShadow {
                    key: key.clone(),
                    action,
                },
            ));
        }
        Ok(KeyBinding {
            key: key.clone(),
            chords,
            guard,
        })
    }
}

/// A loaded plugin, as the host keeps it.  Hooks fire from the editor loop;
/// keybindings register against rustyline on the next sync.  `state_cell` is
/// the plugin's persistent state, read and updated via `_ed-state`.
///
/// Plugins run with host authority — the manifest cannot narrow it.  A
/// `capabilities:` key is rejected as a load error rather than accepted and
/// ignored, so a plugin author cannot mistake the declaration for
/// confinement; to confine a plugin invocation, wrap it in `grant { ... }`.
#[derive(Debug, Clone)]
pub(crate) struct LoadedPlugin {
    pub(crate) name: String,
    /// The hook events this plugin registered.  The handler values
    /// themselves live only in the engine's hook table.
    pub(crate) hooks: Vec<HookEvent>,
    pub(crate) keybindings: Vec<KeyBinding>,
    pub(crate) state_cell: Option<FOValue>,
    /// Circuit-breaker state of this plugin's braked hooks, by event.
    pub(crate) health: HashMap<HookEvent, HookHealth>,
}

impl LoadedPlugin {
    /// The host's record of `manifest`, its keybindings re-validated.
    pub(crate) fn admit(manifest: Manifest) -> Result<Self, KeybindingFailure> {
        Ok(Self {
            keybindings: manifest
                .keybindings
                .iter()
                .map(KeySpec::bind)
                .collect::<Result<_, _>>()?,
            name: manifest.name,
            hooks: manifest.hooks,
            state_cell: None,
            health: HashMap::new(),
        })
    }
}

/// The handler values a manifest parse extracts, which the engine registers
/// into its hook table and env; `aliases` are also what unload removes.
#[derive(Debug)]
pub(super) struct ManifestHandlers {
    pub(super) hooks: Vec<(HookEvent, Value)>,
    pub(super) keybindings: Vec<(String, Value)>,
    pub(super) aliases: Vec<(String, Value)>,
}

/// Parse a plugin manifest value into its first-order part plus the handler
/// values the engine registers.
pub(super) fn keybinding_refusal(cause: KeybindingFailure) -> Error {
    Error::new(Reason::Host(Box::new(HostFailure::Ral(
        RalFailure::Keybinding(cause),
    ))))
}

fn invalid_at(path: Vec<DataStep>, cause: InvalidCause) -> Error {
    Error::new(Reason::Invalid(Box::new(InvalidFailure {
        operation: "plugin manifest".into(),
        path,
        cause,
    })))
}

fn expected_at(path: Vec<DataStep>, value: &Value, expected: Expected) -> Error {
    invalid_at(
        path,
        InvalidCause::Shape(ShapeFailure {
            expected,
            found: value.found(),
        }),
    )
}

pub(super) fn parse(val: &Value) -> Result<(Manifest, ManifestHandlers), Error> {
    let Some(map) = settings_map(val) else {
        return Err(expected_at(vec![], val, Expected::Shape(ValueShape::Map)));
    };
    let table = ral_core::typecheck::contract::declared(ral_core::typecheck::Form::Manifest);
    for key in map.keys() {
        match table.holds(key) {
            Some(ral_core::typecheck::contract::Holds::Refused(cause)) => {
                return Err(invalid_at(
                    vec![],
                    InvalidCause::Grant(GrantFailure::RefusedKey(GrantFailureRefusedKey {
                        form: GrantForm::Manifest,
                        key: key.into(),
                        cause: *cause,
                    })),
                ));
            }
            Some(_) => {}
            None => {
                return Err(invalid_at(
                    vec![],
                    InvalidCause::UnknownField(table.unknown_key(key)),
                ));
            }
        }
    }
    let name = match map.get("name").as_deref() {
        Some(Value::String(s)) => s.to_string(),
        Some(other) => {
            return Err(expected_at(
                vec![DataStep::Field("name".into())],
                other,
                Expected::Shape(ValueShape::String),
            ));
        }
        None => {
            return Err(invalid_at(
                vec![],
                InvalidCause::MissingField("name".into()),
            ));
        }
    };
    let aliases = match map.get("aliases").as_deref() {
        Some(Value::Map(m)) => parse_aliases(m)?,
        Some(other) => {
            return Err(expected_at(
                vec![DataStep::Field("aliases".into())],
                other,
                Expected::Shape(ValueShape::Map),
            ));
        }
        None => Vec::new(),
    };
    let hooks = match map.get("hooks").as_deref() {
        Some(Value::Map(m)) => parse_hooks(m)?,
        Some(other) => {
            return Err(expected_at(
                vec![DataStep::Field("hooks".into())],
                other,
                Expected::Shape(ValueShape::Map),
            ));
        }
        None => Vec::new(),
    };
    let (keybindings, keybinding_handlers) = match map.get("keybindings").as_deref() {
        Some(Value::List(l)) => parse_keybindings(l.iter().map(std::borrow::Cow::into_owned))?,
        Some(other) => {
            return Err(expected_at(
                vec![DataStep::Field("keybindings".into())],
                other,
                Expected::Shape(ValueShape::List),
            ));
        }
        None => (Vec::new(), Vec::new()),
    };
    let manifest = Manifest {
        name,
        hooks: hooks.iter().map(|&(event, _)| event).collect(),
        keybindings,
    };
    let handlers = ManifestHandlers {
        hooks,
        keybindings: keybinding_handlers,
        aliases,
    };
    Ok((manifest, handlers))
}

fn parse_hooks(entries: &Map) -> Result<Vec<(HookEvent, Value)>, Error> {
    entries
        .iter()
        .map(|(name, value)| {
            let event = HookEvent::VARIANTS
                .iter()
                .copied()
                .find(|&e| <&str>::from(e) == name)
                .ok_or_else(|| {
                    super::refusal(PluginFailure::UnknownHookEvent(
                        PluginFailureUnknownHookEvent {
                            event: name.into(),
                            offered: HookEvent::VARIANTS
                                .iter()
                                .copied()
                                .map(|e| <&str>::from(e).into())
                                .collect(),
                        },
                    ))
                })?;
            if !matches!(value.as_ref(), Value::Thunk(_)) {
                return Err(expected_at(
                    vec![
                        DataStep::Field("hooks".into()),
                        DataStep::Field(name.into()),
                    ],
                    &value,
                    Expected::Closure,
                ));
            }
            Ok((event, value.into_owned()))
        })
        .collect()
}

type KeybindingHandlers = Vec<(String, Value)>;

fn parse_keybindings<I>(entries: I) -> Result<(Vec<KeySpec>, KeybindingHandlers), Error>
where
    I: Iterator<Item = Value>,
{
    let mut out = Vec::new();
    let mut handlers = Vec::new();
    for (index, entry) in entries.enumerate() {
        let path = || {
            vec![
                DataStep::Field("keybindings".into()),
                DataStep::Index(index.into()),
            ]
        };
        let field_path = |field: &str| {
            let mut path = path();
            path.push(DataStep::Field(field.into()));
            path
        };
        let Value::Map(map) = entry else {
            return Err(expected_at(
                path(),
                &entry,
                Expected::Shape(ValueShape::Map),
            ));
        };
        let key = match map.get("key").as_deref() {
            Some(Value::String(s)) => s.to_string(),
            Some(other) => {
                return Err(expected_at(
                    field_path("key"),
                    other,
                    Expected::Shape(ValueShape::String),
                ));
            }
            None => return Err(invalid_at(path(), InvalidCause::MissingField("key".into()))),
        };
        let handler = match map.get("handler").as_deref() {
            Some(h @ Value::Thunk(_)) => h.clone(),
            Some(other) => {
                return Err(expected_at(field_path("handler"), other, Expected::Closure));
            }
            None => {
                return Err(invalid_at(
                    path(),
                    InvalidCause::MissingField("handler".into()),
                ));
            }
        };
        let guard = match map.get("guard").as_deref() {
            Some(Value::String(g)) => Some(g.to_string()),
            Some(other) => {
                return Err(expected_at(
                    field_path("guard"),
                    other,
                    Expected::Shape(ValueShape::String),
                ));
            }
            None => None,
        };
        let spec = KeySpec { key, guard };
        spec.bind().map_err(keybinding_refusal)?;
        handlers.push((spec.key.clone(), handler));
        out.push(spec);
    }
    Ok((out, handlers))
}

fn parse_aliases(entries: &Map) -> Result<Vec<(String, Value)>, Error> {
    entries
        .iter()
        .map(|(name, value)| {
            if !matches!(value.as_ref(), Value::Thunk(_)) {
                return Err(expected_at(
                    vec![
                        DataStep::Field("aliases".into()),
                        DataStep::Field(name.into()),
                    ],
                    &value,
                    Expected::Closure,
                ));
            }
            Ok((name.to_string(), value.into_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::shell::runtime::Closure;

    fn invalid_reason(error: &Error) -> &InvalidFailure {
        let Reason::Invalid(failure) = &error.reason else {
            panic!("expected invalid manifest, got {:?}", error.reason)
        };
        assert_eq!(error.hints(), []);
        assert!(error.context().is_none());
        failure
    }

    fn keybinding_reason(error: &Error) -> &KeybindingFailure {
        let Reason::Host(host) = &error.reason else {
            panic!("expected host refusal")
        };
        let HostFailure::Ral(RalFailure::Keybinding(cause)) = host.as_ref() else {
            panic!("expected keybinding refusal")
        };
        assert_eq!(error.hints(), []);
        assert!(error.context().is_none());
        cause
    }

    /// A `capabilities:` key is rejected with an error that names the key and
    /// points at `grant` (the authority rationale lives on the struct doc).
    #[test]
    fn capabilities_key_is_rejected() {
        let manifest = Value::map(vec![
            ("name".into(), Value::string("p")),
            ("capabilities".into(), Value::map(vec![])),
        ]);
        let err = parse(&manifest).unwrap_err();
        assert_eq!(
            invalid_reason(&err).cause,
            InvalidCause::Grant(GrantFailure::RefusedKey(GrantFailureRefusedKey {
                form: GrantForm::Manifest,
                key: "capabilities".into(),
                cause: ral_core::fact::RefusedGrantKey::ManifestCapabilitiesUnenforced,
            }))
        );
        assert_eq!(
            invalid_reason(&err).path,
            Vec::<ral_core::fact::DataStep>::new()
        );
        assert!(
            err.reason
                .advice()
                .iter()
                .any(|hint| hint.contains("grant"))
        );
        assert!(err.describe().contains("capabilities"));
    }

    /// A key the table does not name is refused with the table's own list —
    /// the door a manifest arriving from a factory meets, having no row for
    /// the checker to hold.
    #[test]
    fn unknown_key_names_the_manifests_list() {
        let manifest = Value::map(vec![
            ("name".into(), Value::string("p")),
            ("hookz".into(), Value::map(vec![])),
        ]);
        let err = parse(&manifest).unwrap_err();
        let table = ral_core::typecheck::contract::declared(ral_core::typecheck::Form::Manifest);
        assert_eq!(
            invalid_reason(&err).cause,
            InvalidCause::UnknownField(table.unknown_key("hookz"))
        );
        assert_eq!(
            invalid_reason(&err).path,
            Vec::<ral_core::fact::DataStep>::new()
        );
        assert!(err.describe().contains("hookz"));
    }

    /// Every key `Form::Manifest`'s table declares reaches its own handling
    /// in `LoadedPlugin::parse`: a `Holds::Refused` key errors with its own
    /// advice, and every other key parses with a minimal value of its
    /// declared shape rather than being refused as unknown or silently
    /// dropped.
    #[test]
    fn every_declared_manifest_key_is_handled_by_parse() {
        use ral_core::typecheck::contract::Holds;

        let table = ral_core::typecheck::contract::declared(ral_core::typecheck::Form::Manifest);
        for key in table.keys {
            let mut fields = vec![("name".into(), Value::string("p"))];
            if key.label != "name" {
                let value = if key.label == "keybindings" {
                    Value::list(vec![])
                } else {
                    Value::map(vec![])
                };
                fields.push((key.label.into(), value));
            }
            let manifest = Value::map(fields);
            let result = parse(&manifest);
            match &key.holds {
                Holds::Refused(advice) => {
                    let err = result.expect_err("refused key must error");
                    assert_eq!(
                        invalid_reason(&err).cause,
                        InvalidCause::Grant(GrantFailure::RefusedKey(GrantFailureRefusedKey {
                            form: GrantForm::Manifest,
                            key: key.label.into(),
                            cause: *advice
                        }))
                    );
                }
                _ => {
                    result.unwrap_or_else(|err| {
                        panic!(
                            "key '{}' should parse cleanly, got: {}",
                            key.label,
                            err.describe()
                        )
                    });
                }
            }
        }
    }

    /// A trivial block-shaped `Value::Thunk` handler for well-formed
    /// keybinding entries.
    fn dummy_block() -> Value {
        Value::Thunk(Closure::closed(ral_core::source::Spanned::synthetic(
            ral_core::ir::CompKind::Return(ral_core::ir::Val::Unit),
        )))
    }

    fn keybinding_entry(key: &str, guard: Option<&str>) -> Value {
        let mut fields = vec![
            ("key".into(), Value::string(key)),
            ("handler".into(), dummy_block()),
        ];
        if let Some(g) = guard {
            fields.push(("guard".into(), Value::string(g)));
        }
        Value::map(fields)
    }

    /// Every manifest field with a declared type rejects a value of any
    /// other type with an error naming the field and both types — nothing
    /// stringifies or silently degrades to an empty collection.
    #[test]
    fn wrong_typed_fields_are_load_errors() {
        for (field, expected) in [
            ("name", ValueShape::String),
            ("aliases", ValueShape::Map),
            ("hooks", ValueShape::Map),
            ("keybindings", ValueShape::List),
        ] {
            let manifest = Value::map(vec![
                ("name".into(), Value::string("p")),
                (field.into(), Value::Int(42)),
            ]);
            let err = parse(&manifest).unwrap_err();
            assert_eq!(
                invalid_reason(&err).path,
                vec![DataStep::Field(field.into())]
            );
            assert_eq!(
                invalid_reason(&err).cause,
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(expected),
                    found: ral_core::fact::Found::Int(42)
                })
            );
            assert!(err.describe().contains(field));
        }
    }

    /// `()` is a manifest with nothing set, so it lacks `name`.
    #[test]
    fn unit_manifest_lacks_a_name() {
        let err = parse(&Value::Unit).unwrap_err();
        assert_eq!(
            invalid_reason(&err).cause,
            InvalidCause::MissingField("name".into())
        );
        assert_eq!(
            invalid_reason(&err).path,
            Vec::<ral_core::fact::DataStep>::new()
        );
    }

    /// A non-block keybinding handler rejects the manifest — it is not
    /// warned about and skipped.
    #[test]
    fn non_block_handler_is_load_error() {
        let entry = Value::map(vec![
            ("key".into(), Value::string("ctrl-t")),
            ("handler".into(), Value::Int(3)),
        ]);
        let err = parse_keybindings(std::iter::once(entry)).unwrap_err();
        assert_eq!(
            invalid_reason(&err).path,
            vec![
                DataStep::Field("keybindings".into()),
                DataStep::Index(0_usize.into()),
                DataStep::Field("handler".into())
            ]
        );
        assert_eq!(
            invalid_reason(&err).cause,
            InvalidCause::Shape(ShapeFailure {
                expected: Expected::Closure,
                found: ral_core::fact::Found::Int(3)
            })
        );
    }

    /// An invalid guard regex is a load error naming the key.
    #[test]
    fn invalid_guard_regex_is_load_error() {
        let err = parse_keybindings(std::iter::once(keybinding_entry("ctrl-t", Some("("))))
            .expect_err("malformed regex must be rejected");
        assert!(
            err.describe().contains("ctrl-t") && err.describe().contains("guard"),
            "error should name the key and the guard, got: {}",
            err.describe()
        );
    }

    /// An unguarded binding on a chord with a ral-owned built-in action is
    /// rejected — tab/completion is one instance of the general rule.
    #[test]
    fn unguarded_builtin_chord_is_load_error() {
        use ral_core::fact::BuiltinKeyAction;
        for (key, action) in [
            ("tab", BuiltinKeyAction::Completion),
            ("enter", BuiltinKeyAction::AcceptLine),
            ("up", BuiltinKeyAction::HistoryNavigation),
            ("t", BuiltinKeyAction::TextInsertion),
        ] {
            let err = parse_keybindings(std::iter::once(keybinding_entry(key, None))).unwrap_err();
            assert_eq!(
                keybinding_reason(&err),
                &KeybindingFailure::UnguardedBuiltinShadow(
                    KeybindingFailureUnguardedBuiltinShadow {
                        key: key.into(),
                        action
                    }
                )
            );
            assert!(
                err.reason
                    .advice()
                    .iter()
                    .any(|hint| hint.contains("guard"))
            );
        }
    }

    /// The same chords parse with a guard: they compose with the built-in
    /// tail instead of shadowing it.
    #[test]
    fn guarded_builtin_chord_parses() {
        let (out, _) =
            parse_keybindings(std::iter::once(keybinding_entry("enter", Some(r"^git "))))
                .expect("guarded enter parses");
        assert_eq!(out.len(), 1);
    }

    /// Interrupt and EOF are reserved outright — a guard does not help.
    #[test]
    fn reserved_chords_are_rejected_guard_or_not() {
        for (key, guard) in [("ctrl-c", Some(r"\S")), ("ctrl-d", None)] {
            let err = parse_keybindings(std::iter::once(keybinding_entry(key, guard)))
                .expect_err("reserved chord must be rejected");
            assert!(
                err.describe().contains(key) && err.describe().contains("reserved"),
                "error should name the key as reserved, got: {}",
                err.describe()
            );
        }
    }

    /// A key sequence begins with plain ctrl-x, the prefix the editor leaves
    /// free; anything else is refused, guard or not.
    #[test]
    fn key_sequences_begin_with_ctrl_x() {
        let (out, _) =
            parse_keybindings(std::iter::once(keybinding_entry("ctrl-x ctrl-e", None))).unwrap();
        assert_eq!(out.len(), 1);
        for key in ["escape escape", "alt-x y", "ctrl-alt-x y"] {
            let err =
                parse_keybindings(std::iter::once(keybinding_entry(key, Some("x")))).unwrap_err();
            assert_eq!(
                keybinding_reason(&err),
                &KeybindingFailure::SequencePrefix(KeybindingFailureSequencePrefix {
                    key: key.into()
                })
            );
            assert!(
                err.reason
                    .advice()
                    .iter()
                    .any(|hint| hint.contains("ctrl-x"))
            );
        }
    }

    /// Shift applies to named keys; for a character the shifted character is
    /// itself the key.
    #[test]
    fn shift_with_a_character_is_load_error() {
        let err =
            parse_keybindings(std::iter::once(keybinding_entry("shift-a", Some("x")))).unwrap_err();
        assert!(
            matches!(keybinding_reason(&err), KeybindingFailure::Notation(KeybindingFailureNotation { cause: ral_core::fact::KeyNotationFailure::ShiftedCharacter(fact), .. }) if fact.character == "a" && !fact.control && !fact.alt)
        );
        assert!(err.reason.advice().iter().any(|hint| hint.contains("'A'")));
        parse_keybindings(std::iter::once(keybinding_entry("shift-f5", None))).unwrap();
    }

    /// Reserved means exactly ctrl-c / ctrl-d: another modifier frees the chord.
    #[test]
    fn ctrl_alt_c_is_not_reserved() {
        parse_keybindings(std::iter::once(keybinding_entry("ctrl-alt-c", None)))
            .expect("ctrl-alt-c binds unguarded");
    }
}
