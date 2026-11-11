//! What the `ral` process does: argv, stdin and tty, rc and login, the REPL, signals, `ral-sh`.

mod audit;
mod check;
mod diagnostics;
mod examples;
mod invocation;
mod login;
pub(crate) mod pipeline;
mod pipeline_windows;
mod posix_bridge;
mod prompt;
mod repl;
mod signals;
mod wait_for_port;
mod workers;
