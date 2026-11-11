//! The grant tables: `grant`'s options, which are also the capability
//! profile's keyset, and the two nested tables `exec` and `fs` hold.
//!
//! The vocabularies are minted from the Rust types (`EditorVerb`, `ShellVerb`,
//! `Net`, `Gpu`, `Selector`, `Anchor`), so a new verb, selector or XDG kind is one
//! variant.  No type can sit in a `static`, so each key holds the function that
//! builds its type.

use super::{Holds, Key, Table, Vocab};
use crate::authority::grant::freeze::{Anchor, Selector};
use crate::authority::policy::{EditorVerb, Gpu, Net, ShellVerb};
use crate::fact::{GrantVocabulary, RefusedGrantKey};
use crate::ty::Ty;
use crate::typecheck::error::Reason;
use strum::VariantArray;

/// `grant`'s options.
///
/// Checked statically: `net`, `gpu`, `editor` and `shell` are held to closed
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
            holds: Holds::At(net),
            reason: Some(Reason::Vocabulary(&NET)),
        },
        Key {
            label: "gpu",
            holds: Holds::At(gpu),
            reason: Some(Reason::Vocabulary(&GPU)),
        },
        Key {
            label: "editor",
            holds: Holds::At(editor),
            reason: Some(Reason::Vocabulary(&EDITOR)),
        },
        Key {
            label: "shell",
            holds: Holds::At(shell),
            reason: Some(Reason::Vocabulary(&SHELL)),
        },
        Key {
            label: "detach",
            holds: Holds::Refused(RefusedGrantKey::DetachMovedToShell),
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
            holds: Holds::At(exec_targets),
            reason: Some(Reason::Vocabulary(&EXEC_TARGET)),
        },
        Key {
            label: "deny",
            holds: Holds::At(exec_targets),
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
            holds: Holds::At(places),
            reason: Some(Reason::Vocabulary(&PLACE)),
        },
        Key {
            label: "write",
            holds: Holds::At(places),
            reason: Some(Reason::Vocabulary(&PLACE)),
        },
        Key {
            label: "deny",
            holds: Holds::At(places),
            reason: Some(Reason::Vocabulary(&PLACE)),
        },
    ],
    factory: false,
};

pub(crate) static PLACE: Vocab = Vocab {
    id: GrantVocabulary::Place,
    row: place_row,
    untagged: "a place is a tagged path: write `` `absolute /tmp `` or `` `cwd src `` for a path under the working directory",
    payload: "a place's payload is a path: `` `absolute /tmp ``, `` `cwd src ``",
    within: &[],
};

pub(crate) static EXEC_TARGET: Vocab = Vocab {
    id: GrantVocabulary::ExecTarget,
    row: exec_target_row,
    untagged: "an exec target is tagged: write `` `name git `` for a command found on the PATH, or a place such as `` `absolute /bin/sh `` or `` `cwd tools/build ``",
    payload: "`` `name `` takes a command name and a place takes a path: `` `name git ``, `` `cwd tools/build ``; `` `dir `` takes a place or a selector with no payload of its own",
    within: &[("dir", &DIR_TARGET)],
};

pub(crate) static DIR_TARGET: Vocab = Vocab {
    id: GrantVocabulary::DirTarget,
    row: dir_target_row,
    untagged: "`` `dir `` takes a place or a selector: `` `dir `cwd tools ``, or `` `dir `system ``",
    payload: "a place takes a path and a selector takes none: `` `dir `cwd tools ``, `` `dir `system ``",
    within: &[],
};

pub(crate) static NET: Vocab = Vocab {
    id: GrantVocabulary::Net,
    row: nullary::<Net>,
    untagged: "a grant only narrows: omit `net` to inherit, or write `` net: `offline ``",
    payload: "`` `offline `` takes no payload",
    within: &[],
};

pub(crate) static GPU: Vocab = Vocab {
    id: GrantVocabulary::Gpu,
    row: nullary::<Gpu>,
    untagged: "a grant only narrows: omit `gpu` to inherit, or write `` gpu: `withheld ``",
    payload: "`` `withheld `` takes no payload",
    within: &[],
};

pub(crate) static EDITOR: Vocab = Vocab {
    id: GrantVocabulary::Editor,
    row: nullary::<EditorVerb>,
    untagged: "`editor` takes the list of verbs it permits, such as `` editor: [`read] ``; omit it to inherit",
    payload: "a verb takes no payload",
    within: &[],
};

pub(crate) static SHELL: Vocab = Vocab {
    id: GrantVocabulary::Shell,
    row: nullary::<ShellVerb>,
    untagged: "`shell` takes the list of verbs it permits, such as `` shell: [`chdir] ``; omit it to inherit",
    payload: "a verb takes no payload",
    within: &[],
};

type Tags = Vec<(String, Ty)>;

fn nullary<V: VariantArray + Copy + Into<&'static str>>() -> Tags {
    V::VARIANTS
        .iter()
        .map(|&v| (v.into().to_string(), Ty::unit()))
        .collect()
}

fn place_row() -> Tags {
    Anchor::all()
        .map(|a| (a.tag().into_owned(), Ty::string()))
        .collect()
}

fn dir_target_row() -> Tags {
    let selectors = Selector::names().map(|n| (n.to_string(), Ty::unit()));
    place_row().into_iter().chain(selectors).collect()
}

fn exec_target_row() -> Tags {
    let name = ("name".to_string(), Ty::string());
    let nested = EXEC_TARGET
        .within
        .iter()
        .map(|&(tag, v)| (tag.to_string(), v.ty()));
    std::iter::once(name)
        .chain(place_row())
        .chain(nested)
        .collect()
}

fn net() -> Ty {
    NET.ty()
}

fn gpu() -> Ty {
    GPU.ty()
}

fn editor() -> Ty {
    Ty::list(EDITOR.ty())
}

fn shell() -> Ty {
    Ty::list(SHELL.ty())
}

fn places() -> Ty {
    Ty::list(PLACE.ty())
}

fn exec_targets() -> Ty {
    Ty::list(EXEC_TARGET.ty())
}
