//! The fixes both enforcers suggest, each worded once: a guard refusal, a
//! kernel-denial hint and a sandbox's launch refusal say why at their site,
//! then end in one of these.

/// No allowance reopens an exclusion.
pub(crate) fn remove_the_exclusion() -> &'static str {
    "an allowance cannot override an exclusion; remove the exclusion from the policy that \
     imposes it"
}

/// The fs list a fix adds to.
#[derive(Clone, Copy)]
pub(crate) enum FsList {
    Read,
    Write,
    Either,
}

/// Let the grant's fs `list` name `what`.
pub(crate) fn add_to_fs(what: &str, list: FsList) -> String {
    let list = match list {
        FsList::Read => "`read`",
        FsList::Write => "`write`",
        FsList::Either => "`read` or `write`",
    };
    format!("add {what} to the grant's fs {list} list")
}

/// An allowance keeps the real path it was frozen to, so a link put on that
/// path since redirects nothing.
#[cfg(any(target_os = "linux", windows))]
pub(crate) fn grant_the_new_target() -> &'static str {
    "an allowance names what it named when granted. To allow the new target, grant it where \
     the ceiling is set (in exarch, restart with a grant that names it)"
}

/// Let the grant's exec `allow` list name the program, in any key's form.
pub(crate) fn add_to_exec() -> &'static str {
    "add it to the grant's exec `allow` list: `` `name git `` for a command name, \
     `` `absolute /path/to/it `` for a file, or `` `dir `absolute /path `` for its directory"
}
