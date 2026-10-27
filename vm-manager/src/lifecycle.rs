#![allow(
    clippy::disallowed_macros,
    reason = "[silent:vm-host] a host aside with no ral shell to print through"
)]

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::error::{Act, Error, GuestConsole, HostFile, Wire};
use crate::machine::{Socket, Wires};
use crate::site::{Silent, site};

const BOOT_TIMEOUT: Duration = Duration::from_mins(1);
const STOP_GRACE: Duration = Duration::from_secs(5);
const STOP_PULSE: Duration = Duration::from_millis(50);
const REMOVE_GRACE: Duration = Duration::from_secs(2);
const REMOVE_PULSE: Duration = Duration::from_millis(100);
const ORPHAN_AGE: Duration = Duration::from_hours(1);
const LEASE: &str = "lease";

pub(crate) enum Leaf {
    Raw,
    Vhd,
}

impl Leaf {
    const fn name(self) -> &'static str {
        match self {
            Self::Raw => "session.img",
            Self::Vhd => "session.vhd",
        }
    }
}

pub(crate) fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

pub(crate) fn aside_spelling(path: &Path) -> String {
    site!(Silent::AsideSpelling, path => display()).to_string()
}

pub(crate) enum Missed {
    Timeout(String),
    Failed(Error),
}

pub(crate) fn await_dials(
    hypervisor: &'static str,
    mut accept: impl FnMut(Wire, Duration) -> Result<Socket, Missed>,
    console: impl FnOnce() -> Option<GuestConsole>,
) -> Result<Wires, Error> {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut dial = |wire| {
        accept(wire, deadline.saturating_duration_since(Instant::now()))
            .map_err(|missed| Box::new((wire, missed)))
    };
    let outcome =
        dial(Wire::Control).and_then(|control| dial(Wire::Net).map(|net| Wires { control, net }));
    outcome.map_err(|failure| match *failure {
        (wire, Missed::Timeout(cause)) => Error::Undialled {
            hypervisor: hypervisor.into(),
            wire,
            within_secs: BOOT_TIMEOUT.as_secs(),
            cause,
            console: console(),
        },
        (_, Missed::Failed(error)) => error,
    })
}

pub(crate) fn stop_within(
    hypervisor: &'static str,
    request: impl FnOnce(),
    mut stopped: impl FnMut() -> Result<bool, Error>,
    force: impl FnOnce(),
) -> Result<(), Error> {
    request();
    let deadline = Instant::now() + STOP_GRACE;
    loop {
        if stopped()? {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            force();
            return Err(Error::ForcedStop {
                hypervisor: hypervisor.into(),
                grace_secs: STOP_GRACE.as_secs(),
            });
        }
        thread::sleep(STOP_PULSE.min(left));
    }
}

pub(crate) struct SessionDisk {
    dir: PathBuf,
    disk: PathBuf,
    lease: Option<File>,
}

