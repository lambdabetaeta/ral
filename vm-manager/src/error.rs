//! Paths remain lossless `PathBuf`s until the caller supplies spelling to `Error::worded`.
//! Platform causes are text; their paths are carried separately.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

/// Windows reads `TMP` first, and only a folder of the user's own keeps the session private.
pub(crate) const MOVE_TEMP: &str = "Point both the TMP and TEMP environment variables (Windows \
                                    reads TMP first) at a short folder on an NTFS drive that is \
                                    yours alone, such as a new folder directly in your user \
                                    profile, and start synod again";

const UNKEPT: &str = "This backend sends the guest's console to the owning process's standard output \
                      and keeps no copy";

#[derive(Debug, Clone)]
pub enum GuestConsole {
    Kept {
        log: Option<PathBuf>,
        tail: Vec<String>,
    },
    Unkept,
}

impl GuestConsole {
    /// The engine's failure and its retained console, with paths spelled by the caller.
    #[must_use]
    pub fn report(&self, why: &str, spell: Spelling) -> String {
        let mut out = String::new();
        out.push_str("---- Assistant engine lost ----\n");
        out.push_str(why);
        out.push_str("\n\n");
        let tail = match self {
            Self::Unkept => {
                out.push_str(UNKEPT);
                out.push_str(". No last words were retained.\n");
                return out;
            }
            Self::Kept { log, tail } => {
                if let Some(path) = log {
                    let _ = write!(out, "The guest's whole console is at {}.\n\n", spell(path));
                } else {
                    out.push_str("The guest's console could not be kept on disk.\n\n");
                }
                tail
            }
        };
        if tail.is_empty() {
            out.push_str(
                "The guest's console offered no last words. Either it never got as far as a first \
                 message, or this host could not reach the console at all; the line above says \
                 which.\n",
            );
        } else {
            out.push_str("The guest's last words on its console, oldest first:\n");
            for line in tail {
                out.push_str("  ");
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
        out
    }
}

/// Path spelling belongs to the front-end.
pub type Spelling = fn(&Path) -> String;

/// Paths are kept in platform-native units and are worded only by the caller.
#[derive(Debug, Clone)]
pub enum Error {
    NoBackend,
    Unentitled,
    /// Windows' hypervisor API is missing or reports no hypervisor.
    HypervisorPlatformOff,
    NoSuchFolder(PathBuf),
    NotAFolder(PathBuf),
    FolderUnreadable {
        path: PathBuf,
        cause: String,
    },
    MissingBootFile {
        path: PathBuf,
        cause: String,
    },
    NoGuestImage {
        looked_in: Vec<PathBuf>,
    },
    /// Every place `openvmm.exe` was looked for.
    NoVmm {
        looked_in: Vec<PathBuf>,
    },
    CorruptGuestImage {
        archive: PathBuf,
    },
    HostFile {
        file: HostFile,
        act: Act,
        path: PathBuf,
        cause: String,
    },
    /// Lossy spelling could name another file, so non-Unicode hypervisor paths are refused.
    NotUnicode(PathBuf),
    /// Without its `\\?\` head, Win32 would read this path as another.
    NoPlainSpelling(PathBuf),
    /// As `NoPlainSpelling`, for the temporary folder, whose remedy is to move it.
    TempNoPlainSpelling(PathBuf),
    /// macOS requires file URLs; empty paths and embedded NULs are refused.
    NotAFileUrl(PathBuf),
    /// Windows' `AF_UNIX` names a socket in at most 107 bytes.
    SocketPathTooLong {
        path: PathBuf,
    },
    /// The hypervisor's complaint names no file, so the supplied files travel beside it.
    NotConstructed {
        hypervisor: &'static str,
        why: String,
        files: Box<MachineFiles>,
    },
    /// `None` means no console could be opened; an empty retained console is distinct.
    Undialled {
        hypervisor: &'static str,
        wire: Wire,
        within_secs: u64,
        console: Option<GuestConsole>,
        /// The hypervisor's own log, where it keeps one.
        hypervisor_log: Option<PathBuf>,
    },
    /// The guest ended its machine before it dialled `wire`.
    EndedBeforeDial {
        hypervisor: &'static str,
        wire: Wire,
        how: PowerEvent,
        console: Option<GuestConsole>,
        hypervisor_log: Option<PathBuf>,
    },
    ForcedStop {
        hypervisor: &'static str,
        grace_secs: u64,
    },
    /// Platform causes carry no paths; those belong in the error's data.
    Backend {
        hypervisor: &'static str,
        why: String,
    },
    /// `OpenVMM` failed in itself; `None` means its log could not be kept.
    VmmFailed {
        why: String,
        log: Option<PathBuf>,
        tail: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostFile {
    GuestImage,
    /// The checksum describes the inflated image, not its archive.
    ImageChecksum,
    ImageCache,
    SessionDirectory,
    SessionDisk,
    DiskImage,
    MachineCache,
    ConsolePipe,
    ConsoleLog,
    Vmm,
    VmmLog,
    Socket,
}

impl HostFile {
    const fn noun(self) -> &'static str {
        match self {
            Self::GuestImage => "the guest image",
            Self::ImageChecksum => "the guest image checksum",
            Self::ImageCache => "the guest image cache",
            Self::SessionDirectory => "the session directory",
            Self::SessionDisk => "the session disk",
            Self::DiskImage => "the disk image",
            Self::MachineCache => "the machine cache",
            Self::ConsolePipe => "the console pipe",
            Self::ConsoleLog => "the console log",
            Self::Vmm => "OpenVMM's program",
            Self::VmmLog => "OpenVMM's log",
            Self::Socket => "the socket",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Open,
    Read,
    Unpack,
    Write,
    Flush,
    Create,
    Make,
    Place,
    Start,
    Bind,
}

impl Act {
    const fn participle(self) -> &'static str {
        match self {
            Self::Open => "opened",
            Self::Read => "read",
            Self::Unpack => "unpacked",
            Self::Write => "written",
            Self::Flush => "flushed",
            Self::Create => "created",
            Self::Make => "made",
            Self::Place => "moved into place",
            Self::Start => "started",
            Self::Bind => "bound",
        }
    }
}

/// The hypervisor opens these files in its own process; any may cause an unattributed path refusal.
#[derive(Debug, Clone)]
pub struct MachineFiles {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub rootfs: PathBuf,
    pub session: PathBuf,
    pub folder: PathBuf,
}

/// How the guest itself ended its machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    PowerOff,
    Reset,
    TripleFault,
    Watchdog,
}

impl PowerEvent {
    /// A clause whose subject is the guest.
    pub(crate) const fn clause(self) -> &'static str {
        match self {
            Self::PowerOff => "the guest powered itself off",
            Self::Reset => "the guest reset itself",
            Self::TripleFault => "the guest triple-faulted",
            Self::Watchdog => "the guest's watchdog fired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Control,
    Net,
}

impl Wire {
    pub(crate) const fn noun(self) -> &'static str {
        match self {
            Self::Control => "the control plane",
            Self::Net => "the net wire",
        }
    }

    /// What a guest that never dialled this wire was last seen doing.
    const fn stall(self) -> &'static str {
        match self {
            Self::Control => "the kernel may have panicked before its init could run",
            Self::Net => "its init ran, since it dialled the control plane, but went no further",
        }
    }
}

impl Error {
    pub fn backend(hypervisor: &'static str, why: impl Into<String>) -> Self {
        Self::Backend {
            hypervisor,
            why: why.into(),
        }
    }

    pub fn host_file<E: fmt::Display>(
        file: HostFile,
        act: Act,
        path: &Path,
    ) -> impl FnOnce(E) -> Self + use<E> {
        let path = path.to_path_buf();
        move |cause| Self::HostFile {
            file,
            act,
            path,
            cause: cause.to_string(),
        }
    }

    /// This error as a sentence for the person who granted the folder, with
    /// every path in it spelled by `spell`.
    #[must_use]
    pub fn worded(&self, spell: Spelling) -> Worded<'_> {
        Worded { error: self, spell }
    }
}

pub struct Worded<'a> {
    error: &'a Error,
    spell: Spelling,
}

