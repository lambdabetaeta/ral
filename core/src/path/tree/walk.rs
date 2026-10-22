//! The one walk of a directory tree: ral enumerates it, and `ignore` only
//! matches.
//!
//! A walk lists with [`Dir::read_dir`] and descends with [`Dir::open_dir`],
//! depth first in name order, and polls the cancel scope at each entry.  The
//! root comes first and is filtered by nothing: a hidden or ignored root asked
//! for by name is searched, and a root that is a link is entered through it.
//!
//! A walk is told which region it may read ([`Walk::readable_by`]) and judges
//! every directory it enters, every file it reads for its own guidance and
//! every entry it yields against it, so it cannot forget to ask.  It exposes
//! no trace: a caller that reads an entry locates it by [`WalkEntry::shown`]
//! and is judged again on the walk that makes.

use super::ignore::{self, Files, Layer, Rules};
use crate::capability::ReadRegion;
use crate::path::sigil::literal_relative;
use crate::path::{
    CwdPath, Dir, EntryName, Judged, Kind, LeafMode, LexicalPath, Listing, Located, ObjectId,
    Reading, Stat, git, shown, walk, walk_dir,
};
use crate::process::Scope;
use crate::types::{Settled, Shell, sig};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Component, Path};

type Keep = Box<dyn Fn(&LexicalPath) -> bool + Send + Sync>;

/// A tree walk, as far as it is told what to skip.
pub struct Walk {
    root: LexicalPath,
    ignores: bool,
    max_depth: Option<usize>,
    keep: Option<Keep>,
    same_file_system: bool,
    follow_links: bool,
}

impl Walk {
    fn new(root: &LexicalPath, ignores: bool) -> Self {
        Self {
            root: root.clone(),
            ignores,
            max_depth: None,
            keep: None,
            same_file_system: false,
            follow_links: false,
        }
    }

    /// Every entry beneath `root` but those the ignore rules match and the
    /// hidden that no rule matches: a `!.visible` whitelist admits a dotfile.
    /// `root` itself always.
    pub fn respecting_ignores(root: &LexicalPath) -> Self {
        Self::new(root, true)
    }

    /// Every entry beneath `root`.
    pub fn all(root: &LexicalPath) -> Self {
        Self::new(root, false)
    }

    /// Enter no directory deeper than `max`.
    #[must_use]
    pub fn max_depth(mut self, max: Option<usize>) -> Self {
        self.max_depth = max;
        self
    }

    /// Yield only the entries `keep` admits by their `shown` name, and enter
    /// nothing beneath the rest.
    #[must_use]
    pub fn filter_entry(
        mut self,
        keep: impl Fn(&LexicalPath) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.keep = Some(Box::new(keep));
        self
    }

    /// Enter no mount beneath the root: no `/proc`, `/sys`, or `/dev`.
    #[must_use]
    pub fn same_file_system(mut self) -> Self {
        self.same_file_system = true;
        self
    }

    /// Enter a link that names a directory, as the directory.  A directory
    /// already on the way down is not entered again.
    #[must_use]
    pub fn follow_links(mut self) -> Self {
        self.follow_links = true;
        self
    }

    /// The walk, judged by `region`.
    pub fn readable_by(self, region: ReadRegion) -> Granted {
        Granted { walk: self, region }
    }

    /// The entries the live grant admits.  `named` is the root as the caller
    /// wrote it: a cwd-relative name gives cwd-relative entries where it can,
    /// any other absolute ones.  `who` heads a refused name.
    pub fn admitted(self, shell: &Shell, who: &'static str, named: &str) -> Admitted {
        let base = (shell.context.resolver().is_relative(named))
            .then(|| shell.resolve(".").into_cwd().ok())
            .flatten();
        let region = shell.context.grants.read_region(shell.floor());
        Admitted {
            entries: self.readable_by(region).entries(shell.scope.clone()),
            base,
            who,
        }
    }
}

/// A [`Walk`] and the region it may read.
pub struct Granted {
    walk: Walk,
    region: ReadRegion,
}

