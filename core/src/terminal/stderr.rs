//! The shell's own stderr lines and the print macros behind them.
//!
//! This sits below the renderer: nothing here reaches `diagnostic`, so any
//! layer may print.

use super::stderr_color;
use crate::ansi::{BOLD_RED, BOLD_YELLOW, RESET};

/// Print `{cmd}: {msg}` to stderr.
pub fn cmd_error(cmd: &str, msg: &str) {
    if stderr_color() {
        crate::errln!("{BOLD_RED}{cmd}{RESET}: {msg}");
    } else {
        crate::errln!("{cmd}: {msg}");
    }
}

/// Print `warning: {msg}` to stderr.
pub fn shell_warning(msg: &str) {
    if stderr_color() {
        crate::errln!("{BOLD_YELLOW}warning{RESET}: {msg}");
    } else {
        crate::errln!("warning: {msg}");
    }
}

// `println!` and `eprintln!` panic when the write fails, and once the terminal
// hangs up every write does, with EIO.  The shell's own output goes through
// these instead, which drop the failed write: there is no one left to tell.

/// `println!`, dropping a failed write.
#[macro_export]
macro_rules! outln {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stdout(), $($arg)*);
    }};
}

/// `eprint!`, dropping a failed write.
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::write!(::std::io::stderr(), $($arg)*);
    }};
}

/// `eprintln!`, dropping a failed write.
#[macro_export]
macro_rules! errln {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr(), $($arg)*);
    }};
}

// Call sites of `dbg_trace!` are permanent instrumentation, not stray print
// statements; leave them in.

/// Emit a `[[DEBUG] tag]` line to stderr; nothing at all in release builds.
#[cfg(debug_assertions)]
#[macro_export]
macro_rules! dbg_trace {
    ($tag:expr, $($arg:tt)*) => {
        if $crate::terminal::stderr_color() {
            $crate::errln!("\x1b[1;91m[[DEBUG] {}]\x1b[0m {}", $tag, format!($($arg)*))
        } else {
            $crate::errln!("[[DEBUG] {}] {}", $tag, format!($($arg)*))
        }
    };
}

#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! dbg_trace {
    ($tag:expr, $($arg:tt)*) => {
        ()
    };
}
