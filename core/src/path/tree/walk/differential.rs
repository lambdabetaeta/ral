//! The walk against `ignore::WalkBuilder`, differentially: the same entry set
//! under no grant over a generated corpus of nested repositories, ignore
//! files, hidden entries, byte-order marks, whitelists, hidden and ignored
//! roots and root links.  The cases in `tests` say what the rules are; this
//! says nothing was left out.  `.jj` repositories are the one known
//! difference, and the corpus has none.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] a corpus laid by name, and the walker it is held against"
)]

use super::tests::{lay, lexical, open, scope, tmp};
use super::*;
use crate::path::relative_key;
use std::collections::BTreeSet;
use std::path::PathBuf;

const DIRS: &[&str] = &["a", "b", "c", "build", "sub", ".hid", "vendor", "x.d"];
const FILES: &[&str] = &[
    "a.txt",
    "b.log",
    "keep.log",
    "c.tmp",
    ".visible",
    ".other",
    "x1.log",
    "secret.txt",
    "top.txt",
    "build",
];
const PATTERNS: &[&str] = &[
    "*.log",
    "!keep.log",
    "build/",
    "/top.txt",
    "sub",
    "!sub",
    "*.tmp",
    "!.visible",
    "a",
    "!a",
    "c/",
    "**/vendor",
    "b/*",
    "# a comment",
    "",
    "/*.txt",
    "x?.log",
    "secret.txt",
    "!secret.txt",
    ".hid",
    "!.hid/",
    "*",
    "!*/",
    "x.d/**",
    "**/x.d/*",
    "\\#literal",
    "dir/*.txt",
];

/// A tree of generated entries and what was written to make it.
struct Corpus {
    rng: fastrand::Rng,
    top: PathBuf,
    spare: PathBuf,
    dirs: Vec<PathBuf>,
    log: Vec<String>,
}

impl Corpus {
    fn pick<'a>(&mut self, from: &'a [&'a str]) -> &'a str {
        from[self.rng.usize(..from.len())]
    }

    fn write(&mut self, at: &Path, text: &str) {
        self.log.push(format!("{}: {text:?}", at.display()));
        lay(
            at.parent().unwrap(),
            &[(at.file_name().unwrap().to_str().unwrap(), text)],
        );
    }

    /// The text of an ignore file: a few patterns, perhaps behind a byte-order
    /// mark, perhaps with DOS line ends.
    fn patterns(&mut self) -> String {
        let end = if self.rng.u8(..4) == 0 { "\r\n" } else { "\n" };
        let mark = if self.rng.u8(..4) == 0 {
            "\u{feff}"
        } else {
            ""
        };
        let lines: Vec<&str> = (0..self.rng.usize(1..5))
            .map(|_| self.pick(PATTERNS))
            .collect();
        format!("{mark}{}{end}", lines.join(end))
    }

    fn generate(seed: u64) -> (tempfile::TempDir, Self) {
        let (guard, root) = tmp();
        let mut corpus = Self {
            rng: fastrand::Rng::with_seed(seed),
            top: root.join("top"),
            spare: root.join("spare"),
            dirs: Vec::new(),
            log: Vec::new(),
        };
        std::fs::create_dir_all(&corpus.top).unwrap();
        corpus.dirs.push(corpus.top.clone());
        for _ in 0..corpus.rng.usize(6..14) {
            let parent = corpus.dirs[corpus.rng.usize(..corpus.dirs.len())].clone();
            let at = parent.join(corpus.pick(DIRS));
            if !at.exists() {
                std::fs::create_dir_all(&at).unwrap();
                corpus.dirs.push(at);
            }
        }
        for dir in corpus.dirs.clone() {
            for _ in 0..corpus.rng.usize(0..4) {
                let at = dir.join(corpus.pick(FILES));
                if !at.exists() {
                    corpus.write(&at, "");
                }
            }
            for name in [".ignore", ".gitignore"] {
                if corpus.rng.u8(..3) == 0 {
                    let text = corpus.patterns();
                    corpus.write(&dir.join(name), &text);
                }
            }
            if corpus.rng.u8(..4) == 0 {
                corpus.repository(&dir);
            }
        }
        (guard, corpus)
    }

    /// `dir` made a repository, as a directory or as a pointer file, with an
    /// `info/exclude` of its own.  A pointer's git directory has a
    /// `commondir`, as every linked worktree's does.
    fn repository(&mut self, dir: &Path) {
        let exclude = self.patterns();
        match self.rng.u8(..3) {
            0 => {
                std::fs::create_dir_all(dir.join(".git")).unwrap();
                self.write(&dir.join(".git/info/exclude"), &exclude);
            }
            1 => {
                let id = self.log.len();
                let (linked, common) = (
                    self.spare.join(format!("w{id}")),
                    self.spare.join(format!("c{id}")),
                );
                self.write(&linked.join("commondir"), &format!("{}\n", shown(&common)));
                self.write(&common.join("info/exclude"), &exclude);
                self.write(&dir.join(".git"), &format!("gitdir: {}\n", shown(&linked)));
            }
            _ => self.write(&dir.join(".git"), "not a pointer\n"),
        }
    }
}

/// What `ignore` yields beneath `root`, by the names it reached them by.
#[allow(clippy::disallowed_methods, reason = "[test] the walker held against")]
fn theirs(root: &Path) -> BTreeSet<String> {
    let mut walker = ::ignore::WalkBuilder::new(root);
    walker.git_global(false);
    walker
        .build()
        .flatten()
        .filter(|entry| entry.depth() > 0)
        .map(|entry| {
            let rest = entry.path().strip_prefix(root).unwrap();
            rest.components()
                .map(|c| c.as_os_str().to_str().unwrap())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect()
}

/// What the walk yields beneath `root`, by the names it reached them by.
fn ours(root: &Path) -> BTreeSet<String> {
    let at = lexical(root);
    Walk::respecting_ignores(&at)
        .readable_by(open())
        .entries(scope())
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.depth > 0)
        .map(|entry| relative_key(&entry.shown, &at).unwrap())
        .collect()
}

#[test]
fn the_walk_and_ignore_see_the_same_entries_over_a_generated_corpus() {
    for seed in 0..60 {
        let (_guard, corpus) = Corpus::generate(seed);
        std::fs::create_dir_all(&corpus.spare).unwrap();
        let linked = corpus.top.parent().unwrap().join("linked");
        let hosted = corpus.dirs[corpus.dirs.len() / 2].clone();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&hosted, &linked).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&hosted, &linked).unwrap();
        for root in corpus.dirs.iter().chain([&linked]) {
            assert_eq!(
                ours(root),
                theirs(root),
                "seed {seed}, root {}\n{}",
                root.display(),
                corpus.log.join("\n")
            );
        }
    }
}
