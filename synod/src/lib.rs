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

pub mod grant;
pub mod prompt;
pub mod session;
#[cfg(test)]
pub(crate) mod test_fixture;
pub mod workspace;

/// The boot media this build of synod ships, if this computer holds it: the
/// kernel, initramfs, and rootfs the virtual machine boots
/// (`dev/docs/VM/SYNOD.md` §7).
///
/// Two layouts are looked in, mirroring how the window finds its own
/// bundle: the shipped bundle keeps all three under
/// `Contents/Resources/boot/`, beside the binary's own `Contents/MacOS/`;
/// a development build reaches the image pipeline's own output, found by
/// walking up to the workspace `target` directory.  `None` — no media
/// anywhere — is not an error this function raises: [`vm_manager::detect`]
/// turns it into the refusal a synod with nothing to boot must give.
pub(crate) fn boot_media() -> Option<vm_manager::BootArtifact> {
    let media = |boot: &std::path::Path, rootfs: std::path::PathBuf| {
        let artifact = vm_manager::BootArtifact {
            kernel: boot.join("kernel"),
            initramfs: boot.join("initramfs.img"),
            rootfs,
        };
        let complete = [&artifact.kernel, &artifact.initramfs, &artifact.rootfs]
            .into_iter()
            .all(|file| file.is_file());
        complete.then_some(artifact)
    };

    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    if let Some(found) = dir.parent().and_then(|contents| {
        let boot = contents.join("Resources").join("boot");
        media(&boot, boot.join("rootfs.img"))
    }) {
        return Some(found);
    }
    let mut cursor = dir;
    while let Some(parent) = cursor.parent() {
        if parent.file_name().is_some_and(|name| name == "target") {
            let out = parent.parent()?.join("vm-image").join("out");
            return media(&out.join("boot"), out.join("rootfs.img"));
        }
        cursor = parent;
    }
    None
}