impl SessionDisk {
    pub(crate) fn create(
        parent: &Path,
        leaf: Leaf,
        make: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> Result<Self, Error> {
        site!(Silent::SessionDiskCreate, fs::create_dir_all(parent)).map_err(Error::host_file(
            HostFile::SessionDirectory,
            Act::Make,
            parent,
        ))?;
        let dir = tempfile::Builder::new()
            .prefix(&format!(
                "ral-vm-session-{}-{}-",
                unix_seconds(),
                std::process::id()
            ))
            .tempdir_in(parent)
            .map_err(Error::host_file(
                HostFile::SessionDirectory,
                Act::Create,
                parent,
            ))?
            .keep();
        let mut session = Self {
            disk: dir.join(leaf.name()),
            dir,
            lease: None,
        };
        let lease_path = session.dir.join(LEASE);
        let lease = site!(Silent::SessionDiskCreate, OpenOptions::new().read(true).write(true).create_new(true) => open(&lease_path))
            .map_err(Error::host_file(HostFile::SessionDirectory, Act::Create, &lease_path))?;
        lease
            .try_lock()
            .map_err(io::Error::from)
            .map_err(Error::host_file(
                HostFile::SessionDirectory,
                Act::Create,
                &lease_path,
            ))?;
        session.lease = Some(lease);
        let mut disk = site!(Silent::SessionDiskCreate, OpenOptions::new().write(true).create_new(true) => open(&session.disk))
            .map_err(Error::host_file(HostFile::SessionDisk, Act::Create, &session.disk))?;
        make(&mut disk).map_err(Error::host_file(
            HostFile::SessionDisk,
            Act::Write,
            &session.disk,
        ))?;
        Ok(session)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.disk
    }
}

impl Drop for SessionDisk {
    fn drop(&mut self) {
        let outcome = remove_disk(&self.disk);
        let disk_removed = outcome.is_ok();
        drop(self.lease.take());
        let outcome = outcome
            .and_then(|()| remove_file(&self.dir.join(LEASE)))
            .and_then(|()| site!(Silent::SessionDiskRelease, fs::remove_dir(&self.dir)));
        if let Err(why) = outcome {
            if disk_removed {
                eprintln!(
                    "synod: the session directory {} could not be deleted — {why}. The session \
                     disk is gone; you may delete the directory yourself",
                    aside_spelling(&self.dir),
                );
                return;
            }
            let bytes = site!(Silent::SessionDiskMeasure, fs::metadata(&self.disk))
                .map_or(0, |file| file.len());
            eprintln!(
                "synod: the session disk {} could not be deleted after {}s — {why}. It is {} MiB that \
                 nothing will ever read again; the next backend to start sweeps up disks \
                 like it once they are an hour old, or you may delete it yourself",
                aside_spelling(&self.disk),
                REMOVE_GRACE.as_secs(),
                bytes / (1024 * 1024),
            );
        }
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    match site!(Silent::SessionDiskRelease, fs::remove_file(path)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn remove_disk(path: &Path) -> io::Result<()> {
    let deadline = Instant::now() + REMOVE_GRACE;
    loop {
        match remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(REMOVE_PULSE),
        }
    }
}

pub(crate) fn sweep(parent: &Path) {
    let Ok(entries) = site!(Silent::SessionDiskSweep, fs::read_dir(parent)) else {
        return;
    };
    let (mut swept, mut reclaimed) = (0, 0);
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() || is_reparse(&entry) {
            continue;
        }
        let name = entry.file_name();
        let Some(born) = site!(Silent::SessionDiskSweep, &name => to_str()).and_then(session_epoch)
        else {
            continue;
        };
        if unix_seconds().saturating_sub(born) < ORPHAN_AGE.as_secs() {
            continue;
        }
        if let Ok(bytes) = sweep_directory(&entry.path()) {
            swept += 1;
            reclaimed += bytes;
        }
    }
    if swept > 0 {
        eprintln!(
            "synod: {swept} session disk(s) from earlier runs were still in {} — deleted, {} MiB back",
            aside_spelling(parent),
            reclaimed / (1024 * 1024),
        );
    }
}

fn session_epoch(name: &str) -> Option<u64> {
    let mut parts = name.strip_prefix("ral-vm-session-")?.splitn(3, '-');
    let epoch = parts.next()?.parse().ok()?;
    parts.next()?.parse::<u32>().ok()?;
    (!parts.next()?.is_empty()).then_some(epoch)
}

fn sweep_directory(dir: &Path) -> io::Result<u64> {
    let mut disk = None;
    let mut lease = None;
    let mut empty = true;
    for entry in site!(Silent::SessionDiskSweep, fs::read_dir(dir))? {
        let entry = entry?;
        empty = false;
        if !entry.file_type()?.is_file() || is_reparse(&entry) {
            return Err(io::Error::other(
                "session directory contains a link or non-file entry",
            ));
        }
        let name = entry.file_name();
        if name == LEASE {
            lease = Some(entry.path());
        } else if (name == Leaf::Raw.name() || name == Leaf::Vhd.name()) && disk.is_none() {
            disk = Some(entry.path());
        } else {
            return Err(io::Error::other(
                "session directory contains an unknown entry",
            ));
        }
    }
    if empty {
        site!(Silent::SessionDiskSweep, fs::remove_dir(dir))?;
        return Ok(0);
    }
    let lease = lease.ok_or_else(|| io::Error::other("session directory has no lease"))?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let probe = site!(Silent::SessionDiskSweep, options => open(&lease))?;
    if !probe.metadata()?.is_file() {
        return Err(io::Error::other("session lease is not a regular file"));
    }
    probe.try_lock().map_err(io::Error::from)?;
    drop(probe);
    let bytes = if let Some(disk) = disk {
        let bytes = site!(Silent::SessionDiskMeasure, fs::metadata(&disk))?.len();
        site!(Silent::SessionDiskSweep, fs::remove_file(&disk))?;
        bytes
    } else {
        0
    };
    site!(Silent::SessionDiskSweep, fs::remove_file(&lease))?;
    site!(Silent::SessionDiskSweep, fs::remove_dir(dir))?;
    Ok(bytes)
}

fn is_reparse(entry: &fs::DirEntry) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        entry.metadata().map_or(true, |meta| {
            meta.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
        })
    }
    #[cfg(not(windows))]
    {
        let _ = entry;
        false
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn abandoned(parent: &Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(LEASE), b"").unwrap();
        fs::write(dir.join(Leaf::Raw.name()), b"disk").unwrap();
        dir
    }

