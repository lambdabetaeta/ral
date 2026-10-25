//! Helpers shared by core's tests and by hosts' tests under `test-util`.

use std::time::{Duration, Instant};

/// The first `Some` that `probe` answers within `within`, polled every 10ms;
/// `None` if it never came.
pub fn eventually<T>(within: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + within;
    loop {
        if let found @ Some(_) = probe() {
            return found;
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Core's Σ alone, no host dressing: what a checker with no live shell types
/// against.
pub fn core_schemes() -> crate::typecheck::SessionSchemes {
    crate::typecheck::SessionSchemes::new(crate::HostSurface::default().manifest())
}

/// A layer resolved from `caps`, a bare name found on the host's `PATH`: the
/// helper a test authors a layer through.  A path the grant names need not
/// exist, for the walk ends its steps where the disk does.
///
/// # Panics
/// If a spelling cannot be resolved.
pub fn layer(caps: crate::authority::policy::Capabilities) -> crate::authority::policy::GrantLayer {
    crate::authority::policy::GrantLayer::from_capabilities(caps, &crate::host::EnvVars::new())
        .expect("a test grant resolves")
}

/// The stack of `layers` over the ambient root.
pub fn stack(
    layers: impl IntoIterator<Item = crate::authority::policy::GrantLayer>,
) -> crate::authority::policy::GrantStack {
    let mut stack = crate::authority::policy::GrantStack::root();
    layers.into_iter().for_each(|layer| stack.push(layer));
    stack
}

/// The object at the absolute `path`, held as the walk holds it: what a
/// program is made of.
///
/// # Panics
/// If `path` is not absolute or names nothing the walk reaches.
pub fn located(path: impl AsRef<std::path::Path>) -> crate::path::Located {
    let spelled =
        crate::path::LexicalPath::absolute(path.as_ref()).expect("a test names an absolute path");
    crate::path::walk(&spelled, crate::path::LeafMode::Follow).expect("a test names a file")
}

/// Empty files and directories laid on disk, for a test to judge programs at.
///
/// A program is the object a walk holds, and no walk holds a path that is
/// nowhere.  A name is spelled `/`-rooted, as a grant writes it.
#[derive(Debug)]
pub struct Tree(std::path::PathBuf);

impl Tree {
    /// Lays `names` under `dir`, a directory the caller owns (a tempdir): each
    /// a directory when it ends in `/` and an empty file otherwise, its parents
    /// made.  A name the volume folds onto another is the same object.
    ///
    /// # Panics
    /// If `dir` is missing or a name cannot be laid.
    pub fn lay(dir: &std::path::Path, names: impl IntoIterator<Item: AsRef<str>>) -> Self {
        let tree = Self(real(dir).as_path().to_path_buf());
        for name in names {
            let name = name.as_ref();
            let at = tree.path(name);
            let parent = if name.ends_with('/') {
                Some(&*at)
            } else {
                at.parent()
            };
            std::fs::create_dir_all(parent.expect("a name has a place")).expect("a test dir");
            if !name.ends_with('/') {
                std::fs::write(&at, "").expect("a test file");
            }
        }
        tree
    }

    /// A tree nobody laid: its root is a name under the temp dir that nothing
    /// occupies, so a walk of a name below it goes as far as the temp dir and
    /// is lexical beyond.  For tests that judge names no disk need hold.
    ///
    /// # Panics
    /// If the temp dir is not an existing, absolute path.
    pub fn nowhere() -> Self {
        Self(real(std::env::temp_dir()).as_path().join("ral-nowhere"))
    }

    /// The real path of the tree's root.
    pub fn root(&self) -> &std::path::Path {
        &self.0
    }

    /// Where the `/`-rooted `spelled` lies on disk.
    pub fn path(&self, spelled: &str) -> std::path::PathBuf {
        let parts = spelled.split('/').filter(|part| !part.is_empty());
        parts.fold(self.0.clone(), |path, part| path.join(part))
    }

    /// [`Self::path`] as the text a grant or a check names it by.
    ///
    /// # Panics
    /// If the path is not UTF-8, which a tempdir's is.
    pub fn at(&self, spelled: &str) -> String {
        crate::path::utf8(&self.path(spelled))
            .expect("a test tree is UTF-8")
            .to_owned()
    }
}

/// The position of the existing, absolute `path`: where its walk ends.
///
/// # Panics
/// If `path` is not absolute or names nothing the walk reaches.
pub fn real(path: impl AsRef<std::path::Path>) -> crate::path::RealPath {
    let spelled =
        crate::path::LexicalPath::absolute(path.as_ref()).expect("a test names an absolute path");
    crate::path::RealPath::of(&spelled).expect("a test names what exists")
}

/// The position where the walk of the absolute `path` ends, as far as the
/// disk goes and the rest lexical: a place that need not exist.
///
/// # Panics
/// If `path` is not absolute or its walk fails.
pub fn place(path: impl AsRef<std::path::Path>) -> crate::path::RealPath {
    trace(path).real().clone()
}

/// The walk of the absolute `path`, leniently: the transcript an access to it
/// would be judged by.
///
/// # Panics
/// If `path` is not absolute or its walk fails.
pub fn trace(path: impl AsRef<std::path::Path>) -> crate::path::Trace {
    let spelled =
        crate::path::LexicalPath::absolute(path.as_ref()).expect("a test names an absolute path");
    crate::path::walk_lenient(&spelled, crate::path::LeafMode::Follow).expect("a test path walks")
}

/// The test process's working directory, which a test shell is rooted at.
///
/// # Panics
/// If the process has none, which no test can run without.
pub fn process_cwd() -> crate::path::CwdPath {
    crate::host::cwd().expect("the test process has a working directory")
}

/// A shell over core's own surface alone at [`process_cwd`]: no env vars, no
/// prelude.  The scaffold unit tests start from.
pub fn core_shell() -> crate::Shell {
    crate::Shell::root(core_runtime().into(), process_cwd())
}

/// Core's own surface as a runtime: what [`core_shell`] is a root over.
///
/// # Panics
/// If the host's baseline does not resolve, which no test can run without.
pub fn core_runtime() -> crate::Runtime {
    crate::HostSurface::default()
        .runtime(crate::io::terminal::TerminalState::default())
        .expect("a test host resolves its baseline")
}
