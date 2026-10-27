//! PID 1 supervises engines and reaps orphans without interpreting their protocol.
//! Root exit or a fatal worker failure ends the session; the root is never restarted.

mod fleet;
mod halt;
mod serve;

pub(crate) use serve::serve;
