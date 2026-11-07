//! A grant a test writes in Rust: it spells a grant value, which the one
//! front-end resolves as it resolves any other.

use super::freeze::Place;
use super::value::{Axes, ExecTable, FsTable, GrantValue, Target};
use crate::authority::policy::{EditorVerb, Gpu, Net, ShellVerb};
use crate::path::FrozenPath;
use std::collections::BTreeSet;

/// An exec key: a bare name, a file, or every program beneath a directory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExecKey {
    Name(String),
    Path(FrozenPath),
    Dir(FrozenPath),
}

/// Exec authority as written. A key in both sets is Both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecGrant {
    allow: BTreeSet<ExecKey>,
    deny: BTreeSet<ExecKey>,
}

impl ExecGrant {
    pub fn of(
        allow: impl IntoIterator<Item = ExecKey>,
        deny: impl IntoIterator<Item = ExecKey>,
    ) -> Self {
        Self {
            allow: allow.into_iter().collect(),
            deny: deny.into_iter().collect(),
        }
    }
}

/// Filesystem access as written: `deny_paths` carve out subtrees no
/// access may touch, even under a covering prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsPolicy {
    pub read_prefixes: Vec<FrozenPath>,
    pub write_prefixes: Vec<FrozenPath>,
    pub deny_paths: Vec<FrozenPath>,
}

/// The per-effect policies as written, `None` inheriting: every path an
/// `absolute` spelling, nothing resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub exec: Option<ExecGrant>,
    pub fs: Option<FsPolicy>,
    pub net: Option<Net>,
    pub gpu: Option<Gpu>,
    pub editor: Option<BTreeSet<EditorVerb>>,
    pub shell: Option<BTreeSet<ShellVerb>>,
}

impl Capabilities {
    /// The grant value this spells, as an author writes it in ral.
    pub fn value(self) -> GrantValue {
        let Self {
            exec,
            fs,
            net,
            gpu,
            editor,
            shell,
        } = self;
        let targets = |keys: BTreeSet<ExecKey>| keys.into_iter().map(target).collect();
        let places = |paths: Vec<FrozenPath>| paths.into_iter().map(place).collect();
        Axes {
            exec: exec.map(|e| ExecTable {
                allow: targets(e.allow),
                deny: targets(e.deny),
            }),
            fs: fs.map(|f| FsTable {
                read: places(f.read_prefixes),
                write: places(f.write_prefixes),
                deny: places(f.deny_paths),
            }),
            net,
            gpu,
            editor,
            shell,
        }
        .into()
    }
}

fn place(path: FrozenPath) -> Place {
    Place::Absolute(path.into_string())
}

fn target(key: ExecKey) -> Target {
    match key {
        ExecKey::Name(name) => Target::Name(name),
        ExecKey::Path(file) => Target::File(place(file)),
        ExecKey::Dir(dir) => Target::Dir(place(dir)),
    }
}
