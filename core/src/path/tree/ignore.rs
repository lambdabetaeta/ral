//! The ignore rules of a tree walk: which of an entry's ancestors' files apply
//! to it, and in what order.  Nothing here opens anything: the walk hands over
//! each file's text and asks about each entry.
//!
//! Three classes of file, as `ignore` has them.  `.ignore` crosses every
//! repository boundary; `.gitignore` and `info/exclude` apply only within the
//! nearest enclosing repository.  The first class that matches decides, across
//! every depth: the deepest `.ignore` on the way to the entry, failing any the
//! deepest `.gitignore`, failing any `info/exclude`; within one file the last
//! matching pattern wins.

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// What the first lines of `bytes` say, up to the first that is not UTF-8:
/// `ignore` reads a file line by line and stops there.
pub(super) fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| {
        let valid = e.utf8_error().valid_up_to();
        let whole = e.as_bytes()[..valid].iter().rposition(|&b| b == b'\n');
        let mut bytes = e.into_bytes();
        bytes.truncate(whole.map_or(0, |i| i + 1));
        String::from_utf8(bytes).unwrap_or_default()
    })
}

/// One file's patterns, each line fed to the matcher.  A pattern `ignore`
/// cannot compile is skipped, as it skips it; the first line's BOM is
/// stripped, as git does.
fn compile(text: Option<&str>) -> Gitignore {
    let Some(text) = text else {
        return Gitignore::empty();
    };
    let mut builder = GitignoreBuilder::new(".");
    for (i, line) in text.lines().enumerate() {
        let line = if i == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        let _ = builder.add_line(None, line);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// The files of one directory: its text, where it has any.
#[derive(Default)]
pub(super) struct Files<'a> {
    pub ignore: Option<&'a str>,
    pub gitignore: Option<&'a str>,
    pub exclude: Option<&'a str>,
}

/// The rules one directory contributes: a matcher per file, and whether the
/// directory is a repository.
pub(super) struct Layer {
    /// How many names lead to the directory, from the filesystem root.
    depth: usize,
    repository: bool,
    ignore: Gitignore,
    gitignore: Gitignore,
    exclude: Gitignore,
}

impl Layer {
    pub(super) fn new(depth: usize, repository: bool, files: &Files<'_>) -> Self {
        Self {
            depth,
            repository,
            ignore: compile(files.ignore),
            gitignore: compile(files.gitignore),
            exclude: compile(files.exclude),
        }
    }

    pub(super) fn is_repository(&self) -> bool {
        self.repository
    }
}

/// The layers on the way to the directory being walked, outermost first, and
/// the names that lead there.
pub(super) struct Rules {
    names: Vec<OsString>,
    base: usize,
    layers: Vec<Layer>,
}

impl Rules {
    /// Rules for a walk whose root is `names` below the filesystem root.
    pub(super) fn new(names: Vec<OsString>) -> Self {
        Self {
            base: names.len(),
            names,
            layers: Vec::new(),
        }
    }

    /// How many names lead to the directory being walked.
    pub(super) fn depth(&self) -> usize {
        self.names.len()
    }

    /// Whether a repository encloses the directory being walked.
    pub(super) fn in_repository(&self) -> bool {
        self.layers.iter().any(Layer::is_repository)
    }

    /// A directory above the root, or the root, whose own name is already
    /// among the names.
    pub(super) fn push(&mut self, layer: Layer) {
        self.layers.push(layer);
    }

    /// The directory `name` of the one being walked.
    pub(super) fn enter(&mut self, name: &OsStr, layer: Layer) {
        self.names.push(name.to_owned());
        self.layers.push(layer);
    }

    /// Back out of the directory last entered.
    pub(super) fn leave(&mut self) {
        self.layers.pop();
        if self.names.len() > self.base {
            self.names.pop();
        }
    }

    /// Whether the entry `name` of the directory being walked is skipped:
    /// matched by an ignore rule, or hidden and matched by none.
    pub(super) fn skips(&self, name: &OsStr, is_dir: bool, hidden: bool) -> bool {
        let in_repository = self.in_repository();
        let (mut ignore, mut gitignore, mut exclude) = (Match::None, Match::None, Match::None);
        let mut bounded = false;
        for layer in self.layers.iter().rev() {
            let path = || -> PathBuf {
                (self.names[layer.depth..].iter().map(OsString::as_os_str))
                    .chain([name])
                    .collect()
            };
            if ignore.is_none() && !layer.ignore.is_empty() {
                ignore = layer.ignore.matched(path(), is_dir);
            }
            if in_repository && !bounded {
                if gitignore.is_none() && !layer.gitignore.is_empty() {
                    gitignore = layer.gitignore.matched(path(), is_dir);
                }
                if exclude.is_none() && !layer.exclude.is_empty() {
                    exclude = layer.exclude.matched(path(), is_dir);
                }
            }
            bounded |= layer.repository;
        }
        match ignore.or(gitignore).or(exclude) {
            Match::Ignore(_) => true,
            Match::Whitelist(_) => false,
            Match::None => hidden,
        }
    }
}
