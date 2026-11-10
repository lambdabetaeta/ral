//! The declared tables.
//!
//! A form's options and a contract file's return value are the same thing: a
//! closed set of labels, each held at what its table says.  `within` and
//! `grant` check theirs key by written key at the form, the bracket being
//! syntax (`Generator::check_options`); an rc file, a capability profile
//! and a plugin manifest are ascribed the table once inference has finished,
//! as ML ascribes a signature (`ascribe`).  A table may hold another
//! ([`Holds::Table`]), ascribed the same way from a queue `typecheck` drains
//! once inference has finished (`check_nested`).  The runtime doors —
//! `apply_rc_key`, `LoadedPlugin::parse`, `decode_capability_map` — each keep
//! their own `match` over the values; a table gives them only the *keyset*
//! and the unknown-key wording, so that much is written once.  A door's own
//! arm is per-key by nature and stays hand-written; what would let a table
//! and a door drift apart — a key declared but unhandled, refused at run
//! time by a message that lists it as offered — is caught instead by a test
//! at each door that walks its table asking whether every key reaches a
//! specific arm rather than falling to `unknown_key`:
//! `every_declared_rc_key_is_handled_by_apply_rc_key`,
//! `every_declared_manifest_key_is_handled_by_parse`, and
//! `every_declared_grant_key_is_handled_by_decode_capability_map`.

mod grant;

use std::fmt;

pub(crate) use grant::{DIR_TARGET, EDITOR, EXEC_TARGET, GPU, NET, PLACE, SHELL};

use super::error::{Clash, Reason, Standing, TypeError, TypeErrorKind};
use super::solve::{Solution, sort_errors};
use super::unify::Unifier;
use crate::ir::{CompKind, Phrase, Val};
use crate::source::{Span, Spanned};
pub(crate) use crate::text::code;
use crate::ty::{CompTy, Label, Row, TAG_PREFIX, Ty};

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
    /// Held at this ground type.
    At(Ty),
    /// Held at this ground type, and the row must have it.
    Required(Ty),
    /// The decoders': a fresh variable per occurrence, nothing imposed.
    Decoded,
    /// `within`'s catch-all handler, the one value inferred in context.
    CatchAll,
    /// A ground shape built per occurrence.
    Shaped(fn() -> Ty),
    /// A nested table, whose optional keys no row can express: ascribed once
    /// inference has finished, and never constraining.
    Table(&'static Table),
    /// Known, and refused, with advice of its own rather than "unknown key".
    Refused(&'static str),
}

/// What a written or inferred value is done with, by what its key holds it to.
pub(super) enum Held<'a> {
    /// Unified with this type, minted fresh.
    Unify(Ty),
    /// Ascribed once inference has finished.
    Nested(&'static Table),
    /// Refused with this advice.
    Refuse(&'a str),
    /// Left to the runtime decoder.
    Nothing,
}

impl Holds {
    /// What a value held here is done with.  Exhaustive, so a new way of
    /// holding a value cannot be ignored by the doors that apply it.
    pub(super) fn held(&self) -> Held<'_> {
        match self {
            Self::At(ty) | Self::Required(ty) => Held::Unify(ty.clone()),
            Self::CatchAll => Held::Unify(catch_all_ty()),
            Self::Shaped(mint) => Held::Unify(mint()),
            Self::Table(table) => Held::Nested(table),
            Self::Refused(advice) => Held::Refuse(advice),
            Self::Decoded => Held::Nothing,
        }
    }
}

/// The tags one closed variant row offers, as the words a clash with it uses.
///
/// The row itself is minted from `row`, so what a clash names and what the
/// checker admits are one list; the decoder words its refusals from the same
/// vocabulary.
pub struct Vocab {
    /// What a member is called: "place".
    pub noun: &'static str,
    /// The row's tags with their payloads, in the order a refusal lists them.
    pub row: fn() -> Vec<(String, Ty)>,
    /// Advice for a tag refused on purpose.
    pub misses: fn(&str) -> Option<String>,
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
            .field("noun", &self.noun)
            .field("row", &(self.row)())
            .field("untagged", &self.untagged)
            .field("payload", &self.payload)
            .field("within", &self.within)
            // Callback identity is not vocabulary content.
            .finish_non_exhaustive()
    }
}

