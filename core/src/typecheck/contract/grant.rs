//! The grant tables: `grant`'s options, which are also the capability
//! profile's keyset, and the two nested tables `exec` and `fs` hold.
//!
//! The vocabularies are minted from the Rust types (`EditorVerb`, `ShellVerb`,
//! `Net`, `Selector`, `Anchor`), so a new verb, selector or XDG kind is one
//! variant.  A `Ty::List` cannot sit in a `static`, hence `Shaped`.

use super::{Holds, Key, Table, Vocab};
use crate::capability::{EditorVerb, Net, ShellVerb};
use crate::guard::freeze::{Anchor, Selector};
use crate::ty::Ty;
use crate::typecheck::error::Reason;
use crate::typecheck::unify::Unifier;
use strum::VariantArray;

/// `grant`'s options.
///
/// Checked statically: `net`, `editor` and `shell` are held to closed
/// variants, so a misspelt tag or verb is an error at the form; `exec` and
/// `fs` are nested tables, ascribed once inference has finished (their keys
/// are optional, which no row expresses), as records only, with their lists
/// held to closed rows of exec targets and places.  Nothing here constrains
/// a bound table.
///
/// Left to `decode_capability_map`: whatever the types cannot see.  A table
/// typed at a variable (a generic function's parameter, a `restrict` record
/// off the wire) reaches the decoder unchecked, and the decoder still
/// judges every payload: a place's subpath, an executable's name, a `~` that
/// would never expand.
pub(super) static GRANT: Table = Table {
    form: "grant",
    keys: &[
        Key {
            label: "exec",
            holds: Holds::Table(&EXEC),
            reason: None,
        },
        Key {
            label: "fs",
            holds: Holds::Table(&FS),
            reason: None,
        },
        Key {
            label: "net",
            holds: Holds::Shaped(net),
            reason: Some(Reason::Vocabulary(&NET)),
        },
        Key {
            label: "editor",
            holds: Holds::Shaped(editor),
            reason: Some(Reason::Vocabulary(&EDITOR)),
        },
        Key {
            label: "shell",
            holds: Holds::Shaped(shell),
            reason: Some(Reason::Vocabulary(&SHELL)),
        },
        Key {
            label: "detach",
            holds: Holds::Refused(
                "`detach` is a shell verb now: list it in `shell`, as in `` shell: [`chdir, `detach] ``",
            ),
            reason: None,
        },
    ],
    factory: false,
};

static EXEC: Table = Table {
    form: "grant exec",
    keys: &[
        Key {
            label: "allow",
            holds: Holds::Shaped(exec_targets),
            reason: Some(Reason::Vocabulary(&EXEC_TARGET)),
        },
        Key {
            label: "deny",
            holds: Holds::Shaped(exec_targets),
            reason: Some(Reason::Vocabulary(&EXEC_TARGET)),
        },
    ],
    factory: false,
};

static FS: Table = Table {
    form: "grant fs",
    keys: &[
        Key {
            label: "read",
            holds: Holds::Shaped(places),
            reason: Some(Reason::Vocabulary(&PLACE)),
        },
        Key {
            label: "write",
            holds: Holds::Shaped(places),
            reason: Some(Reason::Vocabulary(&PLACE)),
        },
        Key {
            label: "deny",
            holds: Holds::Shaped(places),
            reason: Some(Reason::Vocabulary(&PLACE)),
        },
    ],
    factory: false,
};

pub(crate) static PLACE: Vocab = Vocab {
    noun: "place",
    row: place_row,
    misses: place_misses,
    untagged: "a place is a tagged path: write `` `absolute /tmp `` or `` `cwd src `` for a path under the working directory",
    payload: "a place's payload is a path: `` `absolute /tmp ``, `` `cwd src ``",
    within: &[],
};

const FS_TAKES: &str = "fs lists take places such as `` `absolute `` or `` `cwd ``";

fn selector_tag(tag: &str) -> bool {
    tag.parse::<Selector>().is_ok()
}

