// The typed-site vocabulary, shared by every crate: see `lint/site.rs`.
include!("../../lint/site.rs");

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// The editor spawned on a prompt draft, for the `C-x C-e` hatch: the
    /// user's own choice of program, run with the draft file as its argument.
    /// It raises no card: a UI action on a file the harness made for it, not
    /// turn-time model I/O.
    EditorCompose,
    /// The engine's home, which is the launching user's `HOME`, read from the
    /// host process.  It raises no card: it reads a variable, not an object.
    EngineHome,
    /// The scratch variable's value, lifted to the path it names, which the
    /// engine anchors.  It raises no card: it names a path and touches none.
    ScratchPath,
    /// The `USER` and `HOME` the prompt's `Host` section reports: host truths
    /// by definition, so no shell overlay applies.  It raises no card: it
    /// reads variables, not objects.
    HostSection,
    /// `git(1)`, run to probe the working tree for the `Host` section:
    /// best-effort host information.  It raises no card: not turn-time data
    /// I/O.
    GitLaunch,
    /// The one HTTPS client the genai transport and the model listing share.
    /// It raises no card: per-turn machinery carrying the conversation to the
    /// provider, which the transcript already is, so a card here would card
    /// the transcript's own delivery.
    ProviderClient,
    /// A client for the one-shot fetch of the public model catalogue: prices
    /// and context windows, no session data either way.  It raises no card:
    /// startup bookkeeping, not turn-time model I/O.
    PricingClient,
    /// The loopback port the authorize page redirects back to: 1455 by
    /// convention, else any free one, so only the port may vary.  It raises no
    /// card: sign-in machinery the user started, on this machine only, with no
    /// turn running to card it; the browser handoff beside it is silent on the
    /// same ground.
    CallbackListener,
    /// The platform browser, opened on the OAuth authorize URL with `open`.
    /// It raises no card: not turn-time data I/O.
    #[cfg(target_os = "macos")]
    BrowserLaunch,
    /// The platform browser, opened on the OAuth authorize URL with
    /// `xdg-open`.  It raises no card: not turn-time data I/O.
    #[cfg(target_os = "linux")]
    BrowserLaunchLinux,
    /// The client every login flow and token refresh talks to the issuer's
    /// endpoints with.  It raises no card: sign-in machinery the user started,
    /// carrying credentials and no session data, silent on the same ground as
    /// [`Self::CallbackListener`].
    OauthClient,
    /// A path the user gave as a command-line argument: the session `exarch
    /// export` renders, and the file `--file` reads the prompt from.  It raises
    /// no card: no model named it, and the user's own argument is nothing a
    /// grant judges.
    CliArgument,
    /// The file an export is written to, as the user named it: `exarch export
    /// --out`, or the argument of the TUI's `/export`.  It raises no card: no
    /// model named it, and the user's own command is nothing a grant judges.
    ExportDestination,
}

impl Site for Silent {}
