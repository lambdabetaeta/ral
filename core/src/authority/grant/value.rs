//! The grant value: a grant as data, the form in which one host hands a
//! grant to another, decoded where it lands by
//! [`decode_grant`](crate::shell::grant::decode_grant).

use super::freeze::Place;
use crate::authority::policy::{EditorVerb, Gpu, Net, ShellVerb};
use crate::data::FOValue;
use crate::data::datum::{Datum as _, tag};
use std::collections::{BTreeMap, BTreeSet};

/// A grant value, made only by encoding [`Axes`].
#[derive(Debug, Clone, PartialEq)]
pub struct GrantValue(FOValue);

/// An exec target as a grant writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A bare command name: `` `name git ``.
    Name(String),
    /// One program.
    File(Place),
    /// Every program beneath a directory: `` `dir `absolute /opt/bin ``.
    Dir(Place),
}

/// `exec: [allow: [...], deny: [...]]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecTable {
    pub allow: Vec<Target>,
    pub deny: Vec<Target>,
}

/// `fs: [read: [...], write: [...], deny: [...]]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsTable {
    pub read: Vec<Place>,
    pub write: Vec<Place>,
    pub deny: Vec<Place>,
}

/// A grant's axes as written, `None` where it inherits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Axes {
    pub exec: Option<ExecTable>,
    pub fs: Option<FsTable>,
    pub net: Option<Net>,
    pub gpu: Option<Gpu>,
    pub editor: Option<BTreeSet<EditorVerb>>,
    pub shell: Option<BTreeSet<ShellVerb>>,
}

impl GrantValue {
    /// Every axis pinned to its least: pushed as a layer, it withholds all
    /// positive authority.
    pub fn deny_all() -> Self {
        Axes {
            exec: Some(ExecTable::default()),
            fs: Some(FsTable::default()),
            net: Some(Net::Offline),
            gpu: Some(Gpu::Withheld),
            editor: Some(BTreeSet::new()),
            shell: Some(BTreeSet::new()),
        }
        .into()
    }
}

impl From<Axes> for GrantValue {
    fn from(axes: Axes) -> Self {
        let Axes {
            exec,
            fs,
            net,
            gpu,
            editor,
            shell,
        } = axes;
        let entries = [
            exec.map(|e| {
                let table = record([
                    ("allow", list(e.allow, target)),
                    ("deny", list(e.deny, target)),
                ]);
                ("exec", table)
            }),
            fs.map(|f| {
                let table = record([
                    ("read", list(&f.read, place)),
                    ("write", list(&f.write, place)),
                    ("deny", list(&f.deny, place)),
                ]);
                ("fs", table)
            }),
            net.map(|n| ("net", nullary(n))),
            gpu.map(|g| ("gpu", nullary(g))),
            editor.map(|v| ("editor", list(v, nullary))),
            shell.map(|v| ("shell", list(v, nullary))),
        ];
        Self(record(entries.into_iter().flatten()))
    }
}

impl From<GrantValue> for FOValue {
    fn from(value: GrantValue) -> Self {
        value.0
    }
}

impl AsRef<FOValue> for GrantValue {
    fn as_ref(&self) -> &FOValue {
        &self.0
    }
}

fn record<'a>(entries: impl IntoIterator<Item = (&'a str, FOValue)>) -> FOValue {
    let entries = entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value));
    entries.collect::<BTreeMap<_, _>>().encode()
}

fn list<T>(items: impl IntoIterator<Item = T>, encode: impl Fn(T) -> FOValue) -> FOValue {
    items.into_iter().map(encode).collect::<Vec<_>>().encode()
}

fn nullary(word: impl Into<&'static str>) -> FOValue {
    tag(word.into(), None)
}

fn place(place: &Place) -> FOValue {
    tag(&place.tag(), Some(place.payload().to_owned().encode()))
}

fn target(target: Target) -> FOValue {
    match target {
        Target::Name(name) => tag("name", Some(name.encode())),
        Target::File(file) => place(&file),
        Target::Dir(dir) => tag("dir", Some(place(&dir))),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::grant::freeze::FreezeCtx;
    use crate::authority::policy::{Flag, FsOp};
    use crate::shell::grant::decode_grant;
    use crate::test_helper::Tree;

    /// Every axis the encoder writes, the decoder reads: anchored places
    /// resolve against the decoding host's own anchor.
    #[test]
    fn every_axis_written_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = Tree::lay(tmp.path(), ["/work/", "/bin/tool"]);
        let cwd = crate::path::CwdPath::new(tree.path("/work")).unwrap();
        let ctx = FreezeCtx::at(crate::host::EnvVars::new(), cwd);
        let tool = Place::Absolute(tree.at("/bin/tool"));
        let value = GrantValue::from(Axes {
            exec: Some(ExecTable {
                allow: vec![Target::File(tool), Target::Name("ral-no-such-tool".into())],
                deny: vec![Target::Dir(Place::Cwd(".".into()))],
            }),
            fs: Some(FsTable {
                read: vec![Place::Cwd(".".into())],
                ..FsTable::default()
            }),
            net: Some(Net::Offline),
            gpu: Some(Gpu::Withheld),
            editor: Some([EditorVerb::Read].into()),
            shell: Some(BTreeSet::new()),
        });
        let layer = decode_grant(value.as_ref(), "test", &ctx).unwrap();
        let exec = layer.exec().expect("an exec axis");
        assert_eq!((exec.allows().count(), exec.denies().count()), (1, 1));
        let reads = layer.fs().expect("an fs axis").table(&FsOp::Read);
        assert_eq!(reads.allows().count(), 1);
        let stack = crate::test_helper::stack([layer]);
        assert_eq!(stack.net_veto(), Some(Net::Offline));
        assert_eq!(stack.gpu().collect::<Vec<_>>(), [Gpu::Withheld]);
        assert!(stack.permits(Flag::Editor(EditorVerb::Read)));
        assert!(!stack.permits(Flag::Editor(EditorVerb::Write)));
        assert!(!stack.permits(Flag::Shell(ShellVerb::Chdir)));
    }
}
