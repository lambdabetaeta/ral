//! The declared tables.
//!
//! A form's options and a contract file's return value are the same thing: a
//! closed set of labels, each held at what its table says.  `within` and
//! `grant` check theirs key by written key at the form, the bracket being
//! syntax (`Inferencer::check_options`); an rc file, a capability profile
//! and a plugin manifest are ascribed the table once inference has finished,
//! as ML ascribes a signature (`ascribe`).  The runtime doors —
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

use super::env::InferCtx;
use super::error::{Reason, Standing, TypeErrorKind};
use super::ty::{CompTy, Label, Row, Ty};
use super::unify::Unifier;
use crate::ir::{CompKind, Phrase, Val};
use crate::source::{Span, Spanned};

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
    /// A shape minted fresh per occurrence, variables and all.
    Shaped(fn(&mut Unifier) -> Ty),
    /// Known, and refused, with advice of its own rather than "unknown key".
    Refused(&'static str),
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
    /// Whether a file may return a function of its options instead of the
    /// settings, leaving the door to meet what the function returns.
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
/// and the argv that writes and returns `()`.
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
            holds: Holds::Shaped(catch_all_ty),
            reason: Some(Reason::StandsIn(Standing::EveryCommand)),
        },
    ],
    factory: false,
};

/// `grant`'s options, which are also the capability profile's keyset.
///
/// Everything but the two flags is `decode_capability_map`'s: a policy value
/// may be a string or a list, and the two mix within one record.  Typing them
/// to full depth would put one label at two ground types — `editor.read`
/// beside `fs.read`.
static GRANT: Table = Table {
    form: "grant",
    keys: &[
        Key {
            label: "exec",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "fs",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "net",
            holds: Holds::At(Ty::Bool),
            reason: None,
        },
        Key {
            label: "detach",
            holds: Holds::At(Ty::Bool),
            reason: None,
        },
        Key {
            label: "editor",
            holds: Holds::Decoded,
            reason: None,
        },
        Key {
            label: "shell",
            holds: Holds::Decoded,
            reason: None,
        },
    ],
    factory: false,
};

/// The rc file's eleven keys.  `apply_rc_key` dispatches off this table and
/// applies what each one means; the shapes left to it — a map of hooks, a map
/// of aliases, a theme — are one type per key rather than one across the key,
/// and no single `Ty` pins them.
static RC: Table = Table {
    form: "rc",
    keys: &[
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
            label: "edit_mode",
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
            label: "recursion_limit",
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
/// "unknown key, here is the list" — the advice is the only thing that
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
                "manifest 'capabilities:' is not enforced — plugins run with host \
                 authority. Remove the key; to confine a plugin invocation, wrap it \
                 in `grant { ... }`.",
            ),
            reason: None,
        },
    ],
    factory: true,
};

/// What the catch-all `handler:` is: `String → [String] → F Unit`.
fn catch_all_ty(_u: &mut Unifier) -> Ty {
    Ty::Thunk(Box::new(CompTy::Fun(
        Box::new(Ty::String),
        Box::new(CompTy::Fun(
            Box::new(Ty::argv()),
            Box::new(CompTy::pure(Ty::Unit)),
        )),
    )))
}

/// The table `form` declares.
pub fn declared(form: Form) -> &'static Table {
    match form {
        Form::Within => &WITHIN,
        Form::Grant => &GRANT,
        Form::Rc => &RC,
        Form::Manifest => &MANIFEST,
    }
}

/// What a runtime door says about a key a table does not name.
pub(super) fn unknown_key_message(form: &str, key: &str, offered: &[&str]) -> String {
    format!(
        "unknown key '{key}' — `{form}` takes {list}",
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

/// Where `label`'s value sits, when the options were written out.
pub(super) fn written_at(opts: &Val, label: &str) -> Option<Span> {
    let Val::Record(entries) = opts else {
        return None;
    };
    entries
        .shape()
        .iter()
        .find(|(key, _)| key.as_ref() == label)
        .and_then(|(_, value)| value.span)
}

/// Hold a program's own return value to `table`, once inference has finished.
///
/// The *inferred* type is what is ascribed, whatever syntax produced it, so a
/// key misspelled inside a spread is caught as one written out is.  Nothing
/// constrains the program: the check runs in a scratch copy of the unifier, on
/// a finished result that flows nowhere else, so the row's open tail is read
/// as closed.  A return typed at a variable (`from-json`) and, where the table
/// admits one, a factory stay on the runtime door.
pub(super) fn ascribe(
    ctx: &mut InferCtx,
    tail: Option<&Spanned<Phrase>>,
    cty: Option<CompTy>,
    table: &'static Table,
) {
    let (Some(phrase), Some(cty)) = (tail, cty) else {
        return;
    };
    let Phrase::Run(comp) = &phrase.item else {
        return;
    };
    let CompTy::Return(value) = ctx.unifier.resolve_comp_ty(&cty) else {
        return;
    };
    let written = match &comp.item {
        CompKind::Return(record @ Val::Record(_)) => Some(record),
        _ => None,
    };
    let saved = ctx.pos;
    ctx.pos = phrase.span;
    match ctx.unifier.resolve_ty(&value) {
        Ty::Record(row) => ascribe_row(ctx, &row, table, written),
        Ty::Unit => ascribe_row(ctx, &Row::Empty, table, written),
        Ty::Var(_) => {}
        Ty::Thunk(_) if table.factory => {}
        found => ctx.diagnose(TypeErrorKind::ReturnNotRecord {
            form: table.form,
            found,
            offered: table.offered(),
        }),
    }
    ctx.pos = saved;
}

/// Each field on `row`'s spine against `table`, then every required key
/// against the spine.
fn ascribe_row(ctx: &mut InferCtx, row: &Row, table: &'static Table, written: Option<&Val>) {
    let mut scratch = ctx.unifier.clone();
    let phrase_pos = ctx.pos;
    let mut present = Vec::new();
    let mut rest = ctx.unifier.resolve_row(row);
    while let Row::Extend(label, payload, tail) = rest {
        rest = ctx.unifier.resolve_row(&tail);
        let Label::Field(label) = label else {
            continue;
        };
        present.push(label.clone());
        ctx.pos = written.and_then(|w| written_at(w, &label)).or(phrase_pos);
        scratch.at = ctx.pos;
        let Some(key) = table.keys.iter().find(|k| k.label == label) else {
            ctx.diagnose(TypeErrorKind::UnknownKey {
                form: table.form,
                key: label,
                offered: table.offered(),
            });
            continue;
        };
        let declared = match &key.holds {
            Holds::At(ty) | Holds::Required(ty) => ty.clone(),
            Holds::Shaped(mint) => mint(&mut scratch),
            Holds::Decoded => continue,
            Holds::Refused(advice) => {
                ctx.diagnose(TypeErrorKind::RefusedKey {
                    form: table.form,
                    key: key.label,
                    advice,
                });
                continue;
            }
        };
        if let Err(kind) = scratch.unify_ty(&declared, &payload) {
            ctx.report(kind, field_reason(table, key));
        }
    }
    ctx.pos = phrase_pos;
    for key in table.keys {
        if matches!(key.holds, Holds::Required(_)) && !present.iter().any(|l| l == key.label) {
            ctx.diagnose(TypeErrorKind::RowMissingField {
                label: key.label.to_string(),
            });
        }
    }
}
