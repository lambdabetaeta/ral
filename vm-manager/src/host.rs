//! The machine that isn't one: the granted folder, used where it lies.
//!
//! This is the backend that works today, and on a Linux development box
//! with no hypervisor it is the only one.  It boots nothing, copies
//! nothing, and mounts nothing; [`Machine::workspace_path`] hands back the
//! granted folder itself.  What it does do is the honest part of a boot:
//! it checks the spec, resolves the folder, and reports
//! [`Boundary::None`] so nobody downstream can mistake it for a wall.

use std::path::{Path, PathBuf};

use crate::{Boundary, Error, Hypervisor, Machine, MachineSpec};

/// The backend that runs the agent on this computer, with no machine
/// between them.
pub struct Host;

impl Hypervisor for Host {
    fn name(&self) -> &'static str {
        "this computer"
    }

    fn boundary(&self) -> Boundary {
        Boundary::None
    }

    fn boot(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>, Error> {
        Ok(Box::new(InPlace {
            workspace: spec.resolve()?,
        }))
    }
}

/// A workspace held open in place.  The resource limits of the spec do not
/// apply here — there is no machine to apply them to — and the guest mount
/// point is unused, because there is no guest.
pub struct InPlace {
    workspace: PathBuf,
}

impl Machine for InPlace {
    fn workspace_path(&self) -> &Path {
        &self.workspace
    }

    fn boundary(&self) -> Boundary {
        Boundary::None
    }

    /// There is no wire to hand back: the folder is worked where it lies, so
    /// there is no inside for a control plane to cross into.
    #[cfg(unix)]
    fn take_control(&mut self) -> Option<std::os::fd::OwnedFd> {
        None
    }

    fn shutdown(self: Box<Self>) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: test scaffolding builds the folders it then grants; no shell, no turn, no card."
)]
mod tests {
    use super::*;
    use crate::Workspace;

    #[test]
    fn works_in_the_granted_folder_itself() {
        let dir = tempfile::tempdir().unwrap();
        let machine = Host.boot(&MachineSpec::for_folder(dir.path())).unwrap();

        assert_eq!(
            machine.workspace_path(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
        assert_eq!(machine.boundary(), Boundary::None);
        assert!(!machine.boundary().is_hardware());
        machine.shutdown().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn there_is_no_control_plane_to_hand_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut machine = Host.boot(&MachineSpec::for_folder(dir.path())).unwrap();
        assert!(machine.take_control().is_none());
    }

    #[test]
    fn the_workspace_path_is_real_and_absolute() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("papers")).unwrap();
        let winding = dir.path().join("papers").join("..").join("papers");

        let machine = Host.boot(&MachineSpec::for_folder(winding)).unwrap();
        let path = machine.workspace_path();

        assert!(path.is_absolute());
        assert!(path.ends_with("papers"));
    }

    #[test]
    fn detect_gives_a_backend_that_boots_here() {
        let dir = tempfile::tempdir().unwrap();
        let hypervisor = crate::detect();
        let machine = hypervisor
            .boot(&MachineSpec::for_folder(dir.path()))
            .unwrap();

        assert_eq!(hypervisor.boundary(), machine.boundary());
        assert!(!hypervisor.name().is_empty());
    }

    #[test]
    fn a_read_only_grant_still_boots_and_still_admits_it_cannot_enforce() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = MachineSpec::for_folder(dir.path());
        spec.workspace.read_only = true;

        assert_eq!(Host.boot(&spec).unwrap().boundary(), Boundary::None);
    }

    #[test]
    fn a_missing_folder_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let spec = MachineSpec::for_folder(dir.path().join("minutes"));

        let message = Host.boot(&spec).err().unwrap().to_string();
        assert!(message.contains("there is no folder at"), "{message}");
        assert!(message.contains("minutes"), "{message}");
    }

    #[test]
    fn a_file_is_not_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("letter.docx");
        std::fs::write(&file, "Dear Vice-Chancellor,").unwrap();

        let message = Host
            .boot(&MachineSpec::for_folder(file))
            .err()
            .unwrap()
            .to_string();
        assert!(message.contains("is a file, not a folder"), "{message}");
    }

    #[test]
    fn a_relative_guest_mount_point_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let spec = MachineSpec {
            workspace: Workspace {
                host_path: dir.path().to_path_buf(),
                guest_path: PathBuf::from("work"),
                read_only: false,
            },
            ..MachineSpec::for_folder(dir.path())
        };

        let message = spec.resolve().unwrap_err().to_string();
        assert!(message.contains("full path such as /work"), "{message}");
    }

    #[test]
    fn a_machine_needs_a_processor() {
        let dir = tempfile::tempdir().unwrap();
        let spec = MachineSpec {
            vcpus: 0,
            ..MachineSpec::for_folder(dir.path())
        };

        assert!(
            spec.resolve()
                .unwrap_err()
                .to_string()
                .contains("at least one processor")
        );
    }

    #[test]
    fn a_machine_needs_enough_memory() {
        let dir = tempfile::tempdir().unwrap();
        let spec = MachineSpec {
            memory_mib: 64,
            ..MachineSpec::for_folder(dir.path())
        };

        let message = spec.resolve().unwrap_err().to_string();
        assert!(message.contains("at least 256 MB"), "{message}");
        assert!(message.contains("only 64 MB"), "{message}");
    }

    #[test]
    fn the_spec_is_judged_before_the_folder_is_touched() {
        let spec = MachineSpec {
            vcpus: 0,
            ..MachineSpec::for_folder("/nowhere/at/all")
        };

        assert!(matches!(spec.resolve(), Err(Error::NoProcessors)));
    }

    #[test]
    fn both_boundaries_describe_themselves_to_a_reader() {
        assert!(Boundary::Hardware.to_string().contains("nothing else"));
        assert!(Boundary::None.to_string().contains("in software"));
        assert!(Boundary::Hardware.is_hardware());
    }
}
