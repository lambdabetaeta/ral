use std::io;
use std::path::PathBuf;

#[cfg(any(target_os = "macos", windows))]
use guest_abi::boot::{Boot, DEFAULT_ENGINE, Export, Net, command_line};
#[cfg(any(target_os = "macos", windows))]
use guest_abi::ports;

use crate::error::Error;
use crate::site::{Silent, site};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSpec {
    pub folder: PathBuf,
    pub read_only: bool,
}

pub struct Resources {
    pub vcpus: u8,
    pub memory_mib: u32,
}

pub const SIZE: Resources = Resources {
    vcpus: 4,
    memory_mib: 6144,
};

impl MachineSpec {
    /// # Errors
    /// Returns a folder refusal.
    pub fn resolve(&self) -> Result<PathBuf, Error> {
        let path = &self.folder;
        let root =
            site!(Silent::GrantedFolderResolve, std::fs::canonicalize(path)).map_err(|cause| {
                match cause.kind() {
                    io::ErrorKind::NotFound => Error::NoSuchFolder(path.clone()),
                    _ => Error::FolderUnreadable {
                        path: path.clone(),
                        cause: cause.to_string(),
                    },
                }
            })?;
        let is_folder = site!(Silent::GrantedFolderStat, &root => is_dir());
        if is_folder {
            Ok(root)
        } else {
            Err(Error::NotAFolder(root))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootArtifact {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    /// The read-only rootfs image.
    pub rootfs: PathBuf,
}

impl BootArtifact {
    /// # Errors
    /// Returns [`Error::MissingBootFile`] for the first missing or unreadable file.
    pub fn resolve(&self) -> Result<Self, Error> {
        let resolve_one = |path: &PathBuf| {
            site!(Silent::BootMediaResolve, std::fs::canonicalize(path)).map_err(|cause| {
                Error::MissingBootFile {
                    path: path.clone(),
                    cause: cause.to_string(),
                }
            })
        };
        Ok(Self {
            kernel: resolve_one(&self.kernel)?,
            initramfs: resolve_one(&self.initramfs)?,
            rootfs: resolve_one(&self.rootfs)?,
        })
    }
}

#[cfg(any(target_os = "macos", windows))]
pub(crate) fn boot_line(workspace: Export, console: &str, epoch: u64) -> String {
    command_line(
        &Boot {
            workspace,
            port: ports::CONTROL,
            epoch: i64::try_from(epoch).unwrap_or(i64::MAX),
            engine: DEFAULT_ENGINE.into(),
            net: Some(Net::STANDARD),
        },
        console,
    )
}