impl Granted {
    /// The walk's entries, root first.  Each is settled against `scope`
    /// before it is yielded, inside a listing too.
    pub fn entries(self, scope: Scope) -> Entries {
        Entries {
            walk: self.walk,
            region: self.region,
            scope,
            started: false,
            root_id: None,
            rules: None,
            stack: Vec::new(),
        }
    }
}

/// An entry as the walk reached it.
#[derive(Debug)]
pub struct WalkEntry {
    /// The entry's name as the walk reached it: the root as given, then each
    /// entry's name.  No link in it is resolved: an entry beneath a followed
    /// link is named through the link.
    pub shown: LexicalPath,
    pub depth: usize,
    pub found: Found,
}

/// What the walk found of an entry.
#[derive(Debug)]
pub enum Found {
    /// Stated, and a directory in this state was entered.
    Seen(Stat),
    /// Listed, then unstatable: gone (`NotFound`) or unreadable; neither is
    /// absence.  The root, which no listing lists, is unseen where it cannot
    /// be walked or the region refuses it.
    Unseen(io::Error),
    /// A directory stated but not entered, with why.
    Unentered { stat: Stat, why: Unentered },
}

/// Why a directory was not entered.  A pruning is not an error.
#[derive(Debug)]
pub enum Unentered {
    /// The region refused it, or the open did.
    Refused(io::Error),
    /// `max_depth`, `same_file_system`, or a cycle under `follow_links`.
    Pruned,
}

impl WalkEntry {
    /// Whether the entry was seen, and a file.
    pub fn is_file(&self) -> bool {
        matches!(&self.found, Found::Seen(stat) if stat.kind == Kind::File)
    }

    /// The name a reader takes back unchanged: relative to `base` where the
    /// walk was asked with one, else absolute.
    ///
    /// # Errors
    /// A name no ral string can hold.
    pub fn name(&self, base: Option<&CwdPath>) -> Settled<String> {
        self.named(base).map_err(sig)
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "path-form: a walked name taken relative to the cwd the caller named"
    )]
    fn named(&self, base: Option<&CwdPath>) -> Result<String, String> {
        let path = self.shown.as_path();
        let rel = base
            .and_then(|base| path.strip_prefix(base).ok())
            .unwrap_or(path);
        #[allow(
            clippy::disallowed_methods,
            reason = "native: the refusal of a name no ral string can hold"
        )]
        let text = rel.to_str();
        text.map(|_| literal_relative(&shown(rel)))
            .ok_or_else(|| unnameable(rel))
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "path-form: the longest UTF-8 spelling above a name no ral string can hold, to say where it stops"
)]
#[allow(
    clippy::unnecessary_debug_formatting,
    reason = "Debug escapes the invalid bytes; Display would mangle them"
)]
fn unnameable(shown: &Path) -> String {
    #[allow(
        clippy::disallowed_methods,
        reason = "native: the longest ancestor that is UTF-8, to say where the name stops being"
    )]
    let dir = shown.ancestors().find_map(Path::to_str).unwrap_or("");
    let name = shown.strip_prefix(dir).unwrap_or(shown);
    format!("{dir}: {name:?} is not valid UTF-8, so no ral string can hold it; rename it?")
}

/// The entries of an [`Walk::admitted`] walk the walk saw, which the grant
/// judged on the way.
pub struct Admitted {
    entries: Entries,
    base: Option<CwdPath>,
    who: &'static str,
}