impl Vocab {
    /// The closed variant this vocabulary is.
    pub(crate) fn ty(&self) -> Ty {
        Ty::Variant(
            (self.row)()
                .into_iter()
                .rev()
                .fold(Row::Empty, |rest, (tag, ty)| {
                    Row::Extend(Label::Case(tag), Box::new(ty), Box::new(rest))
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
        (self.misses)(tag)
    }

    /// The headline for `tag`, which the row lacks, and the advice kept for
    /// it: one refusal for the checker and the decoder alike.  Advice replaces
    /// the nearest-tag guess, which would contradict it.
    pub fn refuse(&self, tag: &str) -> (String, Option<String>) {
        let advice = self.miss(tag);
        let headline =
            super::explain::unknown_tag_message(self.noun, tag, &self.tags(), advice.is_none());
        (headline, advice)
    }
}

/// A nested table's value, waiting for inference to finish.
pub(super) struct NestedCheck {
    pub key: &'static str,
    pub table: &'static Table,
    pub ty: Ty,
    pub pos: Option<Span>,
}

/// One label a table names.
pub struct Key {
    pub label: &'static str,
    pub holds: Holds,
    /// The sentence a clash here earns; the form's own when `None`.
    pub reason: Option<Reason>,
}

/// A declared table: the form that owns it, for blame, and its closed keyset.
pub struct Table {
    pub form: &'static str,
    pub keys: &'static [Key],
    /// Whether a file may return a block of its options making the settings,
    /// instead of the settings themselves.
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

    /// What a runtime door says about a key this table does not name — the
    /// one wording all three doors share, each prefixing its own context.
    pub fn unknown_key(&self, key: &str) -> String {
        unknown_key_message(self.form, key, &self.offered())
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

/// `within`'s options.  `env:` is heterogeneous and `parse_env`'s to judge, so
/// it stays the decoder's.
///
/// The catch-all stands in for every command, so it is a function of the name
/// and the argv that is a command.
static WITHIN: Table = Table {
    form: "within",
    keys: &[
        Key {
            label: "dir",
            holds: Holds::At(Ty::String),
            reason: None,
        },
        Key {
            label: "env",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "handler",
            holds: Holds::CatchAll,
            reason: Some(Reason::StandsIn(Standing::EveryCommand)),
        },
    ],
    factory: false,
};

/// The rc file's thirteen keys.  `apply_rc_key` dispatches off this table and
/// applies what each one means; the shapes left to it — a map of hooks, a map
/// of aliases, a theme — are one type per key rather than one across the key,
/// and no single `Ty` pins them.
static RC: Table = Table {
    form: "rc",
    keys: &[
        Key {
            label: "umask",
            holds: Holds::At(Ty::String),
            reason: None,
        },
        Key {
            label: "limits",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "env",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "prompt",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "aliases",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "bindings",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "edit-mode",
            holds: Holds::At(Ty::String),
            reason: None,
        },
        Key {
            label: "bell",
            holds: Holds::At(Ty::Bool),
            reason: None,
        },
        Key {
            label: "surface",
            holds: Holds::At(Ty::String),
            reason: None,
        },
        Key {
            label: "recursion-limit",
            holds: Holds::At(Ty::Int),
            reason: None,
        },
        Key {
            label: "plugins",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "startup",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "theme",
            holds: Holds::Decoded,
            reason: None,
        },
    ],
    factory: false,
};

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
            holds: Holds::Required(Ty::String),
            reason: None,
        },
        Key {
            label: "aliases",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "hooks",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "keybindings",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "capabilities",
            holds: Holds::Refused(
                "manifest 'capabilities:' is not enforced: plugins run with host \
                 authority. Remove the key; to confine a plugin invocation, wrap it \
                 in `grant { ... }`.",
            ),
            reason: None,
        },
    ],
    factory: true,
};

/// The catch-all's shape around what it produces: the name, then the argv.
pub(super) fn catch_all_shape(body: CompTy) -> CompTy {
    CompTy::arrows([Ty::String, Ty::argv()], body)
}

/// What the catch-all `handler:` is: `String → [String] → Command`.
fn catch_all_ty() -> Ty {
    Ty::Thunk(Box::new(catch_all_shape(CompTy::command())))
}

/// The table `form` declares.
pub fn declared(form: Form) -> &'static Table {
    match form {
        Form::Within => &WITHIN,
        Form::Grant => &grant::GRANT,
        Form::Rc => &RC,
        Form::Manifest => &MANIFEST,
    }
}

/// What a runtime door says about a key a table does not name.
pub(super) fn unknown_key_message(form: &str, key: &str, offered: &[&str]) -> String {
    let near = match crate::text::near_names(key, offered.iter().copied(), 1)[..] {
        [near] => format!(" (did you mean '{near}'?)"),
        _ => String::new(),
    };
    format!(
        "unknown key '{key}': `{form}` takes {list}{near}",
        list = offered.join(", "),
    )
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
    match u.resolve_ty(ty) {
        Ty::List(elem) => payload_at(u, &elem, tag),
        Ty::Variant(row) => {
            let mut rest = u.resolve_row(&row);
            while let Row::Extend(label, payload, tail) = rest {
                if label == Label::Case(tag.to_string()) {
                    return Some(*payload);
                }
                rest = u.resolve_row(&tail);
            }
            None
        }
        _ => None,
    }
}

/// `label`'s value, when the options were written out.
fn written_field<'a>(opts: &'a Val, label: &str) -> Option<&'a Spanned<Val>> {
    let Val::Record(entries) = opts else {
        return None;
    };
    entries
        .shape()
        .iter()
        .find(|(key, _)| key.as_ref() == label)
        .map(|(_, value)| value)
}

