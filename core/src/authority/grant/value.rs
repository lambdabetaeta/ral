//! The grant value: a grant as data, its one outward form.
//!
//! One host hands a grant to another in it, decoded where it lands by
//! [`decode_grant`](crate::shell::grant::decode_grant); a layer shows itself
//! in it ([`LayerValue`]); ral's value printer shows it ([`printed`], [`LayerValue`]'s `Display`).

use super::freeze::Place;
use crate::authority::policy::{EditorVerb, Gpu, Net, ShellVerb};
use crate::data::FOValue;
use crate::data::datum::{Datum as _, tag};
use crate::data::print::{PrintParams, pretty_print};
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
                let lists = [
                    ("allow", list(e.allow, target)),
                    ("deny", list(e.deny, target)),
                ];
                ("exec", table(lists))
            }),
            fs.map(|f| {
                let lists = [
                    ("read", list(&f.read, place)),
                    ("write", list(&f.write, place)),
                    ("deny", list(&f.deny, place)),
                ];
                ("fs", table(lists))
            }),
            net.map(|n| ("net", nullary(n))),
            gpu.map(|g| ("gpu", nullary(g))),
            editor.map(|v| ("editor", list(v, nullary))),
            shell.map(|v| ("shell", list(v, nullary))),
        ];
        Self(record(entries.into_iter().flatten()))
    }
}

/// A layer as data, decode's inverse on the host that decoded it.
///
/// The grant that decodes to it, then, where the layer runs every program,
/// which no grant spells, the fragment that extends that grant to it:
/// `decode_grant(grant) ◁ decode_fragment(fragment)`.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerValue {
    pub grant: FOValue,
    pub fragment: Option<FOValue>,
}

crate::record!(LayerValue {
    grant: "grant",
    fragment: "fragment",
});

/// As `grant` writes it, or, where no grant spells it, its grant extended by
/// its fragment.
impl std::fmt::Display for LayerValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `grant`'s bracket is the form's: the empty grant is `[]`.
        let grant = match &self.grant {
            FOValue::Map { entries } if entries.is_empty() => "[]".into(),
            grant => printed(grant),
        };
        write!(f, "grant {grant}")?;
        match &self.fragment {
            None => Ok(()),
            Some(fragment) => write!(f, " extended by {}", printed(fragment)),
        }
    }
}

/// Whole: a rule elided would predict the wrong denial.
const PRINT: PrintParams = PrintParams {
    max_width: 100,
    max_string: 0,
    max_depth: usize::MAX,
    min_quote_hashes: 0,
    quote_bytes: true,
    max_bytes: usize::MAX,
};

/// A grant value, or a part of one, as ral prints a value.
pub fn printed(value: &FOValue) -> String {
    pretty_print(value, 0, &PRINT)
}

/// As [`printed`]: `` `dir `absolute '/opt/bin' ``.
impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&printed(&target(self.clone())))
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

/// A table's lists, an empty one left out, as the decoder reads an omitted
/// list; a table listing nothing is written by its first, `[allow: []]` or
/// `[read: []]`, as `grant` writes the empty table.
fn table<const N: usize>(lists: [(&str, FOValue); N]) -> FOValue {
    let first = lists[0].0;
    let listed: Vec<_> = lists
        .into_iter()
        .filter(|(_, list)| list.as_list() != Some(&[]))
        .collect();
    if listed.is_empty() {
        record([(first, Vec::<FOValue>::new().encode())])
    } else {
        record(listed)
    }
}

fn list<T>(items: impl IntoIterator<Item = T>, encode: impl Fn(T) -> FOValue) -> FOValue {
    items.into_iter().map(encode).collect::<Vec<_>>().encode()
}

fn nullary(word: impl Into<&'static str>) -> FOValue {
    tag(word.into(), None)
}