impl Admitted {
    /// The next entry seen.
    ///
    /// # Errors
    /// The stop in force.
    #[allow(
        clippy::should_implement_trait,
        reason = "a stop is an error that ends the walk, not an item"
    )]
    pub fn next(&mut self) -> Settled<Option<WalkEntry>> {
        for entry in self.entries.by_ref() {
            let entry = entry?;
            if !matches!(entry.found, Found::Unseen(_)) {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    /// `entry`'s name as a reader takes it back, in the shape the root was
    /// named.
    ///
    /// # Errors
    /// A name no ral string can hold, headed by what the walk was for.
    pub fn name(&self, entry: &WalkEntry) -> Settled<String> {
        (entry.named(self.base.as_ref())).map_err(|e| sig(format!("{}: {e}", self.who)))
    }
}

/// A directory entered, and what is left of it to yield.
struct Frame {
    dir: Dir<Judged<Reading>>,
    id: ObjectId,
    shown: LexicalPath,
    depth: usize,
    items: std::vec::IntoIter<Item>,
}

/// An entry the region admitted, as its listing stated it.
struct Item {
    name: EntryName,
    seen: io::Result<(Stat, ObjectId)>,
}

/// Which of the files the walk reads a directory lists.
#[derive(Default)]
struct Marks {
    ignore: bool,
    gitignore: bool,
    git: bool,
}

/// The entries of a walk: a depth-first, name-ordered iteration.
pub struct Entries {
    walk: Walk,
    region: ReadRegion,
    scope: Scope,
    started: bool,
    root_id: Option<ObjectId>,
    rules: Option<Rules>,
    stack: Vec<Frame>,
}

impl Iterator for Entries {
    type Item = Settled<WalkEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let next = if self.started {
            self.advance().transpose()
        } else {
            self.started = true;
            Some(self.root())
        };
        if matches!(next, Some(Err(_))) {
            self.stack.clear();
        }
        next
    }
}

impl Entries {
    /// The next entry beneath the root.
    fn advance(&mut self) -> Settled<Option<WalkEntry>> {
        while let Some(frame) = self.stack.last_mut() {
            let Some(item) = frame.items.next() else {
                self.stack.pop();
                if let Some(rules) = &mut self.rules {
                    rules.leave();
                }
                continue;
            };
            self.scope.poll()?;
            if let Some(entry) = self.step(item)? {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    /// The root: filtered by nothing, entered through a link.
    fn root(&mut self) -> Settled<WalkEntry> {
        self.scope.poll()?;
        let root = self.walk.root.clone();
        let found = match walk_dir(&root) {
            Ok(dir) => self.enter_root(dir)?,
            Err(why) => self.shallow_root(&root, why),
        };
        Ok(WalkEntry {
            shown: root,
            depth: 0,
            found,
        })
    }

    fn enter_root(&mut self, dir: Dir) -> Settled<Found> {
        let Ok(dir) = self.region.judge(dir) else {
            return Ok(Found::Unseen(denied()));
        };
        let stat = dir.stat().clone();
        if self.walk.max_depth == Some(0) {
            return Ok(pruned(stat));
        }
        self.root_id = Some(dir.trace().last_id());
        if self.walk.ignores {
            self.rules = Some(self.ancestry(&dir));
        }
        let shown = self.walk.root.clone();
        Ok(match self.enter(dir, shown, 0, None)? {
            Ok(()) => Found::Seen(stat),
            Err(e) => refused(stat, e),
        })
    }

    /// A root that could not be entered: a file, a link to nothing, or a
    /// directory the walk could not enter, with `why`.
    fn shallow_root(&self, root: &LexicalPath, why: io::Error) -> Found {
        let followed = walk(root, LeafMode::Follow);
        // A followed walk that reaches nothing — an absent leaf, or `NotFound`
        // on the way — is retried without following: a link to nothing is the entry.
        let reached = match &followed {
            Ok(located) => located.trace().kind().is_some(),
            Err(e) => e.kind() != io::ErrorKind::NotFound,
        };
        let located = if reached {
            followed
        } else {
            walk(root, LeafMode::NoFollow).or(followed)
        };
        let judged =
            located.and_then(|located| self.region.judge_file(located).map_err(|_| denied()));
        match judged {
            Ok(located) => match located.stat().cloned() {
                None => Found::Unseen(io::ErrorKind::NotFound.into()),
                Some(stat) if stat.kind == Kind::Dir => refused(stat, why),
                Some(stat) => Found::Seen(stat),
            },
            Err(e) => Found::Unseen(e),
        }
    }

    /// One entry of the directory on top, yielded unless something skips it.
    fn step(&mut self, item: Item) -> Settled<Option<WalkEntry>> {
        let Item { name, seen } = item;
        let top = self.stack.last().expect("an entry belongs to a directory");
        let (shown, depth) = (child(&top.shown, &name), top.depth + 1);
        let (stat, id) = match seen {
            Ok(seen) => seen,
            Err(e) => {
                let dropped = self.skips(&name, false, hidden(&name, None), &shown);
                return Ok((!dropped).then_some(WalkEntry {
                    shown,
                    depth,
                    found: Found::Unseen(e),
                }));
            }
        };
        let enterable = self.walk.max_depth.is_none_or(|max| depth < max);
        let linked = (self.walk.follow_links && enterable && stat.kind == Kind::Symlink)
            .then(|| self.follow(&name));
        let is_dir = stat.kind == Kind::Dir || matches!(linked, Some(Ok(_)));
        if self.skips(&name, is_dir, hidden(&name, Some(&stat)), &shown) {
            return Ok(None);
        }
        let found = match (is_dir, linked) {
            (false, Some(Err(e))) if e.kind() == io::ErrorKind::PermissionDenied => {
                refused(stat, e)
            }
            (false, _) => Found::Seen(stat),
            (true, _) if !enterable => pruned(stat),
            (true, linked) => self.descend(&name, stat, id, linked, &shown, depth)?,
        };
        Ok(Some(WalkEntry {
            shown,
            depth,
            found,
        }))
    }

    /// The directory the link `name` of the one on top names, its target
    /// walked from that directory and through the directories the walk opened
    /// above it.
    fn follow(&self, name: &EntryName) -> io::Result<Dir> {
        self.stack
            .last()
            .expect("a link belongs to a directory")
            .dir
            .open_dir_via_link(name)
    }

    /// A directory not pruned by depth: entered, or why not.
    fn descend(
        &mut self,
        name: &EntryName,
        mut stat: Stat,
        id: ObjectId,
        linked: Option<io::Result<Dir>>,
        shown: &LexicalPath,
        depth: usize,
    ) -> Settled<Found> {
        let beyond = |id: ObjectId| {
            let elsewhere = self.walk.same_file_system
                && self.root_id.is_some_and(|root| !root.same_volume(id));
            let cycle = self.walk.follow_links && self.stack.iter().any(|frame| frame.id == id);
            elsewhere || cycle
        };
        let opened = match linked {
            Some(opened) => opened,
            None if beyond(id) => return Ok(pruned(stat)),
            None => self.stack.last().map_or_else(
                || Err(io::ErrorKind::NotFound.into()),
                |top| top.dir.open_dir(name),
            ),
        };
        let dir = match opened {
            Ok(dir) if beyond(dir.trace().last_id()) => return Ok(pruned(stat)),
            Ok(dir) => dir,
            Err(e) => return Ok(refused(stat, e)),
        };
        let Ok(dir) = self.region.judge(dir) else {
            return Ok(refused(stat, denied()));
        };
        if stat.kind == Kind::Symlink {
            stat = dir.stat().clone();
        }
        Ok(
            match self.enter(dir, shown.clone(), depth, Some(name.as_os_str()))? {
                Ok(()) => Found::Seen(stat),
                Err(e) => refused(stat, e),
            },
        )
    }

    /// Whether an ignore rule or the filter drops the entry.
    fn skips(&self, name: &EntryName, is_dir: bool, hidden: bool, shown: &LexicalPath) -> bool {
        (self.rules.as_ref()).is_some_and(|rules| rules.skips(name.as_os_str(), is_dir, hidden))
            || self.walk.keep.as_ref().is_some_and(|keep| !keep(shown))
    }

    /// List `dir`, take its ignore files, and make it the directory on top.
    /// The listing is judged entry by entry and stated as it goes, the scope
    /// polled between entries: what the region admits is what is left to
    /// yield.
    ///
    /// # Errors
    /// The scope's stop; the listing's own failure, inside.
    fn enter(
        &mut self,
        dir: Dir<Judged<Reading>>,
        shown: LexicalPath,
        depth: usize,
        name: Option<&OsStr>,
    ) -> Settled<io::Result<()>> {
        let (items, marks) = match self.list(&dir)? {
            Ok(listed) => listed,
            Err(e) => return Ok(Err(e)),
        };
        let layer =
            (self.rules.as_ref()).map(|rules| self.layer(rules, &dir, &marks, name.is_some()));
        if let (Some(rules), Some(layer)) = (&mut self.rules, layer) {
            match name {
                Some(name) => rules.enter(name, layer),
                None => rules.push(layer),
            }
        }
        self.stack.push(Frame {
            id: dir.trace().last_id(),
            dir,
            shown,
            depth,
            items: items.into_iter(),
        });
        Ok(Ok(()))
    }

    fn list(&self, dir: &Dir<Judged<Reading>>) -> Settled<io::Result<(Vec<Item>, Marks)>> {
        match dir.read_dir() {
            Ok(listing) => self.admit(listing).map(Ok),
            Err(e) => Ok(Err(e)),
        }
    }

    fn admit(&self, listing: Listing<'_>) -> Settled<(Vec<Item>, Marks)> {
        let (mut items, mut marks) = (Vec::new(), Marks::default());
        for entry in listing {
            self.scope.poll()?;
            match entry.name().as_os_str().as_encoded_bytes() {
                b".ignore" => marks.ignore = true,
                b".gitignore" => marks.gitignore = true,
                b".git" => marks.git = true,
                _ => {}
            }
            if self.region.admits(&entry) {
                let seen = match entry.seen() {
                    Ok(seen) => Ok((seen.stat.clone(), seen.id)),
                    Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
                };
                items.push(Item {
                    name: entry.name().clone(),
                    seen,
                });
            }
        }
        Ok((items, marks))
    }

    /// The rules the entered directory `dir` adds to those above it.
    fn layer(
        &self,
        rules: &Rules,
        dir: &Dir<Judged<Reading>>,
        marks: &Marks,
        named: bool,
    ) -> Layer {
        let dot_git = marks.git.then(|| self.dot_git_in(dir)).flatten();
        let within = dot_git.is_some() || rules.in_repository();
        let depth = rules.depth() + usize::from(named);
        let read = |name: &str| {
            let name = EntryName::parse(OsStr::new(name)).ok()?;
            let found = self
                .region
                .judge_file(dir.locate(&name, LeafMode::Follow).ok()?);
            slurp(&found.ok()?).ok().map(ignore::text)
        };
        self.assemble(depth, marks, dot_git.as_ref(), within, read)
    }

    /// The rules the directory `real`, above the root, adds, `dot_git` its
    /// `.git`: read by name, since no handle on it is held.
    fn above(&self, real: &Path, dot_git: Option<&(LexicalPath, bool)>, within: bool) -> Layer {
        let marks = Marks {
            ignore: true,
            gitignore: true,
            git: true,
        };
        let read = |name: &str| {
            let at = LexicalPath::absolute(&real.join(name))?;
            self.read_named(&at).ok().map(ignore::text)
        };
        self.assemble(normals(real).len(), &marks, dot_git, within, read)
    }

    fn assemble(
        &self,
        depth: usize,
        marks: &Marks,
        dot_git: Option<&(LexicalPath, bool)>,
        within: bool,
        read: impl Fn(&str) -> Option<String>,
    ) -> Layer {
        let ignore = marks.ignore.then(|| read(".ignore")).flatten();
        let gitignore = (marks.gitignore && within)
            .then(|| read(".gitignore"))
            .flatten();
        let exclude = dot_git.and_then(|(at, is_dir)| {
            let file = git::info_exclude(at.as_path(), *is_dir, |p| self.read_text(p))?;
            self.read_named(&file).ok().map(ignore::text)
        });
        let files = Files {
            ignore: ignore.as_deref(),
            gitignore: gitignore.as_deref(),
            exclude: exclude.as_deref(),
        };
        Layer::new(depth, dot_git.is_some(), &files)
    }

    /// The rules of every directory above the root, and the names that lead
    /// to it: `.ignore` of each up to the filesystem root, and `.gitignore`
    /// of those within the nearest repository.
    #[allow(
        clippy::disallowed_methods,
        reason = "path-form: the spellings above a root already walked, whose ignore files discovery reads"
    )]
    fn ancestry(&self, root: &Dir<Judged<Reading>>) -> Rules {
        let real = root.real();
        let mut rules = Rules::new(normals(real.as_path()));
        let above: Vec<(&Path, Option<(LexicalPath, bool)>)> = real
            .as_path()
            .ancestors()
            .skip(1)
            .map(|at| (at, self.dot_git_at(at)))
            .collect();
        let nearest = above.iter().position(|(_, dot_git)| dot_git.is_some());
        for (i, (at, dot_git)) in above.iter().enumerate().rev() {
            rules.push(self.above(at, dot_git.as_ref(), nearest.is_some_and(|n| i <= n)));
        }
        rules
    }

    /// The `.git` of the entered directory `dir`, where one is there and the
    /// region reads it: its spelling, and whether it is a directory.
    fn dot_git_in(&self, dir: &Dir<Judged<Reading>>) -> Option<(LexicalPath, bool)> {
        let name = EntryName::parse(OsStr::new(".git")).ok()?;
        let found = self
            .region
            .judge_file(dir.locate(&name, LeafMode::Follow).ok()?);
        let at = LexicalPath::absolute(&dir.real().as_path().join(".git"))?;
        Some((at, found.ok()?.kind()? == Kind::Dir))
    }

    /// [`dot_git_in`](Self::dot_git_in) of a directory held by name.
    fn dot_git_at(&self, real: &Path) -> Option<(LexicalPath, bool)> {
        let at = LexicalPath::absolute(&real.join(".git"))?;
        let found = self.region.judge_file(walk(&at, LeafMode::Follow).ok()?);
        let is_dir = found.ok()?.kind()? == Kind::Dir;
        Some((at, is_dir))
    }

    /// The file at `path`, walked and judged: a file the region refuses is
    /// not read.
    fn read_named(&self, path: &LexicalPath) -> io::Result<Vec<u8>> {
        let located = walk(path, LeafMode::Follow)?;
        slurp(&self.region.judge_file(located).map_err(|_| denied())?)
    }

    /// [`read_named`](Self::read_named) as the text git discovery reads.
    fn read_text(&self, path: &LexicalPath) -> io::Result<String> {
        String::from_utf8(self.read_named(path)?).map_err(io::Error::other)
    }
}

fn slurp(file: &Located<Judged<Reading>>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.read()?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The names of `path` beneath its root.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: the names of a root already walked, for ignore discovery"
)]
fn normals(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_owned()),
            _ => None,
        })
        .collect()
}

/// `parent`'s entry `name`, as the walk names it.
fn child(parent: &LexicalPath, name: &EntryName) -> LexicalPath {
    LexicalPath::from_lexed(parent.as_path().join(name.as_os_str()))
}

/// Hidden: a leading dot, and on Windows the hidden attribute.
fn hidden(name: &EntryName, stat: Option<&Stat>) -> bool {
    #[cfg(windows)]
    if stat.is_some_and(|stat| stat.hidden) {
        return true;
    }
    #[cfg(not(windows))]
    let _ = stat;
    name.as_os_str().as_encoded_bytes().starts_with(b".")
}

fn denied() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
}

fn refused(stat: Stat, e: io::Error) -> Found {
    Found::Unentered {
        stat,
        why: Unentered::Refused(e),
    }
}

fn pruned(stat: Stat) -> Found {
    Found::Unentered {
        stat,
        why: Unentered::Pruned,
    }
}

#[cfg(test)]
mod differential;
#[cfg(test)]
mod tests;
