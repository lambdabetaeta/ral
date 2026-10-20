//! Engines for core's own tests, booted as every engine is.

use super::IdentityTransport;
use crate::engine::EngineInstaller;
use crate::path::EnginePath;
use crate::protocol::{Attach, EngineSpec, Report, Root, Run};
use crate::source::SourceOrigin;
use crate::terminal::TerminalState;
use crate::types::{Runtime, Shell};

/// An installer that never makes children, so it states the one policy a host
/// with no grant lexicon can: no seeded child.
pub(crate) const fn installer(
    tag: &'static str,
    runtime: fn(&EngineSpec) -> Runtime,
    root: fn(&Root, &mut Shell) -> Result<(), String>,
) -> EngineInstaller {
    EngineInstaller {
        tag,
        runtime,
        root,
        narrow: |base, _| {
            Err(format!(
                "this engine makes no children, so it has no policy to resolve `{base}` by"
            ))
        },
    }
}

/// A runtime with no prelude and no surface.
pub(crate) static BARE: [EngineInstaller; 1] = [installer(
    "bare",
    |_| crate::HostSurface::default().runtime(TerminalState::default()),
    |_, _| Ok(()),
)];

/// An attach to `installers`' first recipe, seated in the temp dir.
pub(crate) fn attach(installers: &[EngineInstaller]) -> Attach {
    let temp = std::env::temp_dir();
    Attach::new(
        installers[0].tag,
        EnginePath::Host(temp.clone()),
        EnginePath::Host(temp),
    )
}

/// Boot `attach` in this process.
pub(crate) fn boot_at(
    installers: &'static [EngineInstaller],
    attach: &Attach,
) -> IdentityTransport {
    IdentityTransport::boot(installers, attach).expect("a test recipe boots")
}

/// Boot `installers`' first recipe, seated in the temp dir.
pub(crate) fn boot(installers: &'static [EngineInstaller]) -> IdentityTransport {
    boot_at(installers, &attach(installers))
}

/// Dispatch `src` under the mute host.
pub(crate) fn eval(transport: &IdentityTransport, src: &str) -> Report {
    super::dispatch_to_report(
        transport,
        Run::captured(src, SourceOrigin::named("<test>")),
        std::sync::Arc::new(()),
    )
    .expect("an identity transport answers synchronously")
}
