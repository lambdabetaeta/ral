include!("../../lint/site.rs");

/// The broker has no ral session; these host operations raise no card.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// Exit after the console listener has reported its failure.
    ConsoleListenerStopped,
    /// Dispatcher refusal precedes serving any machine.
    DispatcherRefused,
    /// Process exit tears down every client-owned machine.
    ServiceStop,
    /// Machine-wide state must not come from a user's profile.
    ProgramDataVar,
    /// Host temp is the fallback when `%ProgramData%` is unset.
    ProgramDataFallback,
    /// The impersonated open proves access and pins the folder's name.
    ClientFolderOpen,
    /// Read the resolved name from the held grant without opening another object.
    HeldFolderPath,
}

impl Site for Silent {}
