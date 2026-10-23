// The typed-site vocabulary, shared by every crate: see `lint/site.rs`.
include!("../../lint/site.rs");

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// The `/bin/sh` that the login-shell bridge hands a POSIX command string
    /// to, built and exec'd in place of this process.  Infrastructure
    /// plumbing, not a model's exec image, so it raises no exec card.
    #[cfg(unix)]
    RespawnPosixSh,
    /// The home directory of this process, taken as an identity engine's own
    /// at attach: the engine runs in this process, so a `within [env: …]`
    /// overlay does not apply.  It raises no card: it reads a variable, not
    /// an object.
    EngineHome,
    /// The exit of the setuid refusal, `main`'s first statement, asked before
    /// any re-exec child is served.  Nothing is booted yet, so there is no
    /// terminal lease, watched child or staged write for the missing
    /// destructors to leave behind.  It raises no card.
    #[cfg(unix)]
    SetuidExit,
    /// The launching user's home, read to complete a typed `~`: completion
    /// completes for the launching user, outside any shell overlay.  It raises
    /// no card: it reads a variable, not an object.
    CompletionHome,
    /// The script the user named as a command-line argument, read to run it.
    /// It raises no card: no model named it, and the user's own argument is
    /// nothing a grant judges.
    CliArgument,
}

impl Site for Silent {}