/// Where `label`'s value sits, when the options were written out.
pub(super) fn written_at(opts: &Val, label: &str) -> Option<Span> {
    written_field(opts, label).and_then(|value| value.span)
}

/// Hold a program's own return value to `table`, once inference has finished.
///
/// The *inferred* type is what is ascribed, whatever syntax produced it, so a
/// key misspelled inside a spread is caught as one written out is.  Nothing
/// constrains the program: the check runs in a scratch copy of the unifier, on
/// a finished result that flows nowhere else, so the row's open tail is read
/// as closed.  A return typed at a variable (`from-json`) stays on the runtime
/// door; where the table admits one, a factory is held to a block of the
/// options, and what it makes is ascribed as a direct return is.
///
/// # Errors
/// Every refusal, sorted by position.
pub(super) fn ascribe(
    solution: &Solution,
    tail: Option<&Spanned<Phrase<()>>>,
    cty: Option<CompTy>,
    table: &'static Table,
) -> Result<(), Vec<TypeError>> {
    let resolved = match cty.map(|cty| solution.resolve_comp(&cty)) {
        Some(CompTy::Return(_, value)) => solution.resolve_ty(&value),
        Some(_) => return Ok(()),
        None => Ty::Unit,
    };
    let written = match tail.map(|phrase| &phrase.item) {
        Some(Phrase::Run(comp)) if let CompKind::Return(record @ Val::Record(_)) = &comp.item => {
            Some(record)
        }
        _ => None,
    };
    let mut check = Ascription {
        pos: tail.and_then(|phrase| phrase.span),
        errors: Vec::new(),
    };
    check.value(solution.scratch(), resolved, table, written, table.factory);
    if check.errors.is_empty() {
        return Ok(());
    }
    sort_errors(&mut check.errors);
    Err(check.errors)
}

/// What a factory `body` makes, once it is held to a block of the options.
fn made_by(scratch: &mut Unifier, body: &CompTy, at: Option<Span>) -> Result<Ty, TypeErrorKind> {
    scratch.at = at;
    let made = scratch.fresh_ty();
    let factory = CompTy::arrows(
        [scratch.fresh_ty()],
        CompTy::Return(scratch.fresh_grade(), Box::new(made.clone())),
    );
    scratch.unify_comp_ty(&factory, body)?;
    Ok(scratch.resolve_ty(&made))
}

struct Ascription {
    pos: Option<Span>,
    errors: Vec<TypeError>,
}

impl Ascription {
    fn report(&mut self, kind: TypeErrorKind, reason: Option<Reason>) {
        self.errors.push(TypeError {
            pos: self.pos,
            kind,
            reason,
            weak: None,
            unit: None,
        });
    }

