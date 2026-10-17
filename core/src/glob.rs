//! Glob patterns: a regular language over path components, and the walk
//! that enumerates a pattern's matches.
//!
//! A wildcard never matches a name beginning with `.`; only a literal
//! component names a dotfile.

use crate::process::CancelScope;
use crate::types::{Settled, sig};
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::iter::successors;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

enum Step {
    Name(OsString),
    Wild(GlobMatcher),
    /// `**`: zero or more names.
    Any,
}

pub(crate) struct Glob {
    /// The leading literal components, where the walk starts.
    root: PathBuf,
    steps: Vec<Step>,
}

impl Glob {
    /// # Errors
    /// A wildcard component beginning with `.`, or one `globset` rejects.
    pub(crate) fn parse(pattern: &Path) -> Result<Self, String> {
        let mut glob = Self {
            root: PathBuf::new(),
            steps: Vec::new(),
        };
        for c in pattern.components() {
            match step(c)? {
                Step::Name(name) if glob.steps.is_empty() => glob.root.push(name),
                s => glob.steps.push(s),
            }
        }
        Ok(glob)
    }

    /// Every path the pattern matches, polling `scope` at each entry visited.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:glob-walk] `glob`'s directory walk: a listing predicate, not turn-time model data I/O; the caller judges each hit against the live grant, and the walk polls the cancel check per entry. Also stats an unfollowable entry to tell a dangling link from a vanished one."
    )]
    pub(crate) fn walk(self, scope: CancelScope) -> impl Iterator<Item = Settled<PathBuf>> {
        /// What a walk error leaves of its entry: a link the walker could not
        /// follow (dangling, or back to an ancestor) is still an entry; a
        /// vanished one is not.
        fn unfollowed(e: ignore::Error) -> Result<Option<PathBuf>, ignore::Error> {
            match e {
                ignore::Error::WithDepth { err, .. } => unfollowed(*err),
                ignore::Error::Loop { child, .. } => Ok(Some(child)),
                ignore::Error::WithPath { path, err }
                    if err
                        .io_error()
                        .is_some_and(|io| io.kind() == ErrorKind::NotFound) =>
                {
                    Ok(std::fs::symlink_metadata(&path).is_ok().then_some(path))
                }
                e => Err(e),
            }
        }

        let glob = Arc::new(self);
        let bounded = !glob.steps.iter().any(|s| matches!(s, Step::Any));
        let alive = Arc::clone(&glob);
        WalkBuilder::new(&glob.root)
            .standard_filters(false)
            .follow_links(true)
            .max_depth(bounded.then_some(glob.steps.len()))
            .filter_entry(move |e| !alive.reach(e.path()).is_empty())
            .build()
            .filter_map(move |item| {
                if let Err(b) = scope.poll() {
                    return Some(Err(b));
                }
                let path = match item.map(ignore::DirEntry::into_path) {
                    Ok(path) => path,
                    Err(e) => match unfollowed(e) {
                        Ok(path) => path?,
                        Err(e) => return Some(Err(sig(format!("glob: {e}")))),
                    },
                };
                glob.reach(&path)
                    .contains(&glob.steps.len())
                    .then_some(Ok(path))
            })
    }

    /// The steps `path` can have reached, closed under `**` matching nothing.
    /// A path outside the root reaches none.
    fn reach(&self, path: &Path) -> Vec<usize> {
        path.strip_prefix(&self.root).map_or_else(
            |_| Vec::new(),
            |rel| {
                rel.components().fold(self.close([0]), |states, c| {
                    self.close(
                        states
                            .into_iter()
                            .filter_map(|i| self.advance(i, c.as_os_str())),
                    )
                })
            },
        )
    }

    fn advance(&self, i: usize, name: &OsStr) -> Option<usize> {
        let hidden = name.as_encoded_bytes().starts_with(b".");
        match self.steps.get(i)? {
            Step::Name(n) => (n == name).then_some(i + 1),
            Step::Wild(m) => (!hidden && m.is_match(name)).then_some(i + 1),
            Step::Any => (!hidden).then_some(i),
        }
    }

    fn close(&self, states: impl IntoIterator<Item = usize>) -> Vec<usize> {
        let mut closed: Vec<usize> = states
            .into_iter()
            .flat_map(|i| {
                successors(Some(i), |&j| {
                    matches!(self.steps.get(j), Some(Step::Any)).then_some(j + 1)
                })
            })
            .collect();
        closed.sort_unstable();
        closed.dedup();
        closed
    }
}

fn step(c: Component<'_>) -> Result<Step, String> {
    let os = c.as_os_str();
    let Some(s) = os.to_str().filter(|s| s.contains(['*', '?', '[', '{'])) else {
        return Ok(Step::Name(os.to_owned()));
    };
    if s == "**" {
        return Ok(Step::Any);
    }
    if s.starts_with('.') {
        return Err(format!(
            "'{s}': a wildcard never matches a name starting with '.'; name the file literally, or filter `list-dir`?"
        ));
    }
    GlobBuilder::new(s)
        .backslash_escape(false)
        .literal_separator(true)
        .build()
        .map(|g| Step::Wild(g.compile_matcher()))
        .map_err(|e| e.to_string())
}
