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
/// door a test authors a layer through.  A path the grant names need not
/// exist, for the walk ends its steps where the disk does.
///
/// # Panics
/// If a spelling cannot be resolved.
pub fn layer(caps: crate::capability::Capabilities) -> crate::capability::GrantLayer {
    crate::capability::GrantLayer::from_capabilities(caps, &crate::types::EnvVars::new())
        .expect("a test grant resolves")
}

/// The stack of `layers` over the ambient root.
pub fn stack(
    layers: impl IntoIterator<Item = crate::capability::GrantLayer>,
) -> crate::capability::GrantStack {
    let mut stack = crate::capability::GrantStack::root();
    layers.into_iter().for_each(|layer| stack.push(layer));
    stack
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
/// If the host's floor does not resolve, which no test can run without.
pub fn core_runtime() -> crate::Runtime {
    crate::HostSurface::default()
        .runtime(crate::terminal::TerminalState::default())
        .expect("a test host resolves its floor")
}
