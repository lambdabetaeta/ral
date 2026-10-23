// The typed-site vocabulary of this program: see `lint/site.rs`.
include!("../../../../lint/site.rs");

/// Sites that raise no card.
///
/// The machine broker links no ral-core and holds machines, never a ral
/// session's lease, children or staged writes, so an exit of its process
/// unwinds nothing another owner needs.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// The exit of the stub that stands in for the service off Windows, after
    /// it has said there is nothing to broker.  Nothing was started, so
    /// nothing is left unreleased, and it raises no card.
    #[cfg(not(windows))]
    Stub,
    /// The exit of the console listener once `serve` has returned and told its
    /// cause: nothing it served is still running, and it raises no card.
    #[cfg(windows)]
    ConsoleListenerStopped,
    /// The exit after the service controller's dispatcher refused the program,
    /// before it served anything: there is no machine to tear down, and it
    /// raises no card.
    #[cfg(windows)]
    DispatcherRefused,
    /// The exit by which the service stops, as the controller asks: every
    /// machine is held by the thread serving its client, so the process ending
    /// is what tears them all down, and there is no ral session here to unwind.
    /// It raises no card.
    #[cfg(windows)]
    ServiceStop,
}

impl Site for Silent {}
