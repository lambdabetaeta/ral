//! The host's system reads, as a [`Baseline`].

use super::freeze::{Home, PolicyError};
use super::toolchains;
use crate::authority::policy::{Baseline, FsAllow};

impl Baseline {
    /// [`of_host`](Self::of_host) with the process's home given, by which
    /// every system read is judged: [`toolchains::keeps_system_read`] drops a
    /// root, the home, an ancestor of it and what lies inside it.
    ///
    /// # Errors
    /// As [`of_env`](Self::of_env).
    pub(crate) fn of_home(home: Option<&str>) -> Result<Self, PolicyError> {
        Self::of_env(crate::host::var, home)
    }

    /// [`of_home`](Self::of_home) over an injected environment, which names
    /// Windows' system directories and the toolchain rows' overrides: what
    /// [`toolchains::system_reads_in`] keeps, each a prefix.
    ///
    /// # Errors
    /// A `home` whose walk fails; a system read that is malformed, named with
    /// what chose it, or whose walk fails.
    pub(crate) fn of_env(
        env: impl Fn(&str) -> Option<String>,
        home: Option<&str>,
    ) -> Result<Self, PolicyError> {
        let home = home.map(Home::of).transpose()?;
        let reads = toolchains::system_reads_in(env, home.as_ref())?;
        Ok(reads
            .into_iter()
            .map(|dir| FsAllow::Prefix(dir.extent()))
            .collect())
    }
}
