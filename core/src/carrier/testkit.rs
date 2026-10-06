//! Engines for core's own tests, booted as every engine is.

use super::IdentityTransport;
use crate::engine::{Booted, EngineInstaller};
use crate::protocol::{Attach, Report, Run};

/// An installer that never hatches, so it states the one policy a host
/// with no grant lexicon can: no seeded child.
pub(crate) const fn installer(
    tag: &'static str,
    boot: fn(&Attach) -> Result<Booted, String>,
) -> EngineInstaller {
    EngineInstaller {
        tag,
        boot,
        narrow: |base, _| {
            Err(format!(
                "this engine hatches no children, so it has no policy to resolve `{base}` by"
            ))
        },
    }
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "must match EngineInstaller::boot's signature, which can genuinely refuse"
)]
fn bare(_attach: &Attach) -> Result<Booted, String> {
    Ok(Booted {
        shell: crate::test_helper::core_shell(),
        keep: Box::new(()),
    })
}

/// A shell with no prelude and no surface.
pub(crate) static BARE: [EngineInstaller; 1] = [installer("bare", bare)];

/// An attach to `installers`' first recipe, seated in the temp dir.
pub(crate) fn attach(installers: &[EngineInstaller]) -> Attach {
    let temp = std::env::temp_dir();
    Attach::new(installers[0].tag, temp.clone(), temp)
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
        Run::captured(src, "<test>"),
        std::sync::Arc::new(()),
    )
    .expect("an identity transport answers synchronously")
}