impl fmt::Display for Worded<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spell = self.spell;
        match self.error {
            Error::NoBackend => f.write_str(
                "this computer cannot run synod's virtual machine: it starts on an Apple Silicon \
                 Mac, or on Windows with the Windows Hypervisor Platform",
            ),
            Error::Unentitled => f.write_str(
                "this copy of synod is not signed with the entitlement it needs to start a virtual \
                 machine; ask whoever installed synod for a properly signed build",
            ),
            Error::HypervisorPlatformOff => f.write_str(
                "synod's virtual machine needs the Windows Hypervisor Platform, and it is not \
                 turned on here. Turn it on under 'Turn Windows features on or off' → 'Windows \
                 Hypervisor Platform', or run `Enable-WindowsOptionalFeature -Online -FeatureName \
                 HypervisorPlatform` in an administrator PowerShell; then restart the computer",
            ),
            Error::NoSuchFolder(path) => write!(
                f,
                "there is no folder at {}. Has it been moved or renamed?",
                spell(path)
            ),
            Error::NotAFolder(path) => write!(
                f,
                "{} is a file, not a folder. Please choose the folder that holds the documents to work on.",
                spell(path)
            ),
            Error::FolderUnreadable { path, cause } => write!(
                f,
                "the folder {} could not be opened: {cause}. Do you have permission to open it?",
                spell(path)
            ),
            Error::MissingBootFile { path, cause } => write!(
                f,
                "synod's virtual-machine image is missing a file it needs to start: {} ({cause}). \
                 Ask whoever installed synod to reinstall it.",
                spell(path)
            ),
            Error::NoGuestImage { looked_in } => write!(
                f,
                "no guest image in {}; an installation puts it there; a checkout builds it \
                 with `just vm::boot` and `just vm::rootfs`.",
                listed(looked_in, spell)
            ),
            Error::NoVmm { looked_in } => write!(
                f,
                "OpenVMM, which runs synod's virtual machine, is not at {}; an installation puts \
                 openvmm.exe beside synod.exe; a checkout builds it with \
                 `scripts/build-openvmm.ps1`.",
                listed(looked_in, spell)
            ),
            Error::CorruptGuestImage { archive } => write!(
                f,
                "the guest image unpacked from {} did not match its checksum; the download is corrupt",
                spell(archive)
            ),
            Error::HostFile {
                file,
                act,
                path,
                cause,
            } => write!(
                f,
                "{} {} could not be {}: {cause}",
                file.noun(),
                spell(path),
                act.participle()
            ),
            Error::NotUnicode(path) => write!(
                f,
                "{} cannot be handed to the virtual machine: its name holds characters that are \
                 not valid Unicode, and the machine is told every path as text. Renaming it, or \
                 whichever folder above it has such a name, lets synod use it",
                spell(path)
            ),
            Error::NotAFileUrl(path) => write!(
                f,
                "the path {} cannot be handed to the virtual machine: it is empty or holds a \
                 NUL character, so it has no file URL, and the machine is told every file as one",
                spell(path)
            ),
            Error::NoPlainSpelling(path) => write!(
                f,
                "{} cannot be handed to the virtual machine: OpenVMM is told every path in \
                 Windows' ordinary spelling, and in that spelling Windows reads this one as a \
                 different path (a name ending in a dot or a space, or a device name such as CON \
                 or NUL, is read differently). Renaming it, or whichever folder above it has such \
                 a name, lets synod use it",
                spell(path)
            ),
            Error::TempNoPlainSpelling(path) => write!(
                f,
                "the temporary folder {} cannot hold the virtual machine's session: OpenVMM is \
                 told every path in Windows' ordinary spelling, and in that spelling Windows \
                 reads this one as a different path. {MOVE_TEMP}",
                spell(path)
            ),
            Error::SocketPathTooLong { path } => write!(
                f,
                "the socket {} has a path longer than the 107 bytes Windows allows a local \
                 socket, because the temporary folder's own path is long. {MOVE_TEMP}",
                spell(path)
            ),
            Error::NotConstructed {
                hypervisor,
                why,
                files,
            } => write!(
                f,
                "{hypervisor} would not construct the machine: {why}. The machine's files were: \
                 kernel {}, initramfs {}, rootfs {}, session disk {}, and the granted folder {}",
                spell(&files.kernel),
                spell(&files.initramfs),
                spell(&files.rootfs),
                spell(&files.session),
                spell(&files.folder),
            ),
            Error::Undialled {
                hypervisor,
                wire,
                within_secs,
                console,
                hypervisor_log,
            } => {
                write!(
                    f,
                    "the guest did not dial {} within {within_secs}s of {hypervisor} starting it: \
                     {}. ",
                    wire.noun(),
                    wire.stall()
                )?;
                console_says(f, console.as_ref(), spell)?;
                log_named(f, hypervisor, hypervisor_log.as_deref(), spell)
            }
            Error::EndedBeforeDial {
                hypervisor,
                wire,
                how,
                console,
                hypervisor_log,
            } => {
                write!(
                    f,
                    "{} before {} was dialled, and {hypervisor} ended the machine. ",
                    how.clause(),
                    wire.noun()
                )?;
                console_says(f, console.as_ref(), spell)?;
                log_named(f, hypervisor, hypervisor_log.as_deref(), spell)
            }
            Error::ForcedStop {
                hypervisor,
                grace_secs,
            } => write!(
                f,
                "the guest did not shut down within {grace_secs}s, so {hypervisor} stopped the \
                 machine for it; anything it had not finished writing to the folder may be \
                 missing"
            ),
            Error::Backend { hypervisor, why } => write!(f, "{hypervisor}: {why}"),
            Error::VmmFailed { why, log, tail } => {
                write!(f, "OpenVMM {why}. ")?;
                match (log, tail.as_slice()) {
                    (None, _) => f.write_str("Its own log could not be kept"),
                    (Some(log), []) => write!(f, "Its own log, {}, is empty", spell(log)),
                    (Some(log), tail) => write!(
                        f,
                        "Its own log is {}, which ends {}",
                        spell(log),
                        quoted(tail)
                    ),
                }
            }
        }
    }
}

