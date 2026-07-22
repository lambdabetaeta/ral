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
//! - **the machine** ([`vm_manager`]) — the folder is placed by a machine
//!   that answers honestly for what stands around it;
//! - **the surface** — plain language, no git vocabulary anywhere.
//!
//! The design record is `dev/docs/VM/SYNOD.md`.
#![allow(
    clippy::disallowed_methods,
    reason = "synod is an application, not the ral shell; the clippy.toml invariants target ral-core's Shell path/cwd/fs discipline"
)]

pub mod cli;
pub mod grant;
pub mod prompt;
pub mod session;
pub mod workspace;

use clap::Parser;

/// The binary's entry point, lifted into the library so tests can link
/// the whole crate.
///
/// Opens the granted folder, boots a machine to hold it, and hands the
/// pair to [`session::start`].
///
/// # Errors
/// Returns `Err` if the folder cannot be granted, if no machine can be
/// booted for it, or if the session itself fails.
pub fn run() -> Result<(), String> {
    let cli = cli::Cli::parse();
    let grant = grant::Grant::open(&cli.folder)?;
    let machine = vm_manager::detect()
        .boot(&grant.machine_spec())
        .map_err(|e| {
            format!(
                "could not start a machine for {}: {e}",
                grant.root().display()
            )
        })?;
    session::start(&grant, machine.as_ref(), &cli)
}
