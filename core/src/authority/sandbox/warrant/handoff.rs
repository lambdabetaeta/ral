//! The descriptors a confined launch hands its re-exec, each at the number it
//! already holds: the [`Slot`]s, the [`Handoff`] that lifts and lends them,
//! the [`parcel`] that carries bytes no other process can write, and the
//! re-exec's sweep of everything else it inherited.

use crate::Role;
#[cfg(target_os = "linux")]
use crate::fact::Count;
use crate::fact::{
    DescriptorSweepFailure, DescriptorSweepFailureSetReportCloexec, HandoffField, LaunchFailure,
    OsFailure, ParcelCause, ParcelFailure, ParcelRole, ParcelStep, WarrantFailure,
    WarrantFailureSlot, WarrantFailureSlotCount,
};
#[cfg(target_os = "macos")]
use crate::fact::{
    DescriptorSweepFailureBufferTooLarge, DescriptorSweepFailureCloseDescriptor,
    DescriptorSweepFailurePartialDescriptorRecord, ParcelCauseSocketTooSmall,
};
#[cfg(target_os = "linux")]
use crate::fact::{
    DescriptorSweepFailureCloseRange, KernelFeatureFailure, StandardStream, UnavailableFailure,
    WarrantFailureStreamInstall,
};
use crate::path::shown;
#[cfg(target_os = "linux")]
use crate::site::{Silent, site};
use libc::c_int;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The number a handed-down descriptor holds, the same in parent and child:
/// from [`Self::FIRST`] up, away from stdio. Only a
/// [`Handoff`] makes one, or a child parsing what its parent wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::authority::sandbox) struct Slot(c_int);

impl Slot {
    /// The floor for copies made by `lift`, not for the child's sweep.
    pub(in crate::authority::sandbox) const FIRST: c_int = 98;

    pub(in crate::authority::sandbox) const fn fd(self) -> c_int {
        self.0
    }

    /// The slot `field` spells as [`fmt::Display`] does: plain digits, from
    /// [`Self::FIRST`] up.
    pub(in crate::authority::sandbox) fn parse(
        field: &str,
        kind: HandoffField,
    ) -> Result<Self, WarrantFailure> {
        (field.parse::<c_int>().ok())
            .filter(|&at| at >= Self::FIRST && at.to_string() == field)
            .map(Self)
            .ok_or_else(|| {
                WarrantFailure::Slot(WarrantFailureSlot {
                    field: kind,
                    spelling: field.to_owned(),
                })
            })
    }

