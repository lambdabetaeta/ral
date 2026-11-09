//! Glob patterns: a regular language over path components, and the walk
//! that enumerates a pattern's candidates.
//!
//! Compile the pattern as written, then resolve its leading literal path.
//! A cwd or expanded sigil is a directory name, never pattern syntax.
//!
//! A wildcard never matches a name beginning with `.`; only a literal
//! component names a dotfile.

use super::Walk;
use crate::path::site::Silent;
use crate::path::{LexicalPath, Resolver, utf8};
use crate::site::site;
use globset::{GlobBuilder, GlobMatcher};
use regex_syntax::ParserBuilder;
use regex_syntax::hir::literal::Extractor;
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
    root: LexicalPath,
    steps: Vec<Step>,
}

/// A compiled source pattern, not yet anchored.  Only the text supplied as
/// the pattern reaches `step`; resolution of `prefix` happens afterwards.
struct Pattern {
    prefix: PathBuf,
    steps: Vec<Step>,
}

impl Pattern {
    fn parse(raw: &str) -> Result<Self, String> {
        let (mut prefix, mut steps) = (PathBuf::new(), Vec::new());
        let pattern = site!(Silent::GlobComponents, Path::new(raw));
        for c in site!(Silent::GlobComponents, pattern => components()) {
            if c == Component::ParentDir && !steps.is_empty() {
                return Err(format!(
                    "'{raw}': a walk never climbs '..' after a wildcard; glob from the directory it climbs to, as in `glob '../x'`?"
                ));
            }
            match step(c)? {
                Step::Name(name) if steps.is_empty() => prefix.push(name),
                s => steps.push(s),
            }
        }
        Ok(Self { prefix, steps })
    }
}

impl Glob {
    /// Compile before resolving: names introduced by cwd anchoring or sigil
    /// expansion never reach the pattern compiler.
    pub(in crate::path) fn resolve(raw: &str, resolver: &Resolver<'_>) -> Result<Self, String> {
        let Pattern { prefix, steps } = Pattern::parse(raw)?;
        let root =
            resolver.resolve(utf8(&prefix).expect("components of a UTF-8 pattern are UTF-8"));
        Ok(Self { root, steps })
    }

