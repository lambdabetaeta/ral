//! Foreground ownership: lending the controlling terminal to a child's group
//! for a run, and taking it back.  `unix` and `windows` exist only on their
//! own platform, so neither can be linked from this page.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{TerminalLoan, interrupt_foreground_child, termios_snapshot};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::TerminalLoan;