    /// The descriptor the launch left here, now the caller's; `None` where
    /// nothing arrived.
    pub(in crate::authority::sandbox) fn take(self) -> Option<OwnedFd> {
        let at = self.fd();
        // SAFETY: `F_GETFD` only asks whether the slot is open.
        (unsafe { libc::fcntl(at, libc::F_GETFD) } >= 0)
            // SAFETY: open, and lent by the launch to this one taker.
            .then(|| unsafe { OwnedFd::from_raw_fd(at) })
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The trampoline's warrant and report; under bwrap, its payload streams
/// travel separately from the envelope's standard descriptors.
#[derive(Clone, Copy, Debug)]
pub(in crate::authority::sandbox) struct Slots {
    pub(in crate::authority::sandbox) warrant: Slot,
    pub(in crate::authority::sandbox) report: Slot,
    #[cfg(target_os = "linux")]
    pub(in crate::authority::sandbox) streams: Option<[Slot; 3]>,
}

impl Slots {
    /// The warrant and report, followed by stream slots for an envelope.
    pub(in crate::authority::sandbox) fn argv(self) -> Vec<String> {
        let args = vec![
            Role::Warrant.flag().to_owned(),
            self.warrant.to_string(),
            self.report.to_string(),
        ];
        #[cfg(target_os = "linux")]
        let args = args
            .into_iter()
            .chain(self.streams.into_iter().flatten().map(|at| at.to_string()))
            .collect();
        args
    }

    /// Direct trampolines (macOS and the Linux confinement harness) inherit
    /// stdio; an envelope names three extra slots.
    pub(super) fn parse(args: &[OsString]) -> Result<Self, WarrantFailure> {
        let slot = |arg: &OsString, kind| Slot::parse(&shown(arg), kind);
        #[cfg(target_os = "linux")]
        let (args, streams) = match args {
            [_, _, input, output, error] => (
                &args[..2],
                Some([
                    slot(input, HandoffField::Stream(StandardStream::Stdin))?,
                    slot(output, HandoffField::Stream(StandardStream::Stdout))?,
                    slot(error, HandoffField::Stream(StandardStream::Stderr))?,
                ]),
            ),
            args => (args, None),
        };
        let [warrant, report] = args else {
            return Err(WarrantFailure::SlotCount(WarrantFailureSlotCount {
                actual: args.len().into(),
            }));
        };
        let (warrant, report) = (
            slot(warrant, HandoffField::Warrant)?,
            slot(report, HandoffField::Report)?,
        );
        let all = [warrant, report];
        #[cfg(target_os = "linux")]
        let all = all
            .into_iter()
            .chain(streams.into_iter().flatten())
            .collect::<Vec<_>>();
        for (index, at) in all.iter().enumerate() {
            if all[..index].contains(at) {
                return Err(WarrantFailure::DuplicateSlot(at.fd()));
            }
        }
        Ok(Self {
            warrant,
            report,
            #[cfg(target_os = "linux")]
            streams,
        })
    }

    /// Install only in the trampoline branch, before the inherited-fd sweep.
    #[cfg(target_os = "linux")]
    pub(super) fn install_streams(self) -> Result<(), WarrantFailure> {
        let Some(streams) = self.streams else {
            return Ok(());
        };
        for (target, source) in [0, 1, 2].into_iter().zip(streams) {
            // SAFETY: distinct high handoff slots, never the destinations.
            if unsafe { libc::dup2(source.fd(), target) } < 0 {
                return Err(WarrantFailure::StreamInstall(WarrantFailureStreamInstall {
                    stream: match target {
                        0 => StandardStream::Stdin,
                        1 => StandardStream::Stdout,
                        _ => StandardStream::Stderr,
                    },
                    source: source.fd(),
                    cause: OsFailure::from(&io::Error::last_os_error()),
                }));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
const SEALS: rustix::fs::SealFlags = rustix::fs::SealFlags::SHRINK
    .union(rustix::fs::SealFlags::GROW)
    .union(rustix::fs::SealFlags::WRITE)
    .union(rustix::fs::SealFlags::SEAL);

/// `bytes` behind a descriptor no other process can write.  Linux: a sealed
/// memfd, since a same-uid process may reopen any descriptor of ours through
/// `/proc` — sealed, the copy it gets is as read-only as ours.  Reopenable
/// until sealed, so the bytes are read back and checked once nothing can
/// change them; the memfd is left at offset 0, where bwrap reads.
#[cfg(target_os = "linux")]
pub(in crate::authority::sandbox) fn parcel(
    name: &str,
    bytes: &[u8],
    role: ParcelRole,
) -> Result<OwnedFd, ParcelFailure> {
    use rustix::fs::{MemfdFlags, fcntl_add_seals, memfd_create};
    use std::io::{Read, Seek};
    let failed = |step, e: io::Error| parcel_io(role.clone(), step, &e);
    let mut file = std::fs::File::from(
        memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
            .map_err(|e| failed(ParcelStep::Create, e.into()))?,
    );
    file.write_all(bytes)
        .map_err(|e| failed(ParcelStep::Write, e))?;
    fcntl_add_seals(&file, SEALS).map_err(|e| failed(ParcelStep::Seal, e.into()))?;
    file.rewind().map_err(|e| failed(ParcelStep::Rewind, e))?;
    let mut sealed = Vec::with_capacity(bytes.len());
    (&file)
        .take(bytes.len() as u64 + 1)
        .read_to_end(&mut sealed)
        .map_err(|e| failed(ParcelStep::ReadBack, e))?;
    if sealed != bytes {
        return Err(ParcelFailure {
            role,
            step: ParcelStep::ReadBack,
            cause: ParcelCause::ChangedBytes,
        });
    }
    file.rewind().map_err(|e| failed(ParcelStep::Rewind, e))?;
    Ok(file.into())
}

/// macOS parcels are filled and closed before any child exists.
#[cfg(target_os = "macos")]
pub(in crate::authority::sandbox) fn parcel(
    _name: &str,
    bytes: &[u8],
    role: ParcelRole,
) -> Result<OwnedFd, ParcelFailure> {
    use rustix::net::sockopt::{set_socket_recv_buffer_size, set_socket_send_buffer_size};
    const SLACK: usize = 64 << 10;
    let (reader, mut writer) = crate::process::cloexec_socketpair()
        .map_err(|e| parcel_io(role.clone(), ParcelStep::Create, &e))?;
    let unfit = |step, e: io::Error| ParcelFailure {
        role: role.clone(),
        step,
        cause: ParcelCause::SocketTooSmall(ParcelCauseSocketTooSmall {
            bytes: bytes.len().into(),
            cause: Box::new(crate::io::failure(&e)),
        }),
    };
    let room = bytes.len() + SLACK;
    set_socket_send_buffer_size(&writer, room)
        .map_err(|e| unfit(ParcelStep::SetSendBuffer, e.into()))?;
    set_socket_recv_buffer_size(&reader, room)
        .map_err(|e| unfit(ParcelStep::SetReceiveBuffer, e.into()))?;
    writer
        .set_nonblocking(true)
        .map_err(|e| parcel_io(role.clone(), ParcelStep::SetNonblocking, &e))?;
    writer
        .write_all(bytes)
        .map_err(|e| unfit(ParcelStep::Write, e))?;
    Ok(reader.into())
}

pub(in crate::authority::sandbox) fn parcel_io(
    role: ParcelRole,
    step: ParcelStep,
    e: &io::Error,
) -> ParcelFailure {
    ParcelFailure {
        role,
        step,
        cause: ParcelCause::Io(Box::new(crate::io::failure(e))),
    }
}

/// Check the channel before reading any bytes.
#[cfg(target_os = "linux")]
pub(super) fn unwritable(fd: &OwnedFd) -> Result<(), WarrantFailure> {
    let seals = rustix::fs::fcntl_get_seals(fd)
        .map_err(|e| WarrantFailure::SealProbe(OsFailure::from(&io::Error::from(e))))?;
    if !seals.contains(SEALS) {
        return Err(WarrantFailure::MissingSeals);
    }
    rustix::fs::seek(fd, rustix::fs::SeekFrom::Start(0))
        .map(drop)
        .map_err(|e| WarrantFailure::Rewind(OsFailure::from(&io::Error::from(e))))
}

#[cfg(target_os = "macos")]
pub(super) fn unwritable(fd: &OwnedFd) -> Result<(), WarrantFailure> {
    use std::os::fd::AsFd;
    match crate::process::report::is_socket(fd.as_fd()) {
        Ok(true) => Ok(()),
        Ok(false) => Err(WarrantFailure::NotSocket),
        Err(e) => Err(WarrantFailure::SocketStat(OsFailure::from(&e))),
    }
}

/// What a confined launch hands its child: copies lifted to free numbers
/// from [`Slot::FIRST`] up, each crossing at the number it holds here.
/// Lending displaces nothing. Stream capture fills reserved slots only, so
/// no descriptor the child still needs — `std`'s exec-error pipe, a later
/// hook's — can be displaced.
#[derive(Default)]
pub(in crate::authority::sandbox) struct Handoff {
    lent: Vec<(OwnedFd, ParcelRole)>,
    kept: Vec<(Slot, ParcelRole)>,
}

impl Handoff {
    /// Lend the child a copy of `fd`, owned here until the spawn.
    ///
    /// # Errors
    /// A lift the descriptor limit refuses.
    pub(in crate::authority::sandbox) fn lend(
        &mut self,
        fd: BorrowedFd<'_>,
        role: ParcelRole,
    ) -> Result<Slot, ParcelFailure> {
        let (copy, at) = lift(fd).map_err(|e| parcel_io(role.clone(), ParcelStep::Lend, &e))?;
        self.lent.push((copy, role));
        Ok(at)
    }

    /// Hand the child a copy of `fd` that the caller owns, to close when its
    /// closing is a signal; it must stay open past the spawn, whose hook
    /// names its number.
    ///
    /// # Errors
    /// As [`Self::lend`].
    pub(in crate::authority::sandbox) fn keep(
        &mut self,
        fd: BorrowedFd<'_>,
        role: ParcelRole,
    ) -> Result<(OwnedFd, Slot), ParcelFailure> {
        let (copy, at) = lift(fd).map_err(|e| parcel_io(role.clone(), ParcelStep::Keep, &e))?;
        self.kept.push((at, role));
        Ok((copy, at))
    }

    /// Reserve the payload's slots now, fill them only after `std` lowers
    /// stdio at spawn. bwrap's monitor and init sweep all extra fds except
    /// their own event/status/sync fds (`close_extra_fds` in bubblewrap);
    /// only its exec branch retains these slots. Never lend them as 0–2.
    #[cfg(target_os = "linux")]
    pub(in crate::authority::sandbox) fn streams(
        &mut self,
    ) -> Result<(crate::process::launch::EnvelopeStdio, [Slot; 3]), ParcelFailure> {
        use std::os::fd::AsFd;
        let null = crate::path::open_discard()
            .map_err(|e| parcel_io(ParcelRole::EnvelopeNullStreams, ParcelStep::Create, &e))?;
        let (input, input_at) = self.keep(
            null.as_fd(),
            ParcelRole::PayloadStream(StandardStream::Stdin),
        )?;
        let (output, output_at) = self.keep(
            null.as_fd(),
            ParcelRole::PayloadStream(StandardStream::Stdout),
        )?;
        let (error, error_at) = self.keep(
            null.as_fd(),
            ParcelRole::PayloadStream(StandardStream::Stderr),
        )?;
        // Unlike a pipe, this remains writable after a detached launch's
        // parent is gone. It carries only envelope startup diagnostics;
        // the bounded read in file_said does not bound the file's storage.
        let diagnostics = std::fs::File::from(
            rustix::fs::memfd_create("ral-envelope-stderr", rustix::fs::MemfdFlags::CLOEXEC)
                .map_err(|e| {
                    parcel_io(
                        ParcelRole::EnvelopeDiagnostics,
                        ParcelStep::Create,
                        &e.into(),
                    )
                })?,
        );
        // Replacement endpoints stay CLOEXEC: only their dup2 copies at 0–2
        // belong to bwrap. Lift them away from closed inherited stdio too.
        let (null, _) = lift(null.as_fd())
            .map_err(|e| parcel_io(ParcelRole::EnvelopeNullStreams, ParcelStep::Lift, &e))?;
        let (writer, _) = lift(diagnostics.as_fd())
            .map_err(|e| parcel_io(ParcelRole::EnvelopeDiagnostics, ParcelStep::Lift, &e))?;
        Ok((
            crate::process::launch::EnvelopeStdio {
                payload: [input, output, error],
                null,
                error: writer,
                diagnostics,
            },
            [input_at, output_at, error_at],
        ))
    }

    /// Hand `cmd`'s child every slot, through one `pre_exec`.
    pub(in crate::authority::sandbox) fn install(
        self,
        cmd: &mut Command,
        evidence: &mut crate::process::launch::setup::Evidence,
    ) {
        use crate::process::launch::setup::Context;
        let Self { lent, kept } = self;
        let lent: Vec<_> = lent
            .into_iter()
            .map(|(fd, role)| {
                let check = evidence.check(Context::Parcel(role, ParcelStep::Lend));
                (fd, check)
            })
            .collect();
        let kept: Vec<_> = kept
            .into_iter()
            .map(|(at, role)| {
                let check = evidence.check(Context::Parcel(role, ParcelStep::Keep));
                (at, check)
            })
            .collect();
        // SAFETY: post-fork, `fcntl` alone, which is async-signal-safe, on
        // numbers open here at the fork: the lent copies, which the hook
        // owns, and the kept ones, which their owners hold past the spawn.
        unsafe {
            cmd.pre_exec(move || {
                let lent = lent.iter().map(|(fd, check)| (fd.as_raw_fd(), check));
                for (at, check) in lent.chain(kept.iter().map(|(at, check)| (at.0, check))) {
                    if libc::fcntl(at, libc::F_SETFD, 0) < 0 {
                        return check.run(Err(io::Error::last_os_error()));
                    }
                }
                Ok(())
            });
        }
    }
}

/// A close-on-exec copy of `fd` at the lowest free number from
/// [`Slot::FIRST`] up, and that number.
fn lift(fd: BorrowedFd<'_>) -> io::Result<(OwnedFd, Slot)> {
    let copy = rustix::io::fcntl_dupfd_cloexec(fd, Slot::FIRST)?;
    let at = Slot(copy.as_raw_fd());
    Ok((copy, at))
}

/// Close every descriptor above stderr but `keep`, which stays close-on-exec;
/// the warrant role must open nothing before this.
///
/// # Errors
/// A kernel that cannot sweep: the program would hold the payload's copy
/// of the `--info-fd` writer, and with it the parent's spawn, for its whole
/// life.
#[cfg(target_os = "linux")]
pub(super) fn close_inherited(keep: Slot) -> Result<(), LaunchFailure> {
    let at = libc::c_uint::try_from(keep.fd()).expect("a slot is positive");
    close_range(3, at - 1)?;
    close_range(at + 1, libc::c_uint::MAX)?;
    cloexec(keep)
}

#[cfg(target_os = "linux")]
fn close_range(first: libc::c_uint, last: libc::c_uint) -> Result<(), LaunchFailure> {
    // SAFETY: the warrant role owns no descriptors in the range here.
    let swept = unsafe {
        site!(
            Silent::InheritedSweep,
            libc::syscall(libc::SYS_close_range, first, last, 0 as libc::c_uint)
        )
    };
    if swept == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    Err(if e.raw_os_error() == Some(libc::ENOSYS) {
        LaunchFailure::Unavailable(UnavailableFailure::Kernel(
            KernelFeatureFailure::CloseRangeAbsent,
        ))
    } else {
        sweep(DescriptorSweepFailure::CloseRange(
            DescriptorSweepFailureCloseRange {
                first: Count::from_u64(u64::from(first)),
                last: Count::from_u64(u64::from(last)),
                cause: OsFailure::from(&e),
            },
        ))
    })
}

/// The report's slot, closed by the program's `execve`.
fn cloexec(keep: Slot) -> Result<(), LaunchFailure> {
    // SAFETY: `F_SETFD` only sets the slot's close-on-exec flag.
    if unsafe { libc::fcntl(keep.fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(sweep(DescriptorSweepFailure::SetReportCloexec(
            DescriptorSweepFailureSetReportCloexec {
                fd: keep.fd(),
                cause: OsFailure::from(&io::Error::last_os_error()),
            },
        )));
    }
    Ok(())
}

/// Close every descriptor above stderr but `keep`, which stays close-on-exec;
/// the warrant role must open nothing before this.
#[cfg(target_os = "macos")]
pub(super) fn close_inherited(keep: Slot) -> Result<(), LaunchFailure> {
    let mut entries = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        64
    ];
    let count = loop {
        let size = c_int::try_from(std::mem::size_of_val(entries.as_slice())).map_err(|_| {
            sweep(DescriptorSweepFailure::BufferTooLarge(
                DescriptorSweepFailureBufferTooLarge {
                    bytes: std::mem::size_of_val(entries.as_slice()).into(),
                },
            ))
        })?;
        // SAFETY: entries is writable for size bytes; this only lists our descriptors.
        let listed = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0,
                entries.as_mut_ptr().cast(),
                size,
            )
        };
        if listed <= 0 {
            return Err(sweep(DescriptorSweepFailure::ListDescriptors(
                OsFailure::from(&io::Error::last_os_error()),
            )));
        }
        let listed = usize::try_from(listed).expect("a positive descriptor-list size fits usize");
        let width = std::mem::size_of::<libc::proc_fdinfo>();
        if !listed.is_multiple_of(width) {
            return Err(sweep(DescriptorSweepFailure::PartialDescriptorRecord(
                DescriptorSweepFailurePartialDescriptorRecord {
                    bytes: listed.into(),
                    width: width.into(),
                },
            )));
        }
        if listed < std::mem::size_of_val(entries.as_slice()) {
            break listed / width;
        }
        entries.resize(
            entries.len() * 2,
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            },
        );
    };
    for entry in &entries[..count] {
        if entry.proc_fd <= 2 || entry.proc_fd == keep.fd() {
            continue;
        }
        // SAFETY: the warrant role owns no descriptors above stderr here.
        if unsafe { libc::close(entry.proc_fd) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EBADF) {
                return Err(sweep(DescriptorSweepFailure::CloseDescriptor(
                    DescriptorSweepFailureCloseDescriptor {
                        fd: entry.proc_fd,
                        cause: OsFailure::from(&error),
                    },
                )));
            }
        }
    }
    cloexec(keep)
}

fn sweep(cause: DescriptorSweepFailure) -> LaunchFailure {
    LaunchFailure::Warrant(WarrantFailure::DescriptorSweep(cause))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn failed_handoff_hooks_reconstruct_lend_and_keep_in_the_parent() {
        use std::os::fd::AsFd;
        for keep in [false, true] {
            let source = crate::path::open_discard().unwrap();
            let mut handoff = Handoff::default();
            let (held, at, step) = if keep {
                let (fd, at) = handoff.keep(source.as_fd(), ParcelRole::Warrant).unwrap();
                (Some(fd), at, ParcelStep::Keep)
            } else {
                (
                    None,
                    handoff.lend(source.as_fd(), ParcelRole::Warrant).unwrap(),
                    ParcelStep::Lend,
                )
            };
            let mut command = Command::new("/bin/true");
            // SAFETY: close only the child's handed-down copy before the handoff hook.
            unsafe {
                command.pre_exec(move || {
                    libc::close(at.fd());
                    Ok(())
                });
            }
            let mut evidence = crate::process::launch::setup::Evidence::new().unwrap();
            handoff.install(&mut command, &mut evidence);
            let launch =
                crate::process::Launch::from_command(command).with_setup_evidence(evidence);
            let why = launch
                .spawn(
                    crate::process::Placement::Inherit,
                    &crate::process::DurableRoot::default().child(),
                )
                .err()
                .expect("F_SETFD on the closed descriptor fails");
            drop(held);
            let crate::process::Unstarted::Refused(error) = why else {
                panic!("handoff failure is ral's refusal");
            };
            assert_eq!(
                error.reason,
                crate::fact::Reason::Launch(Box::new(LaunchFailure::Parcel(ParcelFailure {
                    role: ParcelRole::Warrant,
                    step,
                    cause: ParcelCause::Io(Box::new(crate::io::failure(
                        &io::Error::from_raw_os_error(libc::EBADF)
                    ))),
                })))
            );
            assert!(error.describe().contains("cannot prepare a descriptor"));
            assert!(!error.describe().contains("copy"));
        }
    }

    /// One descriptor cannot be both the warrant and the report: each is
    /// taken as an owner of its slot, so both would close it.
    #[test]
    fn a_warrant_and_a_report_cannot_share_a_slot() {
        let args = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        let err = Slots::parse(&args(&["101", "101"])).expect_err("one slot twice");
        assert_eq!(err, WarrantFailure::DuplicateSlot(101));
        let both = Slots::parse(&args(&["101", "102"])).expect("two slots");
        assert_eq!(both.argv()[1..], ["101", "102"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn payload_stream_slots_cannot_alias_each_other_or_the_warrant_or_report() {
        let fields = ["101", "102", "103", "104", "105"].map(OsString::from);
        let slots = Slots::parse(&fields).expect("five distinct owners");
        assert_eq!(slots.argv()[1..], ["101", "102", "103", "104", "105"]);
        for target in 2..5 {
            for source in 0..target {
                let mut aliased = fields.clone();
                aliased[target] = fields[source].clone();
                assert!(Slots::parse(&aliased).is_err(), "{aliased:?}");
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_closed_inherited_stream_refuses_envelope_preparation() {
        use crate::process::launch::EnvelopedLaunch;
        use crate::process::report::Report;
        use crate::process::{Placement, Unstarted};
        let mut handoff = Handoff::default();
        let (stdio, _) = handoff.streams().expect("reserve stream slots");
        let mut cmd = Command::new("/bin/sh");
        let mut evidence = crate::process::launch::setup::Evidence::new().unwrap();
        handoff.install(&mut cmd, &mut evidence);
        // SAFETY: close is async-signal-safe, and affects only this child.
        unsafe {
            cmd.pre_exec(|| {
                libc::close(0);
                Ok(())
            });
        }
        let (reader, writer) = crate::process::cloexec_socketpair().expect("report socket");
        let launch = EnvelopedLaunch::new(
            cmd,
            stdio,
            crate::fact::LauncherRole::Bwrap,
            None,
            Report::new(reader, writer.into()),
        )
        .with_setup_evidence(evidence);
        let result = launch.spawn(
            Placement::Inherit,
            &crate::process::DurableRoot::default().child(),
        );
        let Err(Unstarted::Refused(why)) = result else {
            panic!("a closed stream must refuse preparation");
        };
        assert!(matches!(&why.reason, crate::fact::Reason::Launch(failure)
        if matches!(**failure, LaunchFailure::Prepare(crate::fact::PrepareFailure {
            step: crate::fact::PrepareStep::CapturePayloadStreams, ..
        }))));
        assert!(
            why.describe()
                .contains("preserve the payload's standard streams"),
            "{why}"
        );
        assert!(why.describe().contains("os error 9"), "{why}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn envelope_capture_uses_final_redirections_before_low_nofile() {
        use crate::process::StdioSpec;
        use crate::process::launch::EnvelopedLaunch;
        use crate::process::report::{Report, Witness};
        let mut handoff = Handoff::default();
        let (stdio, [_, output, error]) = handoff.streams().expect("reserve stream slots");
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            &format!(
                "test \"$(ulimit -n)\" = 64 && test ! -p /proc/self/fd/0 && test ! -p /proc/self/fd/1 || exit 1; \
             printf OUT > /proc/self/fd/{output}; printf ERR > /proc/self/fd/{error}"
            ),
        ]);
        let mut evidence = crate::process::launch::setup::Evidence::new().unwrap();
        handoff.install(&mut cmd, &mut evidence);
        let (reader, writer) = crate::process::cloexec_socketpair().expect("report socket");
        let witness = Witness::new(writer.try_clone().expect("clone report writer").into())
            .expect("a report is a socket");
        drop(witness.started());
        let mut launch = EnvelopedLaunch::new(
            cmd,
            stdio,
            crate::fact::LauncherRole::Bwrap,
            None,
            Report::new(reader, writer.into()),
        )
        .with_setup_evidence(evidence);
        launch.launch_mut().stdin(StdioSpec::Null);
        // Installation must merge and fill the high slots before lowering
        // the descriptor limit.
        launch
            .launch_mut()
            .limit_session(crate::process::ChildLimits {
                open_files: Some(64),
                ..Default::default()
            });
        launch.launch_mut().dup_stdout_to_stderr();
        let output = launch.output().expect("spawn the envelope stand-in");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"OUTERR");
        assert!(output.stderr.is_empty(), "{output:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn envelope_diagnostics_survive_a_null_payload_stderr() {
        use crate::process::launch::EnvelopedLaunch;
        use crate::process::report::Report;
        use crate::process::{Placement, StdioSpec, Unstarted};
        use std::os::fd::AsFd;
        let mut handoff = Handoff::default();
        let (stdio, _) = handoff.streams().expect("reserve stream slots");
        let (reader, writer) = crate::process::cloexec_socketpair().expect("report socket");
        let (kept, _) = handoff
            .keep(writer.as_fd(), ParcelRole::ReportSocket)
            .expect("keep report until fork");
        drop(writer);
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "printf ENVELOPE-REFUSED >&2; exit 1"]);
        let mut evidence = crate::process::launch::setup::Evidence::new().unwrap();
        handoff.install(&mut cmd, &mut evidence);
        let mut launch = EnvelopedLaunch::new(
            cmd,
            stdio,
            crate::fact::LauncherRole::Bwrap,
            None,
            Report::new(reader, kept),
        )
        .with_setup_evidence(evidence);
        launch
            .launch_mut()
            .stdin(StdioSpec::Null)
            .stdout(StdioSpec::Null)
            .stderr(StdioSpec::Null);
        let result = launch.spawn(
            Placement::Inherit,
            &crate::process::DurableRoot::default().child(),
        );
        let Err(Unstarted::Refused(why)) = result else {
            panic!("an envelope that never reports must refuse");
        };
        assert!(why.describe().contains("ENVELOPE-REFUSED"), "{why}");
    }

    /// Every free number below the first slot, held as a host with that
    /// many descriptors open holds them: what opens next lands among the
    /// slots.
    fn below_first_held() -> Vec<std::fs::File> {
        std::iter::repeat_with(|| {
            std::fs::File::open("/dev/null").expect("open /dev/null (is `ulimit -n` below 98?)")
        })
        .take_while(|null| null.as_raw_fd() < Slot::FIRST)
        .collect()
    }

    /// A descriptor opened after the handoff and written by a later hook,
    /// as `detach`'s handshake is, crosses whole beside the lent one, with
    /// every number below the slots busy.  A concurrent test freeing a low
    /// number weakens this witness, never its verdict.
    #[test]
    fn a_later_hooks_descriptor_crosses_the_handoff_whole() {
        use std::os::fd::AsFd;
        let held = below_first_held();
        let (lent, mut filler) = crate::process::cloexec_pipe().expect("a pipe");
        filler.write_all(b"lent").expect("fill the pipe");
        drop(filler);
        let mut handoff = Handoff::default();
        let at = handoff
            .lend(lent.as_fd(), ParcelRole::Warrant)
            .expect("lends");
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &format!("cat /dev/fd/{at}")]);
        let mut evidence = crate::process::launch::setup::Evidence::new().unwrap();
        handoff.install(&mut cmd, &mut evidence);
        let (mut receipt, handshake) = crate::process::cloexec_pipe().expect("a pipe");
        let fd = handshake.as_raw_fd();
        // SAFETY: post-fork, `write` alone, as `detach`'s hook does.
        unsafe {
            cmd.pre_exec(move || {
                let _ = libc::write(fd, b"!".as_ptr().cast(), 1);
                Ok(())
            });
        }
        let out = crate::process::output(&mut cmd).expect("spawn sh");
        drop((handshake, held));
        let mut byte = [0; 1];
        receipt
            .read_exact(&mut byte)
            .expect("the later hook's write must arrive");
        assert_eq!(&byte, b"!");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "lent", "{out:?}");
    }

    /// `std`'s exec-error pipe opens inside the spawn, after every hook, so
    /// with every number below the slots busy it lands among them: the
    /// handoff leaves it whole, and a program that cannot run is the
    /// spawn's error, not a child that died of an abort.
    #[test]
    fn a_program_that_cannot_run_after_a_handoff_is_the_spawns_error() {
        use std::os::fd::AsFd;
        let held = below_first_held();
        let stderr = io::stderr();
        let mut handoff = Handoff::default();
        handoff
            .lend(stderr.as_fd(), ParcelRole::EnvelopeDiagnostics)
            .expect("lends");
        let mut cmd = Command::new("/ral/no/such/program");
        let mut evidence = crate::process::launch::setup::Evidence::new().unwrap();
        handoff.install(&mut cmd, &mut evidence);
        let spawned = crate::process::spawn(&mut cmd).map(|mut child| child.wait());
        drop(held);
        let err = spawned.expect_err("a program that cannot run must not spawn");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
