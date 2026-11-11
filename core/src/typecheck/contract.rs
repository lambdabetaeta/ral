//! The declared tables.
//!
//! A form's options and a contract file's return value are the same thing: a
//! closed set of labels, each held at what its table says.  `within` and
//! `grant` check theirs key by written key at the form, the bracket being
//! syntax (`Generator::check_options`); an rc file, a capability profile
//! and a plugin manifest use their return value at the declared table after
//! defaults, before closure freezes sites (`ascribe`).  A table may hold another
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

use super::builtins::{Arm, handlers_ty};
use super::error::{Clash, Reason, Standing, TypeError, TypeErrorKind};
use super::solve::Settled;
use super::unify::Unifier;
use crate::fact::{
    GrantFailure, GrantFailureUnknownVocabulary, GrantVocabulary, RefusedGrantKey, UnknownField,
};
use crate::ir::{CompKind, Phrase, Val};
use crate::source::{Span, Spanned, WithSpan};
pub(crate) use crate::text::code;
use crate::ty::{CompTy, CompTyNode, Label, Row, RowNode, TAG_PREFIX, Ty, TyNode};

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
    /// The same, and the row must have it.
    Required(fn() -> Ty),
    /// The decoders': a fresh variable per occurrence, nothing imposed.
    Decoded,
    /// The handler map.
    Handlers,
    /// `within`'s catch-all handler, the one value inferred in context.
    CatchAll,
    /// A nested table, whose optional keys no row can express: ascribed once
    /// inference has finished, and never constraining.
    Table(&'static Table),
    /// Known, and refused, with advice of its own rather than "unknown key".
    Refused(RefusedGrantKey),
}

