//! The wire carrier's two halves: [`WireTransport`], the front-end's, and
//! `run_engine`, the engine process's.

mod front;
#[cfg(unix)]
mod serve;

pub use front::WireTransport;
#[cfg(unix)]
pub use serve::run_engine;