fn place_misses(tag: &str) -> Option<String> {
    match tag {
        "name" | "dir" => Some(format!(
            "`` `name `` and `` `dir `` belong in an exec list; {FS_TAKES}"
        )),
        _ if selector_tag(tag) => Some(format!(
            "`` `{tag} `` names directories to run programs from, so it belongs in an exec list \
             as `` `dir `{tag} ``; {FS_TAKES}"
        )),
        _ => None,
    }
}

pub(crate) static EXEC_TARGET: Vocab = Vocab {
    noun: "exec target",
    row: exec_target_row,
    misses: exec_target_misses,
    untagged: "an exec target is tagged: write `` `name git `` for a command found on the PATH, or a place such as `` `absolute /bin/sh `` or `` `cwd tools/build ``",
    payload: "`` `name `` takes a command name and a place takes a path: `` `name git ``, `` `cwd tools/build ``; `` `dir `` takes a place or a selector with no payload of its own",
    within: &[("dir", &DIR_TARGET)],
};

fn exec_target_misses(tag: &str) -> Option<String> {
    selector_tag(tag).then(|| {
        format!("`` `{tag} `` names directories to run programs from; write `` `dir `{tag} ``")
    })
}

pub(crate) static DIR_TARGET: Vocab = Vocab {
    noun: "place or a selector",
    row: dir_target_row,
    misses: dir_target_misses,
    untagged: "`` `dir `` takes a place or a selector: `` `dir `cwd tools ``, or `` `dir `system ``",
    payload: "a place takes a path and a selector takes none: `` `dir `cwd tools ``, `` `dir `system ``",
    within: &[],
};

fn dir_target_misses(tag: &str) -> Option<String> {
    match tag {
        "name" => Some(
            "`` `dir `` takes a place or a selector; write `` `name git `` in the list itself for a command"
                .into(),
        ),
        "dir" => Some("`` `dir `` does not nest; write `` `dir `cwd tools ``".into()),
        _ => None,
    }
}

fn no_misses(_: &str) -> Option<String> {
    None
}

pub(crate) static NET: Vocab = Vocab {
    noun: "net mode",
    row: nullary::<Net>,
    misses: no_misses,
    untagged: "a grant only narrows: omit `net` to inherit, or write `` net: `offline ``",
    payload: "`` `offline `` takes no payload",
    within: &[],
};

pub(crate) static EDITOR: Vocab = Vocab {
    noun: "verb",
    row: nullary::<EditorVerb>,
    misses: no_misses,
    untagged: "`editor` takes the list of verbs it permits, such as `` editor: [`read] ``; omit it to inherit",
    payload: "a verb takes no payload",
    within: &[],
};

pub(crate) static SHELL: Vocab = Vocab {
    noun: "verb",
    row: nullary::<ShellVerb>,
    misses: no_misses,
    untagged: "`shell` takes the list of verbs it permits, such as `` shell: [`chdir] ``; omit it to inherit",
    payload: "a verb takes no payload",
    within: &[],
};

type Tags = Vec<(String, Ty)>;

fn nullary<V: VariantArray + Copy + Into<&'static str>>() -> Tags {
    V::VARIANTS
        .iter()
        .map(|&v| (v.into().to_string(), Ty::Unit))
        .collect()
}

fn place_row() -> Tags {
    Anchor::all()
        .map(|a| (a.tag().into_owned(), Ty::String))
        .collect()
}

fn dir_target_row() -> Tags {
    [place_row(), nullary::<Selector>()].concat()
}

fn exec_target_row() -> Tags {
    let name = ("name".to_string(), Ty::String);
    let nested = EXEC_TARGET
        .within
        .iter()
        .map(|&(tag, v)| (tag.to_string(), v.ty()));
    std::iter::once(name)
        .chain(place_row())
        .chain(nested)
        .collect()
}

fn net(_: &mut Unifier) -> Ty {
    NET.ty()
}

fn editor(_: &mut Unifier) -> Ty {
    Ty::list(EDITOR.ty())
}

fn shell(_: &mut Unifier) -> Ty {
    Ty::list(SHELL.ty())
}

fn places(_: &mut Unifier) -> Ty {
    Ty::list(PLACE.ty())
}

fn exec_targets(_: &mut Unifier) -> Ty {
    Ty::list(EXEC_TARGET.ty())
}