/// What a written or inferred value is done with, by what its key holds it to.
pub(super) enum Held {
    /// Unified with this type, minted fresh.
    Unify(Ty),
    /// Checked as a handler in its own inference region.
    Arm(Arm<'static>),
    /// Ascribed once inference has finished.
    Nested(&'static Table),
    /// Refused with this advice.
    Refuse(&'static str),
    /// Left to the runtime decoder.
    Nothing,
}

impl Holds {
    /// What a value held here is done with.  Exhaustive, so a new way of
    /// holding a value cannot be ignored by the doors that apply it.
    pub(super) fn held(&self) -> Held {
        match self {
            Self::At(mint) | Self::Required(mint) => Held::Unify(mint()),
            Self::CatchAll => Held::Arm(Arm::CatchAll),
            Self::Table(table) => Held::Nested(table),
            Self::Refused(cause) => Held::Refuse(cause.advice()),
            Self::Decoded => Held::Nothing,
            Self::Handlers => Held::Unify(handlers_ty()),
        }
    }
}

/// The tags one closed variant row offers, as the words a clash with it uses.
///
/// The row itself is minted from `row`, so what a clash names and what the
/// checker admits are one list; the decoder words its refusals from the same
/// vocabulary.
pub struct Vocab {
    pub id: GrantVocabulary,
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

/// A nested table's value, waiting for inference to finish.
pub(super) enum NestedCheck {
    Table {
        key: &'static str,
        table: &'static Table,
        ty: Ty,
        pos: Option<Span>,
        written: Option<Val>,
    },
    Walked {
        row: Row,
        table: &'static Table,
        seen: Vec<String>,
        written: Option<Val>,
        pos: Option<Span>,
    },
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
        Key {
            label: "handlers",
            holds: Holds::Handlers,
            reason: Some(Reason::Handlers),
        },
    ],
    factory: false,
};

/// The rc file's thirteen keys.  `apply_rc_key` dispatches off this table and
/// applies what each one means.  `handlers:` is a map; the shapes left to the
/// decoder — hooks, a theme — have no single `Ty` that pins them.
static RC: Table = Table {
    form: "rc",
    keys: &[
        Key {
            label: "umask",
            holds: Holds::At(Ty::string),
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
            label: "handlers",
            holds: Holds::Handlers,
            reason: Some(Reason::Handlers),
        },
        Key {
            label: "bindings",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "edit-mode",
            holds: Holds::At(Ty::string),
            reason: None,
        },
        Key {
            label: "bell",
            holds: Holds::At(Ty::bool),
            reason: None,
        },
        Key {
            label: "surface",
            holds: Holds::At(Ty::string),
            reason: None,
        },
        Key {
            label: "recursion-limit",
            holds: Holds::At(Ty::int),
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
            holds: Holds::Required(Ty::string),
            reason: None,
        },
        Key {
            label: "handlers",
            holds: Holds::Handlers,
            reason: Some(Reason::Handlers),
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
            holds: Holds::Refused(RefusedGrantKey::ManifestCapabilitiesUnenforced),
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

/// The contract uses the return value after elaboration decisions are fixed,
/// but before sites and schemes are frozen.
pub(super) fn ascribe(
    settled: &mut Settled,
    tail: Option<&Spanned<Phrase<()>>>,
    cty: Option<CompTy>,
    table: &'static Table,
) {
    let resolved = cty.map(|cty| settled.resolve_comp(&cty));
    let value = match resolved.as_ref().map(CompTy::node) {
        Some(CompTyNode::Return(_, value)) => value.clone(),
        Some(_) => return,
        None => Ty::unit(),
    };
    let written = match tail.map(|phrase| &phrase.item) {
        Some(Phrase::Run(comp)) if let CompKind::Return(record @ Val::Record(_)) = &comp.item => {
            Some(record)
        }
        _ => None,
    };
    settled.with_span(tail.and_then(|phrase| phrase.span), |settled| {
        hold_value(settled, &value, table, written, table.factory);
    });
}

fn hold_value(
    settled: &mut Settled,
    value: &Ty,
    table: &'static Table,
    written: Option<&Val>,
    factory: bool,
) {
    let found = settled.resolve_ty(value);
    match found.node() {
        TyNode::Record(row) => hold_row(settled, row, table, written),
        TyNode::Unit => hold_row(settled, &Row::empty(), table, written),
        TyNode::Var(_) => {}
        TyNode::Thunk(body) if factory => {
            let made = settled.fresh_ty();
            let options = settled.fresh_ty();
            let grade = settled.fresh_grade();
            let shape = CompTy::arrows([options], CompTy::ret(grade, made.clone()));
            if settled.equate_comp(&shape, body, Reason::Factory) {
                hold_value(settled, &made, table, None, false);
            }
        }
        _ => settled.diagnose(TypeErrorKind::ReturnNotRecord {
            form: table.form,
            found,
            offered: table.offered(),
        }),
    }
}

/// Re-read the live tail after each field; closure judges the final keyset
/// and only newly revealed fields on scratch.
fn hold_row(settled: &mut Settled, row: &Row, table: &'static Table, written: Option<&Val>) {
    let pos = *settled.span_slot();
    let mut seen = Vec::new();
    let mut rest = settled.resolve_row(row);
    while let RowNode::Extend(label, payload, tail) = rest.node() {
        if let Label::Field(label) = label {
            let at = written.and_then(|w| written_at(w, label)).or(pos);
            settled.with_span(at, |settled| {
                if let Some(key) = table.keys.iter().find(|k| k.label == label) {
                    match key.holds.held() {
                        Held::Unify(declared) => {
                            settled.equate_held(&declared, payload, table, key);
                        }
                        Held::Arm(arm) => {
                            settled.equate_held(&Ty::thunk(arm.ty()), payload, table, key);
                        }
                        Held::Nested(table) => settled.nested(NestedCheck::Table {
                            key: key.label,
                            table,
                            ty: payload.clone(),
                            pos: at,
                            written: written
                                .and_then(|w| written_field(w, label))
                                .map(|v| v.item.clone()),
                        }),
                        Held::Refuse(_) | Held::Nothing => {}
                    }
                }
            });
            seen.push(label.clone());
        }
        rest = settled.resolve_row(tail);
    }
    settled.nested(NestedCheck::Walked {
        row: row.clone(),
        table,
        seen,
        written: written.cloned(),
        pos,
    });
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

    /// One scratch holds every field of the row.
    fn row(
        &mut self,
        mut scratch: Unifier,
        row: &Row,
        table: &'static Table,
        written: Option<&Val>,
    ) {
        self.row_unseen(&mut scratch, row, table, written, &[]);
    }

    fn row_unseen(
        &mut self,
        scratch: &mut Unifier,
        row: &Row,
        table: &'static Table,
        written: Option<&Val>,
        seen: &[String],
    ) {
        let phrase_pos = self.pos;
        let mut present = Vec::new();
        let mut rest = scratch.resolve_row(row);
        while let RowNode::Extend(label, payload, tail) = rest.node() {
            let Label::Field(label) = label else {
                rest = scratch.resolve_row(tail);
                continue;
            };
            present.push(label.clone());
            self.pos = written.and_then(|w| written_at(w, label)).or(phrase_pos);
            if let Some(key) = table.keys.iter().find(|k| k.label == label) {
                match key.holds.held() {
                    Held::Unify(_) | Held::Arm(_) | Held::Nested(_) if seen.contains(label) => {}
                    Held::Unify(declared) => {
                        scratch.at = self.pos;
                        if let Err(clash) = unify_held(table, key)(scratch, &declared, payload) {
                            self.report(clash.0, Some(clash.1));
                        }
                    }
                    Held::Arm(arm) => {
                        scratch.at = self.pos;
                        if let Err(clash) =
                            unify_held(table, key)(scratch, &Ty::thunk(arm.ty()), payload)
                        {
                            self.report(clash.0, Some(clash.1));
                        }
                    }
                    Held::Nested(sub) => {
                        let inner = written.and_then(|w| written_field(w, label));
                        self.nested(scratch, key.label, payload, sub, inner.map(|v| &v.item));
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
                        key: label.clone(),
                        offered: table.offered(),
                    },
                    None,
                );
            }
            rest = scratch.resolve_row(tail);
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
        let found = store.resolve_ty(ty);
        match found.node() {
            TyNode::Record(row) => self.row(store.clone(), row, table, written),
            TyNode::Var(_) => {}
            _ => self.report(
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
    for nested in nested {
        match nested {
            NestedCheck::Table {
                key,
                table,
                ty,
                pos,
                written,
            } => {
                check.pos = pos;
                check.nested(store, key, &ty, table, written.as_ref());
            }
            NestedCheck::Walked {
                row,
                table,
                seen,
                written,
                pos,
            } => {
                check.pos = pos;
                check.row_unseen(&mut store.clone(), &row, table, written.as_ref(), &seen);
            }
        }
    }
    check.errors
}

#[cfg(test)]
mod tests {
    use super::{Form, Holds, declared};
    use crate::syntax::parser::parse;
    use crate::{TypeError, elaborator::elaborate, typecheck};

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
            (
                Form::Manifest,
                "let a = 1\nreturn [name: 'p', handlers: 1]",
                "1]",
            ),
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
    fn checked(form: Form, src: &str) -> crate::ir::Toplevel {
        let ast = parse(src).expect("parse");
        let top = elaborate(&ast, [], None).expect("elaborate");
        typecheck(
            &top,
            crate::test_helper::core_schemes(),
            Some(declared(form)),
        )
        .unwrap_or_else(|errors| panic!("{src}: {errors:?}"))
    }

    #[test]
    fn handler_records_are_refused_with_the_map_spelling() {
        for (form, src) in [
            (Form::Rc, "return [handlers: [foo: { |a| echo hi }]]"),
            (
                Form::Manifest,
                "return [name: 'p', handlers: [foo: { |a| echo hi }]]",
            ),
            (
                Form::Manifest,
                "return { |options| return [name: 'p', handlers: [foo: { |a| echo hi }]] }",
            ),
        ] {
            let errors = contract_errors(form, src);
            let [error] = &errors[..] else {
                panic!("{errors:?}")
            };
            assert!(matches!(error.reason, Some(super::Reason::Handlers)));
            assert!(matches!(
                error.kind,
                super::TypeErrorKind::TyMismatch { .. }
            ));
            let hint = error.hint().expect("handler map spelling");
            assert!(hint.contains("write `[:, foo: { |args| … }]`"), "{hint}");
            let pos = error
                .pos
                .expect("the refused handler map has a source location");
            assert!(src[pos.start as usize..pos.end as usize].contains("[foo:"));
        }
    }

    #[test]
    fn handler_maps_are_held_by_one_equation_in_rc_and_manifest() {
        for (form, src) in [
            (Form::Rc, "return [handlers: [:, foo: { |a| return 5 }]]"),
            (
                Form::Manifest,
                "return [name: 'p', handlers: [:, foo: { |a| return 5 }]]",
            ),
            (
                Form::Manifest,
                "return { |options| return [name: 'p', handlers: [:, foo: { |a| return 5 }]] }",
            ),
        ] {
            let errors = contract_errors(form, src);
            let [error] = &errors[..] else {
                panic!("{errors:?}")
            };
            assert!(matches!(error.reason, Some(super::Reason::Handlers)));
            assert!(matches!(
                error.kind,
                super::TypeErrorKind::CompTyMismatch { .. }
            ));
            let hint = error.hint().expect("handler command obligation");
            assert!(
                hint.contains("this one returns an Int: write it, `echo …`"),
                "{hint}"
            );
            let pos = error
                .pos
                .expect("the refused handler map has a source location");
            assert!(src[pos.start as usize..pos.end as usize].contains("[:, foo:"));
        }
    }

    #[test]
    fn contracts_reach_decoder_sites_before_freezing() {
        use crate::ir::{CompKind, Phrase};
        use crate::shell::runtime::Value;
        use crate::ty::Fixings;
        for (form, returned, accepted, use_text) in [
            (Form::Rc, "[umask: $d]", Value::string("022"), "$d"),
            (
                Form::Rc,
                "[handlers: [:, foo: { |a| return $d }]]",
                Value::Unit,
                "return $d",
            ),
            (
                Form::Manifest,
                "{ |options| return [name: $d] }",
                Value::string("p"),
                "name: $d",
            ),
            (
                Form::Manifest,
                "{ |options| return [name: 'p', handlers: [:, foo: { |a| return $d }]] }",
                Value::Unit,
                "return $d",
            ),
        ] {
            let src = format!("let d = echo 5 | from-json; return {returned}");
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
                .admit(&Value::Int(5), &Fixings::default())
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
            let src = format!("let key = 'foo'; return [handlers: {returned}]");
            assert!(!contract_errors(Form::Rc, &src).is_empty(), "{src}");
        }
        for returned in [
            "[:]",
            "[:, foo: { |a| echo ...$a }]",
            "[$key: { |a| echo ...$a }]",
        ] {
            checked(
                Form::Rc,
                &format!("let key = 'foo'; return [handlers: {returned}]"),
            );
        }
        assert!(
            !contract_errors(
                Form::Rc,
                "let key = 'foo'; return [handlers: [$key: { |a| upper $a[0] }]]"
            )
            .is_empty()
        );
        checked(Form::Rc, "return [handlers: [:, echo: { |a| echo ...$a }]]");
    }

    #[test]
    fn computed_handler_refusal_names_the_obligation_not_a_fabricated_head() {
        let errors = contract_errors(
            Form::Rc,
            "let key = 'foo'; return [handlers: [$key: { |a| return 5 }]]",
        );
        let [error] = &errors[..] else {
            panic!("{errors:?}")
        };
        assert!(matches!(error.reason, Some(super::Reason::Handlers)));
        let hint = error.hint().expect("handler command obligation");
        assert!(hint.contains("a handler stands in for a command"), "{hint}");
        assert!(!hint.contains("<computed>"), "{hint}");
        assert!(!error.to_string().contains("<computed>"));
    }

    #[test]
    fn settled_factory_walk_sees_defaulted_extra_fields() {
        let src = "return { |options| let f = $options[f]; let [name: n, extra: e] = !$f; let out = !$f; return [...$out, name: 'p'] }";
        let errors = contract_errors(Form::Manifest, src);
        assert!(errors.iter().any(|error| matches!(&error.kind, super::TypeErrorKind::UnknownKey { key, .. } if key == "extra")), "{errors:?}");
    }

    #[test]
    fn handler_map_constrains_an_unresolved_value_before_a_later_field() {
        let src = "let d = echo '\"x\"' | from-json; return [handlers: $d, umask: $d]";
        let errors = contract_errors(Form::Rc, src);
        let [error] = &errors[..] else {
            panic!("{errors:?}")
        };
        assert!(
            matches!(&error.kind, super::TypeErrorKind::TyMismatch { expected, actual }
                if *expected == crate::ty::Ty::string() && *actual == super::handlers_ty())
        );
        assert!(
            matches!(&error.reason, Some(super::Reason::OptionField { key, .. }) if key == "umask")
        );
        assert_eq!(error.pos.unwrap().start as usize, src.rfind("$d").unwrap());
    }

    #[test]
    fn nested_tables_still_judge_on_scratch_without_solving_payloads() {
        use crate::ty::{Label, Row, Ty};
        use crate::typecheck::solve::Solver;
        static TABLE: super::Table = super::Table {
            form: "nested",
            factory: false,
            keys: &[super::Key {
                label: "flag",
                holds: Holds::At(Ty::bool),
                reason: None,
            }],
        };
        let mut solver = Solver::new();
        let payload = solver.fresh_ty();
        let ty = Ty::record(Row::extend(
            Label::Field("flag".into()),
            payload.clone(),
            Row::empty(),
        ));
        solver.nested(super::NestedCheck::Table {
            key: "data",
            table: &TABLE,
            ty,
            pos: None,
            written: None,
        });
        let solution = solver.settle().close().expect("scratch holds flag at Bool");
        assert_eq!(solution.resolve_ty(&payload), payload);
    }

    #[test]
    fn live_ascription_refuses_shared_row_payloads() {
        use crate::ty::{CompTy, Label, Row, Ty};
        use crate::typecheck::solve::Solver;
        static TABLE: super::Table = super::Table {
            form: "test",
            factory: false,
            keys: &[
                super::Key {
                    label: "a",
                    holds: Holds::At(Ty::bool),
                    reason: None,
                },
                super::Key {
                    label: "b",
                    holds: Holds::At(Ty::string),
                    reason: None,
                },
            ],
        };
        let mut solver = Solver::new();
        let payload = solver.fresh_ty();
        let row = ["a", "b"]
            .into_iter()
            .rev()
            .fold(Row::empty(), |tail, label| {
                Row::extend(Label::Field(label.into()), payload.clone(), tail)
            });
        let mut settled = solver.settle();
        super::ascribe(
            &mut settled,
            None,
            Some(CompTy::pure(Ty::record(row))),
            &TABLE,
        );
        let errors = settled
            .close()
            .err()
            .expect("the shared payload is refused");
        assert_eq!(
            errors.len(),
            1,
            "the shared payload cannot be both Bool and String"
        );
        assert!(matches!(
            errors[0].kind,
            super::TypeErrorKind::TyMismatch { .. }
        ));
    }
}