    fn value(
        &mut self,
        mut scratch: Unifier,
        value: Ty,
        table: &'static Table,
        written: Option<&Val>,
        factory: bool,
    ) {
        match value {
            Ty::Record(row) => self.row(scratch, &row, table, written),
            Ty::Unit => self.row(scratch, &Row::Empty, table, written),
            Ty::Var(_) => {}
            Ty::Thunk(body) if factory => match made_by(&mut scratch, &body, self.pos) {
                Ok(made) => self.value(scratch, made, table, None, false),
                Err(kind) => self.report(kind, Some(Reason::Factory)),
            },
            found => self.report(
                TypeErrorKind::ReturnNotRecord {
                    form: table.form,
                    found,
                    offered: table.offered(),
                },
                None,
            ),
        }
    }

    /// One scratch holds every field of the row.
    fn row(
        &mut self,
        mut scratch: Unifier,
        row: &Row,
        table: &'static Table,
        written: Option<&Val>,
    ) {
        let phrase_pos = self.pos;
        let mut present = Vec::new();
        let mut rest = scratch.resolve_row(row);
        while let Row::Extend(label, payload, tail) = rest {
            let Label::Field(label) = label else {
                rest = scratch.resolve_row(&tail);
                continue;
            };
            present.push(label.clone());
            self.pos = written.and_then(|w| written_at(w, &label)).or(phrase_pos);
            if let Some(key) = table.keys.iter().find(|k| k.label == label) {
                match key.holds.held() {
                    Held::Unify(declared) => {
                        scratch.at = self.pos;
                        if let Err(clash) =
                            unify_held(table, key)(&mut scratch, &declared, &payload)
                        {
                            self.report(clash.0, Some(clash.1));
                        }
                    }
                    Held::Nested(sub) => {
                        let inner = written.and_then(|w| written_field(w, &label));
                        self.nested(&scratch, key.label, &payload, sub, inner.map(|v| &v.item));
                    }
                    Held::Refuse(advice) => self.report(
                        TypeErrorKind::RefusedKey {
                            form: table.form,
                            key: key.label,
                            advice,
                        },
                        None,
                    ),
                    Held::Nothing => {}
                }
            } else {
                self.report(
                    TypeErrorKind::UnknownKey {
                        form: table.form,
                        key: label,
                        offered: table.offered(),
                    },
                    None,
                );
            }
            rest = scratch.resolve_row(&tail);
        }
        self.pos = phrase_pos;
        for key in table.keys {
            if matches!(key.holds, Holds::Required(_)) && !present.iter().any(|l| l == key.label) {
                self.report(
                    TypeErrorKind::RowMissingField {
                        label: key.label.to_string(),
                    },
                    None,
                );
            }
        }
    }

    fn nested(
        &mut self,
        store: &Unifier,
        key: &'static str,
        ty: &Ty,
        table: &'static Table,
        written: Option<&Val>,
    ) {
        match store.resolve_ty(ty) {
            Ty::Record(row) => self.row(store.clone(), &row, table, written),
            Ty::Var(_) => {}
            found => self.report(
                TypeErrorKind::NotATable {
                    key,
                    first: table.keys.first().map_or("", |k| k.label),
                    found,
                },
                None,
            ),
        }
    }
}

/// Queued tables are checked during closure, before any successful export.
pub(super) fn check_nested(store: &Unifier, nested: Vec<NestedCheck>) -> Vec<TypeError> {
    let mut check = Ascription {
        pos: None,
        errors: Vec::new(),
    };
    for NestedCheck {
        key,
        table,
        ty,
        pos,
    } in nested
    {
        check.pos = pos;
        check.nested(store, key, &ty, table, None);
    }
    check.errors
}

#[cfg(test)]
mod tests {
    use super::{Form, Holds, declared};
    use crate::syntax::parser::parse;
    use crate::{TypeError, elaborator::elaborate, typecheck};

