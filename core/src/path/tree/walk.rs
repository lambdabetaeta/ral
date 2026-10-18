//! The one walk of a directory tree, polling the cancel scope at each entry.
//!
//! A walk is told which entries it may enter, and yields them in walk order.
//! An admitted walk also judges every entry against the live grant, by its
//! own path, and names it in the shape its root was named.
//!
//! A walk error is not an entry and is skipped: one unreadable directory
//! cannot blank a listing, and no error names a withheld path.

use crate::capability::FsOp;
use crate::path::LexicalPath;
use crate::path::sigil::literal_relative;
use crate::process::Scope;
use crate::types::{Settled, Shell, sig};
use ignore::WalkBuilder;
use std::fs::FileType;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub struct Walk(WalkBuilder);

impl Walk {
    /// Every entry beneath `root` but the hidden and the ignored; `root`
    /// itself always.
    pub fn tracked(root: &Path) -> Self {
        let mut walk = WalkBuilder::new(root);
        walk.git_global(false);
        Self(walk)
    }

    /// Every entry beneath `root`.
    pub fn everything(root: &Path) -> Self {
        let mut walk = WalkBuilder::new(root);
        walk.standard_filters(false);
        Self(walk)
    }

    #[must_use]
    pub fn depth(mut self, max: Option<usize>) -> Self {
        self.0.max_depth(max);
        self
    }

    #[must_use]
    pub fn following_links(mut self) -> Self {
        self.0.follow_links(true);
        self
    }

    /// Enter only the entries `keep` admits, and nothing beneath the rest.
    #[must_use]
    pub fn keeping(mut self, keep: impl Fn(&Path) -> bool + Send + Sync + 'static) -> Self {
        self.0.filter_entry(move |e| keep(e.path()));
        self
    }

    /// The entries, judged by no grant: for a host-side walk.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:tree-walk] The one directory walk: a listing's value is the list it returns; an admitted walk judges each entry against the live grant, and the walk polls the cancel scope per entry. Also stats an unfollowable entry to tell a dangling link from a vanished one."
    )]
    pub(crate) fn sightings(self, scope: Scope) -> impl Iterator<Item = Settled<Sighting>> {
        /// What a walk error leaves of its entry: a link the walker could
        /// not follow (dangling, or back to an ancestor) is still an entry; a
        /// vanished one is not.
        fn unfollowed(e: ignore::Error, depth: usize) -> Option<Sighting> {
            match e {
                ignore::Error::WithDepth { depth, err } => unfollowed(*err, depth),
                ignore::Error::Loop { child, .. } => Some(Sighting {
                    path: child,
                    depth,
                    kind: None,
                }),
                ignore::Error::WithPath { path, err }
                    if err
                        .io_error()
                        .is_some_and(|io| io.kind() == ErrorKind::NotFound) =>
                {
                    let kind = std::fs::symlink_metadata(&path).ok()?.file_type();
                    Some(Sighting {
                        path,
                        depth,
                        kind: Some(kind),
                    })
                }
                _ => None,
            }
        }

        self.0.build().filter_map(move |item| {
            if let Err(b) = scope.poll() {
                return Some(Err(b));
            }
            match item {
                Ok(e) => Some(Ok(Sighting {
                    depth: e.depth(),
                    kind: e.file_type(),
                    path: e.into_path(),
                })),
                Err(e) => unfollowed(e, 0).map(Ok),
            }
        })
    }

    /// The entries the live grant admits.  `named` is the root as the caller
    /// wrote it: a cwd-relative name gives cwd-relative entries where it can,
    /// any other absolute ones.  `who` heads a refused name.
    pub fn admitted(
        self,
        shell: &Shell,
        who: &'static str,
        named: &str,
    ) -> Admitted<impl Iterator<Item = Settled<Sighting>> + use<>> {
        let base = shell
            .context
            .resolver()
            .is_relative(named)
            .then(|| shell.resolve(".").into_inner());
        Admitted {
            sightings: self.sightings(shell.scope.clone()),
            base,
            who,
        }
    }
}

/// An entry as the walk found it.
pub struct Sighting {
    pub path: PathBuf,
    depth: usize,
    kind: Option<FileType>,
}

pub struct Admitted<I> {
    sightings: I,
    base: Option<PathBuf>,
    who: &'static str,
}

/// An admitted entry: `path` as the guards judged it.
pub struct Entry {
    pub path: LexicalPath,
    pub depth: usize,
    kind: Option<FileType>,
    shown: PathBuf,
    who: &'static str,
}

impl Entry {
    pub fn is_file(&self) -> bool {
        self.kind.is_some_and(|k| k.is_file())
    }

    /// The name a reader takes back unchanged.
    ///
    /// # Errors
    /// A name no ral string can hold.
    pub fn name(&self) -> Settled<String> {
        self.shown
            .to_str()
            .map(literal_relative)
            .ok_or_else(|| unnameable(self.who, &self.shown))
    }
}

impl<I: Iterator<Item = Settled<Sighting>>> Admitted<I> {
    /// The next admitted entry.
    ///
    /// # Errors
    /// The stop in force.
    pub fn next(&mut self, shell: &Shell) -> Settled<Option<Entry>> {
        for sighting in self.sightings.by_ref() {
            let Sighting { path, depth, kind } = sighting?;
            let anchored = shell.anchor(&path);
            if !shell.admits_fs_exact(&FsOp::Read, &anchored.canonicalise_lenient()) {
                continue;
            }
            let shown = match self
                .base
                .as_ref()
                .and_then(|cwd| path.strip_prefix(cwd).ok())
            {
                Some(rel) => rel.to_path_buf(),
                None => path,
            };
            return Ok(Some(Entry {
                path: anchored,
                depth,
                kind,
                shown,
                who: self.who,
            }));
        }
        Ok(None)
    }
}

#[allow(
    clippy::unnecessary_debug_formatting,
    reason = "Debug escapes the invalid bytes; Display would mangle them"
)]
fn unnameable(who: &str, shown: &Path) -> crate::types::Break {
    let dir = shown.ancestors().find_map(Path::to_str).unwrap_or("");
    let name = shown.strip_prefix(dir).unwrap_or(shown);
    sig(format!(
        "{who}: {dir}: {name:?} is not valid UTF-8, so no ral string can hold it; rename it?"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancelled_walk_aborts() {
        let shell = crate::test_helper::core_shell();
        shell
            .cancel_handle()
            .cancel(crate::process::CancelCause::Interrupted);
        let mut walk = Walk::tracked(&shell.cwd()).admitted(&shell, "walk", ".");
        let stop = walk
            .next(&shell)
            .err()
            .expect("a cancelled scope aborts the walk");
        assert_eq!(stop.code(), 130);
    }
}