fn place(place: &Place) -> FOValue {
    tag(&place.anchor.tag(), Some(place.payload.clone().encode()))
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
    use crate::authority::grant::freeze::{Anchor, FreezeCtx};
    use crate::authority::policy::{Flag, FsOp, GrantLayer};
    use crate::shell::grant::{decode_fragment, decode_grant};
    use crate::test_helper::Tree;
    use strum::VariantArray;

    /// Every axis the encoder writes, the decoder reads: anchored places
    /// resolve against the decoding host's own anchor.
    #[test]
    fn every_axis_written_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = Tree::lay(tmp.path(), ["/work/", "/bin/tool"]);
        let cwd = crate::path::CwdPath::new(tree.path("/work")).unwrap();
        let ctx = FreezeCtx::at(crate::host::EnvVars::new(), cwd);
        let tool = Anchor::Absolute.place(tree.at("/bin/tool"));
        let value = GrantValue::from(Axes {
            exec: Some(ExecTable {
                allow: vec![Target::File(tool), Target::Name("ral-no-such-tool".into())],
                deny: vec![Target::Dir(Anchor::Cwd.place("."))],
            }),
            fs: Some(FsTable {
                read: vec![Anchor::Cwd.place(".")],
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
        let reads = layer.fs().expect("an fs axis").table(FsOp::Read);
        assert_eq!(reads.allows().count(), 1);
        let stack = crate::test_helper::stack([layer]);
        assert_eq!(stack.net(), Some(Net::Offline));
        assert_eq!(stack.gpu(), Some(Gpu::Withheld));
        assert!(stack.permits(Flag::Editor(EditorVerb::Read)));
        assert!(!stack.permits(Flag::Editor(EditorVerb::Write)));
        assert!(!stack.permits(Flag::Shell(ShellVerb::Chdir)));
    }

    /// Programs, directories and, on Unix, a link, under one root, and an
    /// anchor in it: `cwd`, `home` and `scratch` there, the `PATH` its `bin`.
    struct Host {
        _tmp: tempfile::TempDir,
        tree: Tree,
        ctx: FreezeCtx,
    }

    impl Host {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let exe = std::env::consts::EXE_SUFFIX;
            let programs = ["tool", "other"].map(|name| format!("/bin/{name}{exe}"));
            let dirs = ["/work/src/", "/home/", "/scratch/", "/opt/sub/"];
            let tree = Tree::lay(tmp.path(), programs.iter().map(String::as_str).chain(dirs));
            std::fs::write(tree.path("/work/notes"), "").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for program in &programs {
                    let mode = std::fs::Permissions::from_mode(0o755);
                    std::fs::set_permissions(tree.path(program), mode).unwrap();
                }
                std::os::unix::fs::symlink(tree.path("/opt"), tree.path("/link")).unwrap();
            }
            let (home, path) = (tree.at("/home"), tree.at("/bin"));
            let env = [("HOME", home.as_str()), ("PATH", path.as_str())];
            let cwd = crate::path::CwdPath::new(tree.path("/work")).unwrap();
            let ctx = FreezeCtx::at(env.into_iter().collect(), cwd)
                .with_scratch(Some(tree.at("/scratch")));
            Self {
                _tmp: tmp,
                tree,
                ctx,
            }
        }

        /// Directories, the root first, which no exclusion names.
        fn dirs(&self) -> Vec<Place> {
            let absolute = |p: &str| Anchor::Absolute.place(self.tree.at(p));
            let mut dirs = vec![
                Anchor::Absolute.place("/"),
                absolute("/opt"),
                absolute("/opt/sub"),
                absolute("/WORK"),
                absolute("/absent/dir"),
                Anchor::Cwd.place("."),
                Anchor::Cwd.place("src"),
                Anchor::Home.place("."),
                Anchor::Scratch.place("."),
            ];
            if cfg!(unix) {
                dirs.push(absolute("/link"));
            }
            dirs
        }

        fn files(&self) -> Vec<Place> {
            let tool = format!("/bin/tool{}", std::env::consts::EXE_SUFFIX);
            vec![
                Anchor::Absolute.place(self.tree.at(&tool)),
                Anchor::Absolute.place(self.tree.at("/work/absent")),
                Anchor::Cwd.place("notes"),
            ]
        }

        fn decode(&self, value: &FOValue) -> GrantLayer {
            decode_grant(value, "property", &self.ctx)
                .unwrap_or_else(|e| panic!("{value:?} decodes: {e}"))
        }

        /// What `value` is the value of: its grant, extended by its fragment.
        fn rebuilt(&self, value: &LayerValue) -> GrantLayer {
            let grant = self.decode(&value.grant);
            match &value.fragment {
                None => grant,
                Some(fragment) => {
                    grant.extend(decode_fragment(fragment, "property", &self.ctx).unwrap())
                }
            }
        }

        /// A grant drawn from this host's places and names, a name spelled
        /// apart from its folded key among them.
        fn axes(&self, rng: &mut fastrand::Rng) -> Axes {
            let some = |rng: &mut fastrand::Rng| rng.usize(..3) != 0;
            let draw = |rng: &mut fastrand::Rng, items: Vec<Place>| -> Vec<Place> {
                items.into_iter().filter(|_| rng.usize(..3) == 0).collect()
            };
            let (dirs, files) = (self.dirs(), self.files());
            let unrooted = || dirs[1..].iter().chain(&files).cloned().collect::<Vec<_>>();
            let targets = |rng: &mut fastrand::Rng, dirs: &[Place]| {
                let names = ["tool", "Tool", "OTHER", "ls", "ral-no-such-tool"];
                let names = names.into_iter().filter(|_| rng.usize(..3) == 0);
                let names: Vec<_> = names.map(|n| Target::Name(n.into())).collect();
                let files = draw(rng, files.clone()).into_iter().map(Target::File);
                let dirs = draw(rng, dirs.to_vec()).into_iter().map(Target::Dir);
                names.into_iter().chain(files).chain(dirs).collect()
            };
            Axes {
                exec: some(rng).then(|| ExecTable {
                    allow: targets(rng, &dirs),
                    deny: targets(rng, &dirs[1..]),
                }),
                fs: some(rng).then(|| FsTable {
                    read: draw(rng, dirs.iter().chain(&files).cloned().collect()),
                    write: draw(rng, dirs.iter().chain(&files).cloned().collect()),
                    deny: draw(rng, unrooted()),
                }),
                net: rng.bool().then_some(Net::Offline),
                gpu: rng.bool().then_some(Gpu::Withheld),
                editor: some(rng).then(|| subset(rng, EditorVerb::VARIANTS)),
                shell: some(rng).then(|| subset(rng, ShellVerb::VARIANTS)),
            }
        }
    }

    fn subset<V: Copy + Ord>(rng: &mut fastrand::Rng, all: &[V]) -> BTreeSet<V> {
        all.iter().copied().filter(|_| rng.bool()).collect()
    }

    /// `grant` with every list its tables leave out written out empty.
    fn explicit(grant: &FOValue) -> FOValue {
        let FOValue::Map { entries } = grant else {
            panic!("a grant is a record: {grant:?}");
        };
        let lists = |axis: &str| match axis {
            "exec" => &["allow", "deny"][..],
            "fs" => &["read", "write", "deny"],
            _ => &[],
        };
        let entries = entries.iter().map(|(axis, value)| match value {
            FOValue::Map { entries: table } => {
                let missing = (lists(axis).iter())
                    .filter(|key| !table.iter().any(|(k, _)| k == *key))
                    .map(|key| ((*key).to_owned(), FOValue::List { items: Vec::new() }));
                let table = table.iter().cloned().chain(missing).collect();
                (axis.clone(), FOValue::Map { entries: table })
            }
            value => (axis.clone(), value.clone()),
        });
        FOValue::Map {
            entries: entries.collect(),
        }
    }

    /// `decode(value(l)) == l` for every decoded layer `l`, and for every
    /// extension of one by a decoded fragment: an anchored place is read
    /// back as the absolute spelling it froze to, on the host that froze it.
    /// A list the encoder leaves out decodes as the empty list it stands for.
    #[test]
    fn a_layers_value_decodes_back_to_it() {
        let host = Host::new();
        let mut rng = fastrand::Rng::with_seed(0x6772_616e);
        for _ in 0..200 {
            let (base, extension) = (host.axes(&mut rng), host.axes(&mut rng));
            let written = GrantValue::from(base.clone());
            let layer = host.decode(written.as_ref());
            let omitted = "a list left out is read as an empty one";
            assert_eq!(host.decode(&explicit(written.as_ref())), layer, "{omitted}");
            let value = layer.value();
            assert_eq!(value.fragment, None, "a decoded layer is a grant: {base:?}");
            assert_eq!(host.rebuilt(&value), layer, "{base:?}");
            assert_eq!(host.decode(&explicit(&value.grant)), layer, "{omitted}");

            let fragment = GrantValue::from(extension.clone());
            let fragment = decode_fragment(fragment.as_ref(), "property", &host.ctx).unwrap();
            let extended = layer.extend(fragment);
            assert_eq!(
                host.rebuilt(&extended.value()),
                extended,
                "{base:?} extended by {extension:?}"
            );
        }
    }

    /// Every program has no grant spelling: an extension of an omitted exec
    /// shows as the grant of the rest, extended by the fragment as written.
    #[test]
    fn every_program_shows_as_the_fragment_that_extends_to_it() {
        let host = Host::new();
        let carve = |axes: Axes| {
            let fragment = GrantValue::from(axes);
            let fragment = decode_fragment(fragment.as_ref(), "carve", &host.ctx).unwrap();
            GrantLayer::root().extend(fragment)
        };
        let data = |axes: Axes| FOValue::from(GrantValue::from(axes));
        let tool = format!("/bin/tool{}", std::env::consts::EXE_SUFFIX);
        let tool = Anchor::Absolute.place(crate::path::shown(host.tree.path(&tool)));
        let exec = Axes {
            exec: Some(ExecTable {
                deny: vec![Target::File(tool)],
                ..ExecTable::default()
            }),
            ..Axes::default()
        };
        assert_eq!(
            carve(exec.clone()).value(),
            LayerValue {
                grant: data(Axes::default()),
                fragment: Some(data(exec)),
            }
        );
        let reads = carve(Axes {
            fs: Some(FsTable {
                deny: vec![Anchor::Cwd.place("notes")],
                ..FsTable::default()
            }),
            ..Axes::default()
        });
        assert_eq!(
            reads.value().fragment,
            None,
            "everywhere less a path is spelled"
        );
        assert_eq!(host.rebuilt(&reads.value()), reads);
    }
}