    #[test]
    fn vocab_debug_is_stable_and_semantic() {
        assert_eq!(
            format!("{:?}", super::GPU),
            "Vocab { noun: \"GPU mode\", row: [(\"withheld\", Unit)], untagged: \"a grant only narrows: omit `gpu` to inherit, or write `` gpu: `withheld ``\", payload: \"`` `withheld `` takes no payload\", within: [], .. }"
        );
        for vocab in [
            &super::PLACE,
            &super::EXEC_TARGET,
            &super::DIR_TARGET,
            &super::NET,
            &super::GPU,
            &super::EDITOR,
            &super::SHELL,
        ] {
            let debug = format!("{vocab:#?}");
            assert!(debug.contains(vocab.noun));
            assert!(debug.contains("row:"));
            assert!(!debug.contains("0x"), "{debug}");
            assert!(!debug.contains("misses:"), "{debug}");
        }
        let nested = format!("{:#?}", super::EXEC_TARGET);
        assert!(nested.contains("place or a selector"));
        assert!(nested.contains("String"));
        assert!(nested.contains("Variant"));
    }

    fn contract_errors(form: Form, src: &str) -> Vec<TypeError> {
        let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
        let top = elaborate(&ast, [], None)
            .unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
        typecheck(
            &top,
            crate::test_helper::core_schemes(),
            Some(declared(form)),
        )
        .err()
        .unwrap_or_default()
    }

    /// A diagnosis sits at the key's written value; one about the file as a
    /// whole at the returning phrase, not at the file's start.
    #[test]
    fn contract_diagnostics_sit_at_what_was_written() {
        let cases = [
            (Form::Rc, "let a = 1\nreturn [bell: 7]", "7"),
            (Form::Rc, "let a = 1\nreturn [nope: 7]", "7"),
            (Form::Grant, "let a = 1\nreturn [detach: true]", "true"),
            (Form::Grant, "let a = 1\nreturn [fs: [nope: 7]]", "7"),
            (Form::Manifest, "let a = 1\nreturn [aliases: 1]", "return"),
        ];
        for (form, src, at) in cases {
            let [err] = &contract_errors(form, src)[..] else {
                panic!("{form:?} {src:?}: one diagnosis expected");
            };
            let pos = err.pos.expect("a located diagnosis");
            let want = src.rfind(at).unwrap();
            assert_eq!(pos.start as usize, want, "{form:?} {src:?}");
        }
    }

    /// A table offers every key but the refused ones, which it knows and
    /// declines.
    #[test]
    fn a_table_offers_every_key_but_the_refused() {
        for form in [Form::Within, Form::Grant, Form::Rc, Form::Manifest] {
            let table = declared(form);
            let offered = table.offered();
            for key in table.keys {
                assert_eq!(
                    offered.contains(&key.label),
                    !matches!(key.holds, Holds::Refused(_)),
                    "{form:?}: {}",
                    key.label
                );
            }
        }
        let manifest = declared(Form::Manifest);
        assert!(manifest.offered().len() < manifest.keys.len());
    }
    #[test]
    fn ascription_uses_one_scratch_for_shared_row_payloads_without_constraining_the_solution() {
        use crate::ty::{CompTy, Label, Row, Ty};
        use crate::typecheck::solve::Solver;
        static TABLE: super::Table = super::Table {
            form: "test",
            factory: false,
            keys: &[
                super::Key {
                    label: "a",
                    holds: Holds::At(Ty::Bool),
                    reason: None,
                },
                super::Key {
                    label: "b",
                    holds: Holds::At(Ty::String),
                    reason: None,
                },
            ],
        };
        let mut solver = Solver::new();
        let payload = solver.fresh_ty();
        let row = ["a", "b"]
            .into_iter()
            .rev()
            .fold(Row::Empty, |tail, label| {
                Row::Extend(
                    Label::Field(label.into()),
                    Box::new(payload.clone()),
                    Box::new(tail),
                )
            });
        let Ok(solution) = solver.close() else {
            panic!("an unconstrained unit closes");
        };
        let errors = super::ascribe(&solution, None, Some(CompTy::pure(Ty::Record(row))), &TABLE)
            .expect_err("the shared payload is refused");
        assert_eq!(
            errors.len(),
            1,
            "the shared payload cannot be both Bool and String"
        );
        assert!(matches!(
            errors[0].kind,
            super::TypeErrorKind::TyMismatch { .. }
        ));
        assert_eq!(solution.resolve_ty(&payload), payload);
    }
}
