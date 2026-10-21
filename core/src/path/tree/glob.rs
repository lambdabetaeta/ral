//! Glob patterns: a regular language over path components, and the walk
//! that enumerates a pattern's candidates.
//!
//! A wildcard never matches a name beginning with `.`; only a literal
//! component names a dotfile.

use super::walk::Walk;
use crate::path::shown;
use globset::{GlobBuilder, GlobMatcher};
use std::ffi::{OsStr, OsString};
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
    /// A wildcard component beginning with `.`, one `globset` rejects, or
    /// a `..` after a wildcard, which no walked entry carries.
    pub(crate) fn parse(pattern: &Path) -> Result<Self, String> {
        let mut glob = Self {
            root: PathBuf::new(),
            steps: Vec::new(),
        };
        for c in pattern.components() {
            if c == Component::ParentDir && !glob.steps.is_empty() {
                return Err(format!(
                    "'{}': a walk never climbs '..' after a wildcard; glob from the directory it climbs to, as in `glob '../x'`?",
                    shown(pattern)
                ));
            }
            match step(c)? {
                Step::Name(name) if glob.steps.is_empty() => glob.root.push(name),
                s => glob.steps.push(s),
            }
        }
        Ok(glob)
    }

    /// The walk over the pattern's candidates, entering only directories
    /// a match can lie beneath; [`Self::matches`] picks the matches out.
    pub(crate) fn walk(self: &Arc<Self>) -> Walk {
        let bounded = !self.steps.iter().any(|s| matches!(s, Step::Any));
        let alive = Arc::clone(self);
        Walk::everything(&self.root)
            .following_links()
            .depth(bounded.then_some(self.steps.len()))
            .keeping(move |p| !alive.reach(p).is_empty())
    }

    pub(crate) fn matches(&self, path: &Path) -> bool {
        self.reach(path).contains(&self.steps.len())
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
    #[allow(
        clippy::disallowed_methods,
        reason = "native: a component that is not UTF-8 holds no wildcard"
    )]
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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[test] fixed patterns")]
mod tests {
    use super::Glob;
    use std::path::Path;

    #[test]
    fn a_parent_after_a_wildcard_is_refused() {
        assert!(Glob::parse(Path::new("*/../x")).is_err());
        assert!(Glob::parse(Path::new("../x")).is_ok());
        assert!(Glob::parse(Path::new("a/../*")).is_ok());
    }
}
