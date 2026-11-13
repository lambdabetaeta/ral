//! A form's options and a host file's tail are one thing: a record literal
//! whose keys a table names, checked key by key during generation
//! (`Generator::check_options`).
//!
//! The runtime decoders keep their own match over the values; the sync tests walk
//! each table.

mod grant;

use std::fmt;

pub(crate) use grant::{DIR_TARGET, EDITOR, EXEC_TARGET, GPU, NET, PLACE, SHELL};

use super::error::{Clash, Reason, Standing};
use super::unify::Unifier;
use crate::fact::{
    GrantFailure, GrantFailureUnknownVocabulary, RefusedGrantKey, UnknownField, Vocabulary,
};
pub(crate) use crate::text::code;
use crate::ty::{Label, Row, RowNode, TAG_PREFIX, Ty, TyNode};

/// Which declared table.  An enum rather than a name, so a lookup cannot miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Within,
    Grant,
    Rc,
    Manifest,
}

/// What a table holds one of its labels to.
pub enum Holds {
    /// Held at the ground type this builds.  A function, not a type: a table
    /// is a `static`, and an interned type is made at run time.
    At(fn() -> Ty),
    /// The decoders': a fresh variable per occurrence, nothing imposed.
    Decoded,
    /// The handler map.
    Handlers,
    /// `within`'s catch-all handler, the one value inferred in context.
    CatchAll,
    /// A nested table, written as a record and checked key by key as its parent is.
    Table(&'static Table),
    /// Known, and refused, with advice of its own rather than "unknown key".
    Refused(RefusedGrantKey),
}

/// The tags one closed variant row offers, as the words a clash with it uses.
///
/// The row itself is minted from `row`, so what a clash names and what the
/// checker admits are one list; the decoder words its refusals from the same
/// vocabulary.
pub struct Vocab {
    pub id: Vocabulary,
    /// The row's tags with their payloads, in the order a refusal lists them.
    pub row: fn() -> Vec<(String, Ty)>,
    /// Advice for a value that is no tag at all.
    pub untagged: &'static str,
    /// Advice for a tag whose payload is the wrong type.
    pub payload: &'static str,
    /// Tags whose payload is a vocabulary of its own, held to it first so that
    /// a clash inside the payload is worded by the row it met.
    pub within: &'static [(&'static str, &'static Self)],
}

impl fmt::Debug for Vocab {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vocab")
            .field("noun", &self.noun())
            .field("row", &(self.row)())
            .field("untagged", &self.untagged)
            .field("payload", &self.payload)
            .field("within", &self.within)
            // Callback identity is not vocabulary content.
            .finish_non_exhaustive()
    }
}

impl Vocab {
    /// What a member is called: "place".
    pub fn noun(&self) -> &'static str {
        self.id.noun()
    }

    /// The closed variant this vocabulary is.
    pub(crate) fn ty(&self) -> Ty {
        Ty::variant(
            (self.row)()
                .into_iter()
                .rev()
                .fold(Row::empty(), |rest, (tag, ty)| {
                    Row::extend(Label::Case(tag), ty, rest)
                }),
        )
    }

    /// The tags as a refusal writes them, `` `cwd ``.
    pub fn tags(&self) -> Vec<String> {
        (self.row)()
            .into_iter()
            .map(|(tag, _)| format!("{TAG_PREFIX}{tag}"))
            .collect()
    }

    /// The advice kept for `tag`, a tag refused on purpose.
    pub fn miss(&self, tag: &str) -> Option<String> {
        self.id.miss(tag)
    }

    /// The rejected vocabulary member, without diagnostic prose as identity.
    pub fn refuse(&self, tag: &str) -> GrantFailure {
        GrantFailure::UnknownVocabulary(GrantFailureUnknownVocabulary {
            vocabulary: self.id,
            tag: tag.to_owned(),
            offered: (self.row)().into_iter().map(|(tag, _)| tag).collect(),
        })
    }
}

