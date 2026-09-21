//! The declared tables, and the one condition a variable flag owes.
//!
//! A form's options and a contract file's return value are the same thing: a
//! closed set of labels, each held at what its table says.  `within` and
//! `grant` check theirs as a row at the form; an rc file and a plugin manifest
//! check theirs as the row their program returns.  The runtime doors —
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
//!
//! §3's condition is why they are gathered here rather than left where they
//! are used: a label may not be optional at two irreconcilable types within one
//! check.  Two tables that disagree at one label would make the order two
//! constraints arrive in decide the verdict, and order-independence is a
//! theorem about the term rules plus this door — so the door is load-bearing,
//! and must be re-established for every new site that introduces a variable
//! flag.
//!
//! An ordinary program cannot reach the condition — the put rule mints a
//! variable flag over a *fresh* payload, and every other rule pins `Present` —
//! so a declared table is the only construct that puts a ground payload beside
//! a variable flag.  [`check_one_optional_type`] is the refusal at the door,
//! and [`declared`] is that door.

use std::sync::OnceLock;

use super::error::Reason;
use super::ty::{CompTy, Ty};
use super::unify::Unifier;

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
    /// Held at this ground type.  Two tables naming one label at two types that
    /// will not unify is the condition's refusal.
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
        format!(
            "unknown key '{key}' — `{form}` takes {list}",
            form = self.form,
            list = self.offered().join(", "),
        )
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

/// Two tables naming one label at two different ground types: the condition's
/// refusal, naming both tables so neither author has to guess whose it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clash {
    pub label: &'static str,
    pub forms: [&'static str; 2],
}

/// `within`'s options.  `env:` is heterogeneous and `parse_env`'s to judge, so
/// it stays the decoder's.
///
/// The catch-all declares a route *variable* and pins the value at `Unit`:
/// `bytes_subsumes`' two branches both end in `value ~ Unit` and differ only
/// in whether the route is left at `Value`, so `Unit` is the whole of WF-2's
/// subsumption and leaving the route free is the rest.  Declaring `Bytes`
/// here would commit a bound arm's route one occurrence at a time.
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
            reason: Some(Reason::CatchAllRoutePin),
        },
    ],
};

/// `grant`'s options, which are also the capability profile's keyset.
///
/// Everything but the two flags is `decode_capability_map`'s: a policy value
/// may be a string or a list, and the two mix within one record.  Typing them
/// to full depth would put one label at two ground types — `editor.read`
/// beside `fs.read` — which is the one condition a variable flag owes.
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
};

/// Every table this build declares, and the one place a new one is added.
static DECLARED: &[&Table] = &[&WITHIN, &GRANT, &RC, &MANIFEST];

fn catch_all_ty(u: &mut Unifier) -> Ty {
    let route = u.fresh_route();
    Ty::Thunk(Box::new(CompTy::Fun(
        Box::new(Ty::String),
        Box::new(CompTy::Fun(
            Box::new(Ty::argv()),
            Box::new(CompTy::Return(route, Box::new(Ty::Unit))),
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

/// §3's condition over the declared set, computed once: within one check, a
/// label may not be optional at two types that will not unify.
///
/// # Errors
/// The clash, if this build's tables have one.
pub fn condition() -> Result<(), &'static Clash> {
    static CHECKED: OnceLock<Result<(), Clash>> = OnceLock::new();
    match CHECKED.get_or_init(|| check_one_optional_type(DECLARED)) {
        Ok(()) => Ok(()),
        Err(clash) => Err(clash),
    }
}

/// §3's condition over an arbitrary set of tables — the check [`condition`]
/// runs, exposed so a host crate's own table can be held to it too.
///
/// # Errors
/// The first label two tables name at two types that will not unify.
pub fn check_one_optional_type(tables: &[&'static Table]) -> Result<(), Clash> {
    for (i, table) in tables.iter().enumerate() {
        for key in table.keys {
            for other in &tables[i + 1..] {
                for twin in other.keys.iter().filter(|k| k.label == key.label) {
                    let mut u = Unifier::new();
                    let (Some(a), Some(b)) = (
                        declaration_ty(&mut u, &key.holds),
                        declaration_ty(&mut u, &twin.holds),
                    ) else {
                        continue;
                    };
                    if u.unify_ty(&a, &b).is_err() {
                        return Err(Clash {
                            label: key.label,
                            forms: [table.form, other.form],
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// What one occurrence of this declaration mints, or `None` for a label that
/// carries no type at all.  Minting is what makes the comparison right:
/// `Decoded` and `Shaped` are fresh per occurrence, so two of them meet as the
/// occurrences would rather than as written text.
fn declaration_ty(u: &mut Unifier, holds: &Holds) -> Option<Ty> {
    match holds {
        Holds::At(ty) | Holds::Required(ty) => Some(ty.clone()),
        Holds::Decoded => Some(u.fresh_ty()),
        Holds::Shaped(mint) => Some(mint(u)),
        Holds::Refused(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_declared_tables_meet_the_condition() {
        assert_eq!(condition(), Ok(()));
    }

    #[test]
    fn one_label_at_two_ground_types_is_refused() {
        static LEFT: Table = Table {
            form: "left",
            keys: &[Key {
                label: "depth",
                holds: Holds::At(Ty::Int),
                reason: None,
            }],
        };
        static RIGHT: Table = Table {
            form: "right",
            keys: &[Key {
                label: "depth",
                holds: Holds::At(Ty::String),
                reason: None,
            }],
        };
        let clash = check_one_optional_type(&[&LEFT, &RIGHT]).expect_err("one label, two types");
        assert_eq!(clash.label, "depth");
        assert_eq!(clash.forms, ["left", "right"]);
    }

    #[test]
    fn a_shared_label_at_one_ground_type_is_no_clash() {
        static LEFT: Table = Table {
            form: "left",
            keys: &[Key {
                label: "bell",
                holds: Holds::At(Ty::Bool),
                reason: None,
            }],
        };
        static RIGHT: Table = Table {
            form: "right",
            keys: &[Key {
                label: "bell",
                holds: Holds::Required(Ty::Bool),
                reason: None,
            }],
        };
        assert_eq!(check_one_optional_type(&[&LEFT, &RIGHT]), Ok(()));
    }

    /// A shape at a label another table holds at a ground type is the same
    /// order-dependence as two ground types, so the door compares
    /// unifiability rather than equality and `Shaped` is not exempt.
    #[test]
    fn a_shape_against_a_ground_type_is_refused() {
        static LEFT: Table = Table {
            form: "left",
            keys: &[Key {
                label: "handler",
                holds: Holds::Shaped(catch_all_ty),
                reason: None,
            }],
        };
        static RIGHT: Table = Table {
            form: "right",
            keys: &[Key {
                label: "handler",
                holds: Holds::At(Ty::String),
                reason: None,
            }],
        };
        let clash = check_one_optional_type(&[&LEFT, &RIGHT]).expect_err("a thunk is not a string");
        assert_eq!(clash.label, "handler");
        assert_eq!(clash.forms, ["left", "right"]);
    }

    /// The labels the four share today — `env` across `within` and the rc,
    /// `aliases` across the rc and the manifest — are untyped in both, so
    /// nothing meets.
    #[test]
    fn the_shared_labels_are_untyped_in_both() {
        for (form, label) in [
            (Form::Within, "env"),
            (Form::Rc, "env"),
            (Form::Rc, "aliases"),
            (Form::Manifest, "aliases"),
        ] {
            assert!(matches!(
                declared(form).holds(label),
                Some(Holds::Decoded | Holds::Shaped(_))
            ));
        }
    }
}