fn listed(paths: &[PathBuf], spell: Spelling) -> String {
    paths
        .iter()
        .map(|path| spell(path))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A silent retained console, an unkept copy and an unopened console are distinct findings.
fn console_says(
    f: &mut fmt::Formatter<'_>,
    console: Option<&GuestConsole>,
    spell: Spelling,
) -> fmt::Result {
    let (log, tail) = match console {
        None => return f.write_str("The machine layer could not open a console"),
        Some(GuestConsole::Unkept) => return f.write_str(UNKEPT),
        Some(GuestConsole::Kept { log, tail }) => (log, tail),
    };
    let kept = log.as_deref().map_or_else(
        || "and its console could not be kept on disk either".to_string(),
        |path| format!("and its whole console log is at {}", spell(path)),
    );
    if tail.is_empty() {
        write!(
            f,
            "The guest said nothing at all on its console, so it never got as far as a first \
             message: {kept}"
        )
    } else {
        write!(
            f,
            "The guest's last words on its console were {}: {kept}",
            quoted(tail)
        )
    }
}

fn log_named(
    f: &mut fmt::Formatter<'_>,
    hypervisor: &str,
    log: Option<&Path>,
    spell: Spelling,
) -> fmt::Result {
    log.map_or(Ok(()), |log| {
        write!(f, ". {hypervisor}'s own log is at {}", spell(log))
    })
}

fn quoted(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("\u{201c}{}\u{201d}", line.trim()))
        .collect::<Vec<_>>()
        .join(" / ")
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn marked(path: &Path) -> String {
        format!(
            "<{}>",
            path.file_name()
                .map_or("", |name| name.to_str().unwrap_or(""))
        )
    }

    #[test]
    fn every_path_is_spelled_by_the_front_end() {
        let error = Error::NotConstructed {
            hypervisor: "OpenVMM",
            why: "the system cannot find the path specified".to_string(),
            files: Box::new(MachineFiles {
                kernel: PathBuf::from("kernel"),
                initramfs: PathBuf::from("initramfs.img"),
                rootfs: PathBuf::from("rootfs.img"),
                session: PathBuf::from("session.img"),
                folder: PathBuf::from("Invoices"),
            }),
        };
        let text = error.worded(marked).to_string();
        for name in [
            "<kernel>",
            "<initramfs.img>",
            "<rootfs.img>",
            "<session.img>",
            "<Invoices>",
        ] {
            assert!(text.contains(name), "{name} in: {text}");
        }
    }
}
