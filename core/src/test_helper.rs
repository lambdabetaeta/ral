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

/// A shell over core's own surface alone: no env vars, no prelude.  The
/// scaffold unit tests start from.
pub fn core_shell() -> crate::Shell {
    crate::HostSurface::default().shell(crate::terminal::TerminalState::default())
}
