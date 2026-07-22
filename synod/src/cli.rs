//! The argv surface: one folder, and nothing else.
//!
//! Synod is not typed at.  This is the internal spawn contract the window
//! opens its child with (`dev/docs/VM/SYNOD-v1.md`, "The increment after:
//! the conversation") — the granted folder, on the command line, once, at
//! spawn.  Every message after that travels down stdin, framed as
//! [`crate::session::read_message`] documents; there is no flag surface
//! left to grow, so this stays plain argv rather than a parser built for
//! one positional argument.

use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug)]
pub struct Cli {
    pub folder: PathBuf,
}

impl Cli {
    /// # Errors
    /// A plain sentence when the window spawned this process with no
    /// folder to work in.
    pub fn parse() -> Result<Self, String> {
        Self::from_args(std::env::args_os().skip(1))
    }

    fn from_args(mut args: impl Iterator<Item = OsString>) -> Result<Self, String> {
        let folder = args
            .next()
            .ok_or_else(|| "synod needs a folder to work in".to_string())?;
        Ok(Self {
            folder: PathBuf::from(folder),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_is_required() {
        Cli::from_args(std::iter::empty()).expect_err("synod cannot run without a folder");
    }

    #[test]
    fn the_one_argument_is_the_folder() {
        let cli = Cli::from_args([OsString::from("/Users/x/Invoices")].into_iter())
            .expect("a folder alone is the whole contract");
        assert_eq!(cli.folder, PathBuf::from("/Users/x/Invoices"));
    }
}