    /// An absolute pattern spelled by the program, with no sigils or cwd.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn absolute(raw: &str) -> Result<Self, String> {
        let Pattern { prefix, steps } = Pattern::parse(raw)?;
        let root = LexicalPath::absolute(&prefix).ok_or_else(|| {
            format!(
                "'{}': a glob walks from an absolute directory",
                crate::path::shown(&prefix)
            )
        })?;
        Ok(Self { root, steps })
    }

    /// The walk over the pattern's candidates, entering only directories
    /// a match can lie beneath; [`Self::matches`] picks the matches out.
    pub(crate) fn walk(self: &Arc<Self>) -> Walk {
        let bounded = !self.steps.iter().any(|s| matches!(s, Step::Any));
        let alive = Arc::clone(self);
        Walk::all(&self.root)
            .follow_links()
            .max_depth(bounded.then_some(self.steps.len()))
            .filter_entry(move |shown| !alive.reach(shown).is_empty())
    }

    pub(crate) fn matches(&self, shown: &LexicalPath) -> bool {
        self.reach(shown).contains(&self.steps.len())
    }

    /// The steps the entry `shown` can have reached, closed under `**`
    /// matching nothing: it is named as the walk reached it, through any link.
    /// A name outside the root reaches none.
    fn reach(&self, shown: &LexicalPath) -> Vec<usize> {
        site!(Silent::GlobReach, shown.as_path() => strip_prefix(self.root.as_path())).map_or_else(
            |_| Vec::new(),
            |rel| {
                site!(Silent::GlobReach, rel => components()).fold(self.close([0]), |states, c| {
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
        let visible = !hidden(name.as_encoded_bytes());
        match self.steps.get(i)? {
            Step::Name(n) => (n == name).then_some(i + 1),
            Step::Wild(m) => (visible && m.is_match(name)).then_some(i + 1),
            Step::Any => visible.then_some(i),
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
    // A Windows namespace prefix may contain `?`; it is a root, not a name.
    let Component::Normal(os) = c else {
        return Ok(Step::Name(c.as_os_str().to_owned()));
    };
    let Some(s) =
        site!(Silent::GlobWildcard, os => to_str()).filter(|s| s.contains(['*', '?', '[', '{']))
    else {
        return Ok(Step::Name(os.to_owned()));
    };
    if s == "**" {
        return Ok(Step::Any);
    }
    let glob = GlobBuilder::new(s)
        .backslash_escape(false)
        .literal_separator(true)
        .build()
        .map_err(|e| e.to_string())?;
    if hidden_only(&glob) {
        return Err(format!(
            "'{s}': a wildcard never matches a name starting with '.', and this one matches nothing else; name each file literally, or filter `list-dir`?"
        ));
    }
    Ok(Step::Wild(glob.compile_matcher()))
}

/// Whether every match is hidden, judged by prefix literals; unbounded prefixes pass.
fn hidden_only(glob: &globset::Glob) -> bool {
    let hir = ParserBuilder::new()
        .utf8(false)
        .build()
        .parse(glob.regex())
        .expect("globset emits a valid regex");
    Extractor::new()
        .extract(&hir)
        .literals()
        .is_some_and(|prefixes| prefixes.iter().all(|p| hidden(p.as_bytes())))
}

fn hidden(name: &[u8]) -> bool {
    name.starts_with(b".")
}

#[cfg(test)]
mod tests {
    use super::Glob;
    use crate::host::EnvVars;
    use crate::path::{CwdPath, Resolver};

    /// `rest` beneath the host's root: `/rest` or `C:\rest`.
    fn rooted(rest: &str) -> String {
        if cfg!(windows) {
            format!(r"C:\{}", rest.replace('/', r"\"))
        } else {
            format!("/{rest}")
        }
    }

    fn parse(rest: &str) -> Result<Glob, String> {
        let pattern = rooted(rest);
        Glob::absolute(&pattern)
    }

    #[test]
    fn the_cwd_is_literal_even_when_its_names_are_pattern_syntax() {
        let env = EnvVars::new();
        for directory in [
            "audio[1]",
            "star*",
            "single?",
            "{a,b}",
            "unclosed[",
            "unclosed{",
            ".hidden[1]",
        ] {
            let cwd = CwdPath::new(rooted(directory).into()).unwrap();
            let resolver = Resolver::at(&env, &cwd);
            let glob = resolver.glob("*.txt").unwrap();
            assert_eq!(glob.root, resolver.resolve(""), "{directory}");
            assert!(glob.matches(&resolver.resolve("a.txt")), "{directory}");
            assert!(
                !glob.matches(&resolver.resolve(".hidden.txt")),
                "{directory}"
            );
        }
    }

    #[test]
    fn sigil_expansions_are_literal_but_their_suffixes_are_patterns() {
        let cwd = CwdPath::new(rooted("cwd[1]").into()).unwrap();
        let home = rooted("home[");
        let config = rooted("config{");
        let env = [
            ("HOME", home.as_str()),
            ("XDG_CONFIG_HOME", config.as_str()),
        ]
        .into_iter()
        .collect::<EnvVars>();
        let resolver = Resolver::at(&env, &cwd);
        for sigil in ["~", "xdg:config"] {
            let glob = resolver.glob(&format!("{sigil}/[ab]/*.txt")).unwrap();
            assert_eq!(glob.root, resolver.resolve(sigil));
            assert!(glob.matches(&resolver.resolve(&format!("{sigil}/a/f.txt"))));
            assert!(glob.matches(&resolver.resolve(&format!("{sigil}/b/f.txt"))));
            assert!(!glob.matches(&resolver.resolve(&format!("{sigil}/c/f.txt"))));
        }
    }

    #[test]
    fn brackets_written_in_the_pattern_remain_syntax() {
        let cwd = CwdPath::new(rooted("cwd[1]").into()).unwrap();
        let env = EnvVars::new();
        let resolver = Resolver::at(&env, &cwd);
        for (pattern, prefix) in [
            ("[ab]/*.txt".to_owned(), "cwd[1]"),
            (rooted("cwd[1]/[ab]/*.txt"), "cwd1"),
        ] {
            let glob = resolver.glob(&pattern).unwrap();
            for name in ["a", "b"] {
                let entry = resolver.resolve(&rooted(&format!("{prefix}/{name}/f.txt")));
                assert!(glob.matches(&entry), "{pattern}");
            }
            let entry = resolver.resolve(&rooted(&format!("{prefix}/c/f.txt")));
            assert!(!glob.matches(&entry), "{pattern}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_namespace_prefix_is_not_a_wildcard() {
        let glob = Glob::absolute(r"\\?\C:\work\*.txt").unwrap();
        let expected = crate::path::LexicalPath::absolute_str(r"\\?\C:\work").unwrap();
        assert_eq!(glob.root, expected);
        assert_eq!(glob.steps.len(), 1);
    }

    #[test]
    fn a_parent_after_a_wildcard_is_refused() {
        assert!(parse("*/../x").is_err());
        assert!(parse("../x").is_ok());
        assert!(parse("a/../*").is_ok());
    }

    #[test]
    fn a_wildcard_that_can_match_only_dotfiles_is_refused() {
        for dotted in ["x/.h*", "x/[.]env", "x/{.env,.git}"] {
            assert!(parse(dotted).is_err(), "{dotted}");
        }
        for open in ["x/*", "x/?env", "x/[.a]env", "x/{.env,a}", "x/*{.rs,.toml}"] {
            assert!(parse(open).is_ok(), "{open}");
        }
    }
}