/// One label a table names.
pub struct Key {
    pub label: &'static str,
    pub holds: Holds,
    pub required: bool,
    /// Source text of the key's empty value, when one is declared.
    pub blank: Option<&'static str>,
    /// The sentence a clash here earns; the form's own when `None`.
    pub reason: Option<Reason>,
}

/// A declared table: the form that owns it, for blame, and its closed keyset.
pub struct Table {
    pub form: &'static str,
    pub keys: &'static [Key],
    /// Whether the file may end in a written one-parameter block, the options,
    /// whose own tail is the settings written out.
    pub factory: bool,
}

impl Table {
    /// The labels this table offers a writer — every key but a refused one,
    /// which is known and is not on offer.
    pub fn offered(&self) -> Vec<&'static str> {
        self.keys
            .iter()
            .filter(|k| !matches!(k.holds, Holds::Refused(_)))
            .map(|k| k.label)
            .collect()
    }

    /// A key this table does not name, with its declared alternatives.
    pub fn unknown_key(&self, key: &str) -> UnknownField {
        UnknownField {
            field: key.to_owned(),
            offered: self.offered().into_iter().map(str::to_owned).collect(),
        }
    }

    /// What this table says about `label`, or `None` for a label it does not
    /// name at all.
    pub fn holds(&self, label: &str) -> Option<&Holds> {
        self.keys
            .iter()
            .find(|k| k.label == label)
            .map(|k| &k.holds)
    }
}

/// `within`'s options.  `handlers:` is a map.  `env:` is heterogeneous and
/// `parse_env`'s to judge, so it stays the decoder's.
///
/// The catch-all stands in for every command, so it is a function of the name
/// and the argv that is a command.
static WITHIN: Table = Table {
    form: "within",
    keys: &[
        Key {
            label: "dir",
            holds: Holds::At(Ty::string),
            required: false,
            blank: None,
            reason: None,
        },
        Key {
            label: "env",
            holds: Holds::Decoded,
            required: false,
            blank: None,
            reason: None,
        },
        Key {
            label: "handler",
            holds: Holds::CatchAll,
            required: false,
            blank: None,
            reason: Some(Reason::StandsIn(Standing::EveryCommand)),
        },
        Key {
            label: "handlers",
            holds: Holds::Handlers,
            required: false,
            blank: None,
            reason: Some(Reason::Handlers),
        },
    ],
    factory: false,
};

/// The rc file's twelve required keys. `RcPlan::decode` applies their values.
static RC: Table = Table {
    form: "rc",
    keys: &[
        Key {
            label: "umask",
            holds: Holds::At(|| SETTING_STRING.ty()),
            required: true,
            blank: Some("`inherit"),
            reason: Some(Reason::Vocabulary(&SETTING_STRING)),
        },
        Key {
            label: "limits",
            holds: Holds::Table(&LIMITS),
            required: true,
            blank: Some(LIMITS_BLANK),
            reason: None,
        },
        Key {
            label: "env",
            holds: Holds::Decoded,
            required: true,
            blank: Some("[:]"),
            reason: None,
        },
        Key {
            label: "prompt",
            holds: Holds::Decoded,
            required: true,
            blank: Some("`inherit"),
            reason: None,
        },
        Key {
            label: "handlers",
            holds: Holds::Handlers,
            required: true,
            blank: Some("[:]"),
            reason: Some(Reason::Handlers),
        },
        Key {
            label: "edit-mode",
            holds: Holds::At(Ty::string),
            required: true,
            blank: Some("emacs"),
            reason: None,
        },
        Key {
            label: "bell",
            holds: Holds::At(Ty::bool),
            required: true,
            blank: Some("false"),
            reason: None,
        },
        Key {
            label: "surface",
            holds: Holds::At(Ty::string),
            required: true,
            blank: Some("readline"),
            reason: None,
        },
        Key {
            label: "recursion-limit",
            holds: Holds::At(|| SETTING_INT.ty()),
            required: true,
            blank: Some("`inherit"),
            reason: Some(Reason::Vocabulary(&SETTING_INT)),
        },
        Key {
            label: "plugins",
            holds: Holds::Decoded,
            required: true,
            blank: Some("[:]"),
            reason: None,
        },
        Key {
            label: "startup",
            holds: Holds::Decoded,
            required: true,
            blank: Some("`inherit"),
            reason: None,
        },
        Key {
            label: "theme",
            holds: Holds::Table(&THEME),
            required: true,
            blank: Some(THEME_BLANK),
            reason: None,
        },
    ],
    factory: false,
};