    #[test]
    fn a_live_lease_keeps_an_aged_directory() {
        let parent = tempfile::tempdir().unwrap();
        let dir = abandoned(parent.path(), "ral-vm-session-0-123-live");
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(LEASE))
            .unwrap();
        lease.try_lock().unwrap();
        sweep(parent.path());
        assert!(dir.join(Leaf::Raw.name()).exists());
        drop(lease);
    }

    #[test]
    fn an_abandoned_directory_and_an_empty_one_are_reclaimed() {
        let parent = tempfile::tempdir().unwrap();
        let dir = abandoned(parent.path(), "ral-vm-session-0-123-abandoned");
        let empty = parent.path().join("ral-vm-session-0-123-empty");
        fs::create_dir(&empty).unwrap();
        sweep(parent.path());
        assert!(!dir.exists());
        assert!(!empty.exists());
    }

    #[test]
    fn a_young_directory_is_kept() {
        let parent = tempfile::tempdir().unwrap();
        let dir = abandoned(
            parent.path(),
            &format!("ral-vm-session-{}-123-young", unix_seconds()),
        );
        sweep(parent.path());
        assert!(dir.join(Leaf::Raw.name()).exists());
    }

    #[test]
    fn unknown_contents_and_malformed_names_are_kept() {
        let parent = tempfile::tempdir().unwrap();
        let dir = abandoned(parent.path(), "ral-vm-session-0-123-unknown");
        fs::write(dir.join("unknown"), b"keep").unwrap();
        let malformed = abandoned(parent.path(), "ral-vm-session-0-abc-malformed");
        sweep(parent.path());
        for dir in [&dir, &malformed] {
            assert!(dir.join(Leaf::Raw.name()).exists());
            assert!(dir.join(LEASE).exists());
        }
        assert!(dir.join("unknown").exists());
        for name in [
            "ral-vm-session-0-123",
            "ral-vm-session-0-123-",
            "ral-vm-session-later-123-suffix",
        ] {
            assert_eq!(session_epoch(name), None);
        }
    }

    #[test]
    fn a_linked_directory_and_its_target_are_kept() {
        let parent = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::write(target.path().join("precious"), b"keep").unwrap();
        let link = parent.path().join("ral-vm-session-0-123-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(target.path(), &link).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_dir(target.path(), &link) {
            assert_eq!(
                error.raw_os_error(),
                Some(1314),
                "unexpected symlink refusal: {error}"
            );
            return;
        }
        sweep(parent.path());
        assert!(fs::symlink_metadata(link).is_ok());
        assert_eq!(fs::read(target.path().join("precious")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn linked_leases_and_disks_are_kept() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("target");
        fs::write(&target, b"keep").unwrap();
        for leaf in [LEASE, Leaf::Raw.name()] {
            let dir = parent.path().join(format!("ral-vm-session-0-123-{leaf}"));
            fs::create_dir(&dir).unwrap();
            std::os::unix::fs::symlink(&target, dir.join(leaf)).unwrap();
        }
        sweep(parent.path());
        assert_eq!(fs::read(target).unwrap(), b"keep");
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 3);
    }

    #[test]
    fn two_creates_have_distinct_directories_and_release_them() {
        let parent = tempfile::tempdir().unwrap();
        let first = SessionDisk::create(parent.path(), Leaf::Raw, |disk| disk.set_len(8)).unwrap();
        let second = SessionDisk::create(parent.path(), Leaf::Raw, |disk| disk.set_len(8)).unwrap();
        assert_ne!(first.path().parent(), second.path().parent());
        assert_eq!(first.path().metadata().unwrap().len(), 8);
        drop((first, second));
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_failed_writer_leaves_no_directory() {
        let parent = tempfile::tempdir().unwrap();
        let result = SessionDisk::create(parent.path(), Leaf::Raw, |_| {
            Err(io::Error::other("writer failed"))
        });
        assert!(matches!(
            result,
            Err(Error::HostFile {
                act: Act::Write,
                ..
            })
        ));
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_timeout_carries_the_wire_and_console() {
        let result = await_dials(
            "test",
            |wire, patience| {
                assert!(matches!(wire, Wire::Control));
                assert!(patience <= BOOT_TIMEOUT);
                Err(Missed::Timeout("no dial".into()))
            },
            || Some(GuestConsole::Unkept),
        );
        assert!(matches!(
            result,
            Err(Error::Undialled {
                wire: Wire::Control,
                within_secs: 60,
                console: Some(GuestConsole::Unkept),
                ..
            })
        ));
    }

    #[test]
    fn a_failed_dial_preserves_its_error() {
        let result = await_dials(
            "test",
            |_, _| Err(Missed::Failed(Error::NoBackend)),
            || panic!("no console for a backend error"),
        );
        assert!(matches!(result, Err(Error::NoBackend)));
    }

    #[cfg(unix)]
    #[test]
    fn both_dials_share_one_deadline_and_the_net_timeout_names_net() {
        let mut first_patience = None;
        let result = await_dials(
            "test",
            |wire, patience| match wire {
                Wire::Control => {
                    first_patience = Some(patience);
                    Ok(std::os::unix::net::UnixStream::pair().unwrap().0.into())
                }
                Wire::Net => {
                    assert!(patience <= first_patience.unwrap());
                    Err(Missed::Timeout("net absent".into()))
                }
            },
            || None,
        );
        assert!(matches!(
            result,
            Err(Error::Undialled {
                wire: Wire::Net,
                ..
            })
        ));
    }

    #[test]
    fn stopping_requests_before_polling_and_never_forces_a_stopped_machine() {
        let requested = Cell::new(false);
        stop_within(
            "test",
            || requested.set(true),
            || {
                assert!(requested.get());
                Ok(true)
            },
            || panic!("already stopped"),
        )
        .unwrap();
        let result = stop_within(
            "test",
            || {},
            || Err(Error::NoBackend),
            || panic!("a state error must not force"),
        );
        assert!(matches!(result, Err(Error::NoBackend)));
    }

    #[cfg(windows)]
    #[test]
    fn a_held_disk_is_released_after_the_worker_closes_it() {
        use std::os::windows::fs::OpenOptionsExt;
        let parent = tempfile::tempdir().unwrap();
        let session = SessionDisk::create(parent.path(), Leaf::Vhd, |_| Ok(())).unwrap();
        let held = OpenOptions::new()
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .open(session.path())
            .unwrap();
        let worker = thread::spawn(move || {
            thread::sleep(REMOVE_PULSE * 3);
            drop(held);
        });
        drop(session);
        worker.join().unwrap();
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_stop_deadline_forces_once_and_reports_the_grace() {
        let forced = Cell::new(0);
        let result = stop_within("test", || {}, || Ok(false), || forced.set(forced.get() + 1));
        assert_eq!(forced.get(), 1);
        assert!(matches!(
            result,
            Err(Error::ForcedStop {
                hypervisor,
                grace_secs: 5,
            }) if hypervisor == "test"
        ));
    }
}
