//! Synod — an office-work delegate over one granted folder.
//!
//! Where [`exarch`] is a coding agent pointed at a repository, synod is
//! pointed at a *folder*: the spreadsheets, letters, and PDFs of someone
//! who does not program.  The two are siblings over one engine — synod
//! reuses exarch's provider transport, agent turn driver, and card bus
//! wholesale, and differs in exactly five places:
//!
//! - **the grant** ([`grant`]) — one folder, nothing else, no network;
//! - **the prompt** ([`prompt`]) — an office persona over an office toolbox;
//! - **the safety net** ([`workspace`]) — the folder is recorded before a
//!   job, the changes reported after it, and anything can be put back;
//! - **the machine** ([`vm_manager`]) — the folder is placed inside a real
//!   virtual machine, walled off from the rest of the computer;
//! - **the surface** — plain language, no git vocabulary anywhere.
//!
//! Synod is one crate in two halves: the library modules here — the engine
//! anyone could drive — and the desktop shell (the binary's `shell` module,
//! rooted at `main.rs`) that is the only thing that drives it.  The shell is
//! an agent itself, in the sense that it starts a machine and talks to it,
//! in-process, over [`session::Conversation`].  There is no command line and
//! nothing here is ever typed at directly.
//!
//! The design record is `dev/docs/VM/SYNOD.md`.
#![allow(
    clippy::disallowed_methods,
    reason = "synod is an application, not the ral shell; the clippy.toml invariants target ral-core's Shell path/cwd/fs discipline"
)]

pub mod boot;
pub mod grant;
pub mod prompt;
pub mod session;
#[cfg(test)]
pub(crate) mod test_fixture;
pub mod workspace;