macro_rules! setting_table {
    ($(#[$doc:meta])* $table:ident, $blank:ident, $form:literal, $vocab:ident; $($label:literal),+ $(,)?) => {
        $(#[$doc])*
        pub static $table: Table = Table {
            form: $form,
            keys: &[$(Key {
                label: $label,
                holds: Holds::At(|| $vocab.ty()),
                required: true,
                blank: Some("`inherit"),
                reason: Some(Reason::Vocabulary(&$vocab)),
            }),+],
            factory: false,
        };
        const $blank: &str = setting_table!(@blank $($label),+);
    };
    (@blank $first:literal $(, $rest:literal)*) => {
        concat!("[", $first, ": `inherit" $(, ", ", $rest, ": `inherit")*, "]")
    };
}

setting_table!(
    /// Child resource limits, each inherited or explicitly set.
    LIMITS, LIMITS_BLANK, "rc limits", SETTING_INT;
    "open-files", "core-bytes", "stack-bytes"
);

setting_table!(
    /// Output styling, each inherited or explicitly set.
    THEME, THEME_BLANK, "rc theme", SETTING_STRING;
    "value-prefix", "value-color"
);

static SETTING_STRING: Vocab = Vocab {
    id: Vocabulary::Setting,
    row: || setting_row(Ty::string()),
    untagged: "a setting is tagged: write `` `inherit `` to keep the current value or `` `set v `` to replace it",
    payload: "`` `inherit `` takes no payload; `` `set `` takes a String",
    within: &[],
};

static SETTING_INT: Vocab = Vocab {
    id: Vocabulary::Setting,
    row: || setting_row(Ty::int()),
    untagged: "a setting is tagged: write `` `inherit `` to keep the current value or `` `set v `` to replace it",
    payload: "`` `inherit `` takes no payload; `` `set `` takes an Int",
    within: &[],
};

fn setting_row(value: Ty) -> Vec<(String, Ty)> {
    vec![("inherit".into(), Ty::unit()), ("set".into(), value)]
}

/// The plugin manifest's four keys, and the fifth that is refused.
///
/// A plugin runs with host authority and the manifest cannot narrow it, so
/// `capabilities:` earns its own sentence rather than being folded into
/// "unknown key, here is the list": the advice is the only thing that
/// message carries.
static MANIFEST: Table = Table {
    form: "plugin manifest",
    keys: &[
        Key {
            label: "name",
            holds: Holds::At(Ty::string),
            required: true,
            blank: None,
            reason: None,
        },
        Key {
            label: "handlers",
            holds: Holds::Handlers,
            required: false,
            blank: None,
            reason: Some(Reason::Handlers),
        },
        Key {
            label: "hooks",
            holds: Holds::Decoded,
            required: false,
            blank: None,
            reason: None,
        },
        Key {
            label: "keybindings",
            holds: Holds::Decoded,
            required: false,
            blank: None,
            reason: None,
        },
        Key {
            label: "capabilities",
            holds: Holds::Refused(RefusedGrantKey::ManifestCapabilitiesUnenforced),
            required: false,
            blank: None,
            reason: None,
        },
    ],
    factory: true,
};

/// The table `form` declares.
pub fn declared(form: Form) -> &'static Table {
    match form {
        Form::Within => &WITHIN,
        Form::Grant => &grant::GRANT,
        Form::Rc => &RC,
        Form::Manifest => &MANIFEST,
    }
}

/// The sentence a clash at `key` earns: the key's own, or the table's.
pub(super) fn field_reason(table: &Table, key: &Key) -> Reason {
    key.reason.clone().unwrap_or_else(|| Reason::OptionField {
        form: table.form,
        key: key.label.to_string(),
    })
}

/// A value against what `key` of `table` holds it to, the failure worded by
/// the vocabulary it met: those nested in the value first, so a clash inside
/// a `dir` payload is the payload's and not the list's.
pub(super) fn unify_held(
    table: &'static Table,
    key: &'static Key,
) -> impl FnOnce(&mut Unifier, &Ty, &Ty) -> Result<(), Clash> {
    move |u, declared, found| match &key.reason {
        Some(Reason::Vocabulary(vocab)) => unify_vocab(u, vocab, declared, found),
        _ => u
            .unify_ty(declared, found)
            .map_err(|kind| Box::new((kind, field_reason(table, key)))),
    }
}

fn unify_vocab(
    u: &mut Unifier,
    vocab: &'static Vocab,
    declared: &Ty,
    found: &Ty,
) -> Result<(), Clash> {
    for &(tag, inner) in vocab.within {
        if let Some(payload) = payload_at(u, found, tag) {
            unify_vocab(u, inner, &inner.ty(), &payload)?;
        }
    }
    u.unify_ty(declared, found)
        .map_err(|kind| Box::new((kind, Reason::Vocabulary(vocab))))
}

/// The payload `tag` carries in the variant `ty` offers, under any lists.
fn payload_at(u: &Unifier, ty: &Ty, tag: &str) -> Option<Ty> {
    let resolved = u.resolve_ty(ty);
    match resolved.node() {
        TyNode::List(elem) => payload_at(u, elem, tag),
        TyNode::Variant(row) => {
            let mut rest = u.resolve_row(row);
            while let RowNode::Extend(label, payload, tail) = rest.node() {
                if *label == Label::Case(tag.to_string()) {
                    return Some(payload.clone());
                }
                rest = u.resolve_row(tail);
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::error::{OptionsContext, TypeErrorKind, Unwritten};
    use super::{Form, Holds, Key, Reason, Standing, Table, declared};
    use crate::syntax::parser::parse;
    use crate::test_helper::rc_source;
    use crate::{TypeError, elaborator::elaborate, typecheck};

    #[cfg(unix)]
    #[test]
    fn declared_limits_match_child_limits() {
        assert_eq!(
            super::LIMITS.offered(),
            crate::process::ChildLimit::ALL
                .iter()
                .map(|limit| limit.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rc_nested_setting_blanks_match_their_required_keys() {
        for key in declared(Form::Rc).keys {
            let Holds::Table(table) = &key.holds else {
                continue;
            };
            assert!(
                table
                    .keys
                    .iter()
                    .all(|key| key.required && key.blank == Some("`inherit"))
            );
            let fields = table
                .keys
                .iter()
                .map(|key| format!("{}: `inherit", key.label))
                .collect::<Vec<_>>()
                .join(", ");
            assert_eq!(key.blank, Some(format!("[{fields}]").as_str()));
        }
    }

    fn table_errors(form: Form, src: &str) -> Vec<TypeError> {
        let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
        let top = match elaborate(&ast, [], None, &mut crate::compile::no_modules) {
            Ok(top) => top,
            Err(crate::compile::CompileError::Types(errors)) => return errors,
            Err(e) => panic!("elaborate error in {src:?}: {e:?}"),
        };
        typecheck(
            &top,
            crate::test_helper::core_schemes(),
            crate::typecheck::Unit::Host(declared(form)),
        )
        .err()
        .unwrap_or_default()
    }

    /// A key's diagnosis sits at its written value.
    #[test]
    fn contract_diagnostics_sit_at_what_was_written() {
        let cases = [
            (
                Form::Rc,
                format!("let a = 1\n{}", rc_source(&[("bell", "7")])),
                "7",
            ),
            (
                Form::Manifest,
                "let a = 1\nreturn [name: 'p', nope: 7]".into(),
                "7",
            ),
            (
                Form::Grant,
                "let a = 1\nreturn [detach: true]".into(),
                "true",
            ),
            (Form::Grant, "let a = 1\nreturn [fs: [nope: 7]]".into(), "7"),
            (
                Form::Manifest,
                "let a = 1\nreturn [name: 'p', handlers: 1]".into(),
                "1",
            ),
        ];
        for (form, src, at) in cases {
            let [err] = &table_errors(form, &src)[..] else {
                panic!("{form:?} {src:?}: one diagnosis expected");
            };
            let pos = err.pos.expect("a located diagnosis");
            let want = src.rfind(at).unwrap();
            assert_eq!(pos.start as usize, want, "{form:?} {src:?}");
        }
    }
    fn checked(form: Form, src: &str) -> crate::ir::Toplevel {
        let ast = parse(src).expect("parse");
        let top = elaborate(&ast, [], None, &mut crate::compile::no_modules).expect("elaborate");
        typecheck(
            &top,
            crate::test_helper::core_schemes(),
            crate::typecheck::Unit::Host(declared(form)),
        )
        .unwrap_or_else(|errors| panic!("{src}: {errors:?}"))
    }

    #[test]
    fn handler_records_are_refused_with_the_map_spelling() {
        for (form, src) in [
            (
                Form::Rc,
                rc_source(&[("handlers", "[foo: { |a| echo hi }]")]),
            ),
            (
                Form::Manifest,
                "return [name: 'p', handlers: [foo: { |a| echo hi }]]".into(),
            ),
            (
                Form::Manifest,
                "return { |options| return [name: 'p', handlers: [foo: { |a| echo hi }]] }".into(),
            ),
        ] {
            let errors = table_errors(form, &src);
            let [error] = &errors[..] else {
                panic!("{errors:?}")
            };
            assert!(matches!(error.reason, Some(Reason::Handlers)));
            assert!(matches!(error.kind, TypeErrorKind::TyMismatch { .. }));
            let hint = error.hint().expect("handler map spelling");
            assert!(hint.contains("write `[:, foo: { |args| … }]`"), "{hint}");
            let pos = error
                .pos
                .expect("the refused handler map has a source location");
            assert!(src[pos.start as usize..pos.end as usize].contains("[foo:"));
        }
    }

    #[test]
    fn written_handler_maps_are_checked_arm_by_arm_in_rc_and_manifest() {
        for (form, src) in [
            (
                Form::Rc,
                rc_source(&[("handlers", "[:, foo: { |a| return 5 }]")]),
            ),
            (
                Form::Manifest,
                "return [name: 'p', handlers: [:, foo: { |a| return 5 }]]".into(),
            ),
            (
                Form::Manifest,
                "return { |options| return [name: 'p', handlers: [:, foo: { |a| return 5 }]] }"
                    .into(),
            ),
        ] {
            let errors = table_errors(form, &src);
            let [error] = &errors[..] else {
                panic!("{errors:?}")
            };
            assert!(
                matches!(&error.reason, Some(Reason::StandsIn(Standing::Command(name))) if name == "foo")
            );
            assert!(matches!(error.kind, TypeErrorKind::CompTyMismatch { .. }));
            let hint = error.hint().expect("handler command obligation");
            assert!(
                hint.contains("this one returns an Int: write it, `echo …`"),
                "{hint}"
            );
            let pos = error
                .pos
                .expect("the refused handler map has a source location");
            assert_eq!(
                &src[pos.start as usize..pos.end as usize],
                "{ |a| return 5 }"
            );
        }
    }

    #[test]
    fn written_host_handler_maps_keep_each_arms_obligation_and_duplicate_span() {
        for (form, source) in [
            (Form::Rc, rc_source(&[("handlers", "[:, first: { |args| return 1 }, second: { |args| return true }]")])),
            (Form::Manifest, "return [name: 'p', handlers: [:, first: { |args| return 1 }, second: { |args| return true }]]".into()),
        ] {
            let errors = table_errors(form, &source);
            assert_eq!(errors.len(), 2, "{source}: {errors:?}");
            for (error, name, arm) in [
                (&errors[0], "first", "{ |args| return 1 }"),
                (&errors[1], "second", "{ |args| return true }"),
            ] {
                assert!(matches!(&error.reason, Some(Reason::StandsIn(Standing::Command(found))) if found == name));
                let pos = error.pos.expect("arm");
                assert_eq!(&source[pos.start as usize..pos.end as usize], arm);
            }
        }
        for (form, source) in [
            (Form::Rc, rc_source(&[("handlers", "[:, tool: { |args| echo first }, tool: { |args| echo second }]")])),
            (Form::Manifest, "return [name: 'p', handlers: [:, tool: { |args| echo first }, tool: { |args| echo second }]]".into()),
        ] {
            let errors = table_errors(form, &source);
            let [error] = &errors[..] else { panic!("{errors:?}") };
            assert!(matches!(&error.kind, TypeErrorKind::DuplicateField { label } if label == "tool"));
            let pos = error.pos.expect("second arm");
            assert_eq!(&source[pos.start as usize..pos.end as usize], "{ |args| echo second }");
        }
    }

    #[test]
    fn contracts_reach_decoder_sites_before_freezing() {
        use crate::ir::ValClos;
        use crate::ir::{CompKind, Phrase};
        use crate::ty::Fixings;
        for (form, returned, accepted, use_text) in [
            (
                Form::Rc,
                rc_source(&[("umask", "$d")]),
                ValClos::variant("set", Some(ValClos::string("022"))),
                "$d",
            ),
            (
                Form::Rc,
                rc_source(&[("handlers", "[:, foo: { |a| return $d }]")]),
                ValClos::UNIT,
                "return $d",
            ),
            (
                Form::Manifest,
                "return { |options| return [name: $d] }".into(),
                ValClos::string("p"),
                "$d",
            ),
            (
                Form::Manifest,
                "return { |options| return [name: 'p', handlers: [:, foo: { |a| return $d }]] }"
                    .into(),
                ValClos::UNIT,
                "return $d",
            ),
        ] {
            let src = format!("let d = echo 5 | from-json; {returned}");
            let top = checked(form, &src);
            let Phrase::Define { comp, .. } = &top.phrases[0].item else {
                panic!("definition")
            };
            let CompKind::Pipeline { stages } = &comp.item else {
                panic!("pipeline")
            };
            let CompKind::Exec(exec) = &stages.last().unwrap().item else {
                panic!("decoder")
            };
            let site = exec.site.as_ref().expect("decoder admission");
            site.admit(&accepted, &Fixings::default())
                .unwrap_or_else(|error| panic!("{src}: {error}"));
            let mismatch = site
                .admit(&ValClos::int(5), &Fixings::default())
                .expect_err("the contract must constrain the decoder before execution");
            let witness = mismatch
                .witness()
                .expect("the returned use constrains the decoder");
            assert!(
                src[witness.start as usize..witness.end as usize].contains(use_text),
                "{src}: {witness:?}"
            );
        }
    }

    #[test]
    fn handler_maps_check_shapes_and_computed_members() {
        for returned in ["1", "[]", "[:, foo: 1]", "[$key: { |a| return 5 }]"] {
            let src = format!("let key = 'foo'; {}", rc_source(&[("handlers", returned)]));
            assert!(!table_errors(Form::Rc, &src).is_empty(), "{src}");
        }
        for returned in [
            "[:]",
            "[:, foo: { |a| echo ...$a }]",
            "[$key: { |a| echo ...$a }]",
        ] {
            checked(
                Form::Rc,
                &format!("let key = 'foo'; {}", rc_source(&[("handlers", returned)])),
            );
        }
        assert!(
            !table_errors(
                Form::Rc,
                &format!(
                    "let key = 'foo'; {}",
                    rc_source(&[("handlers", "[$key: { |a| upper $a[0] }]")])
                )
            )
            .is_empty()
        );
        checked(
            Form::Rc,
            &rc_source(&[("handlers", "[:, echo: { |a| echo ...$a }]")]),
        );
    }

    #[test]
    fn computed_handler_refusal_names_the_obligation_not_a_fabricated_head() {
        let errors = table_errors(
            Form::Rc,
            &format!(
                "let key = 'foo'; {}",
                rc_source(&[("handlers", "[$key: { |a| return 5 }]")])
            ),
        );
        let [error] = &errors[..] else {
            panic!("{errors:?}")
        };
        assert!(matches!(error.reason, Some(Reason::Handlers)));
        let hint = error.hint().expect("handler command obligation");
        assert!(hint.contains("a handler stands in for a command"), "{hint}");
        assert!(!hint.contains("<computed>"), "{hint}");
        assert!(!error.to_string().contains("<computed>"));
    }

    #[test]
    fn a_factory_tail_spread_is_not_written_options() {
        let src = "return { |options| let f = $options[f]; let [name: n, extra: e] = !$f; let out = !$f; return [...$out, name: 'p'] }";
        let errors = table_errors(Form::Manifest, src);
        let [error] = &errors[..] else {
            panic!("{errors:?}")
        };
        assert!(matches!(
            error.kind,
            TypeErrorKind::OptionsNotWritten {
                found: Unwritten::Spread,
                context: OptionsContext::FactoryBody,
                ..
            }
        ));
        let pos = error.pos.expect("the factory's tail");
        assert_eq!(
            &src[pos.start as usize..pos.end as usize],
            "[...$out, name: 'p']"
        );
    }

    #[test]
    fn decoded_data_is_rejected_as_handlers_before_a_later_field() {
        let src = format!(
            "let d = echo '\"x\"' | from-json; {}",
            rc_source(&[("handlers", "$d"), ("surface", "$d")])
        );
        let errors = table_errors(Form::Rc, &src);
        let [error] = &errors[..] else {
            panic!("{errors:?}")
        };
        let TypeErrorKind::KindMismatch { kind, witness, .. } = &error.kind else {
            panic!("{error:?}")
        };
        assert_eq!(*kind, crate::ty::Kind::Data);
        let witness = witness.expect("the decoder call imposes DATA");
        assert_eq!(
            &src[witness.start as usize..witness.end as usize],
            "from-json"
        );
        assert!(matches!(error.reason, Some(Reason::Handlers)));
        let pos = error.pos.expect("the handlers use");
        assert_eq!(pos.start as usize, src.find("$d").unwrap());
        assert_eq!(&src[pos.start as usize..pos.end as usize], "$d");
    }

    #[test]
    fn written_options_refuse_a_shared_payload_at_its_second_use() {
        use crate::ty::Ty;
        static TABLE: Table = Table {
            form: "test",
            factory: false,
            keys: &[
                Key {
                    label: "a",
                    holds: Holds::At(Ty::bool),
                    required: false,
                    blank: None,
                    reason: None,
                },
                Key {
                    label: "b",
                    holds: Holds::At(Ty::string),
                    required: false,
                    blank: None,
                    reason: None,
                },
            ],
        };
        let src = "let x = echo true | from-json; return [a: $x, b: $x]";
        let ast = parse(src).expect("parse");
        let top = elaborate(&ast, [], None, &mut crate::compile::no_modules).expect("elaborate");
        let errors = typecheck(
            &top,
            crate::test_helper::core_schemes(),
            crate::typecheck::Unit::Host(&TABLE),
        )
        .expect_err("the shared payload cannot be both Bool and String");
        let [error] = &errors[..] else {
            panic!("{errors:?}")
        };
        assert!(
            matches!(&error.kind, TypeErrorKind::TyMismatch { expected, actual }
            if *expected == Ty::string() && *actual == Ty::bool())
        );
        let pos = error.pos.expect("second use");
        assert_eq!(pos.start as usize, src.rfind("$x").unwrap());
        assert_eq!(&src[pos.start as usize..pos.end as usize], "$x");
    }
}
