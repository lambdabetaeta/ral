//! The host's environment and identity sites: see `lint/site.rs`.

use crate::site::Site;

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(super) enum Silent {
    /// A variable of the host process environment, as text: the one reader
    /// that [`crate::host::var`] publishes, which the host-only facts of
    /// `host` (`HOME`, `USER`, the XDG bases) and every other host-level read
    /// go through.  A `within [env: …]` overlay never applies there.  It
    /// raises no card: reading a variable opens no object.
    HostVar,
    /// A variable of the host process environment as the OS holds it, for a
    /// value that is a path or that need not be Unicode
    /// ([`crate::host::var_os`]): the same reader as `HostVar`, undecoded.  It
    /// raises no card: reading a variable opens no object.
    HostVarOs,
    /// The host process environment, enumerated as the OS holds it
    /// ([`crate::host::vars_os`]): the one enumeration of the host, for the
    /// `env` builtin, a child's inherited block and the like.  It raises no
    /// card: it reads variables, not objects.
    HostVars,
    /// The host's temp directory ([`crate::host::temp_dir`]): `TMPDIR`,
    /// `TMP` or `TEMP`, else the OS's default.  It raises no card: it reads the
    /// host's environment and opens no object.
    HostTemp,
    /// The account's home from the user database, `std::env::home_dir`'s rule
    /// (`getpwuid_r` on Unix, the profile folder on Windows), asked only where
    /// no `HOME` is bound: the fallback inside `host::xdg` and `host::home_dot`,
    /// so ral's own layout resolves under `env -i`, cron and systemd units.  A
    /// `~` in a script never reads it.  It raises no card: it reads the user
    /// database, not an object a model named.
    AccountHome,
    /// The host process's working directory, for a caller with no shell to
    /// ask.  It raises no card: it reads one fact of the host, not an object
    /// a model names.
    HostCwd,
}

impl Site for Silent {}
