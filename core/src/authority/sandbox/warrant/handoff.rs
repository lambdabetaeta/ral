//! The descriptors a confined launch hands its re-exec, each at the number it
//! already holds: the [`Slot`]s, the [`Handoff`] that lifts and lends them,
//! the [`parcel`] that carries bytes no other process can write, and the
//! re-exec's sweep of everything else it inherited.

use crate::Role;
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
    pub(in crate::authority::sandbox) fn parse(field: &str) -> Result<Self, String> {
        (field.parse::<c_int>().ok())
            .filter(|&at| at >= Self::FIRST && at.to_string() == field)
            .map(Self)
            .ok_or_else(|| {
                format!(
                    "{field:?} names no handoff descriptor, which is a number from {} up \
                     in plain digits",
                    Self::FIRST
                )
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
    pub(super) fn parse(args: &[OsString]) -> Result<Self, String> {
        let slot = |arg: &OsString| Slot::parse(&shown(arg));
        #[cfg(target_os = "linux")]
        let (args, streams) = match args {
            [_, _, input, output, error] => (
                &args[..2],
                Some([slot(input)?, slot(output)?, slot(error)?]),
            ),
            args => (args, None),
        };
        let [warrant, report] = args else {
            return Err(format!(
                "{} takes the warrant and report descriptors, plus three stream descriptors \
                 under a Linux envelope; it is how ral starts its own confined children, \
                 never a command to run by hand",
                Role::Warrant.flag()
            ));
        };
        let (warrant, report) = (slot(warrant)?, slot(report)?);
        let all = [warrant, report];
        #[cfg(target_os = "linux")]
        let all = all
            .into_iter()
            .chain(streams.into_iter().flatten())
            .collect::<Vec<_>>();
        for (index, at) in all.iter().enumerate() {
            if all[..index].contains(at) {
                return Err(format!(
                    "two handoff owners name fd {at}; one would be closed twice"
                ));
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
    pub(super) fn install_streams(self) -> Result<(), String> {
        let Some(streams) = self.streams else {
            return Ok(());
        };
        for (target, source) in [0, 1, 2].into_iter().zip(streams) {
            // SAFETY: distinct high handoff slots, never the destinations.
            if unsafe { libc::dup2(source.fd(), target) } < 0 {
                return Err(format!(
                    "cannot install payload stream {target} from fd {source}: {}",
                    io::Error::last_os_error()
                ));
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
pub(in crate::authority::sandbox) fn parcel(name: &str, bytes: &[u8]) -> io::Result<OwnedFd> {
    use rustix::fs::{MemfdFlags, fcntl_add_seals, memfd_create};
    use std::io::{Read, Seek};
    let mut file = std::fs::File::from(memfd_create(
        name,
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?);
    file.write_all(bytes)?;
    fcntl_add_seals(&file, SEALS)?;
    file.rewind()?;
    let mut sealed = Vec::with_capacity(bytes.len());
    (&file)
        .take(bytes.len() as u64 + 1)
        .read_to_end(&mut sealed)?;
    if sealed != bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the parcel changed before it was sealed: did another process open ral's \
             descriptors through /proc?",
        ));
    }
    file.rewind()?;
    Ok(file.into())
}

/// macOS, which has no `/proc`: the read end of a socketpair whose write end
/// is filled and closed here, before any child exists.  Whole or not at all —
/// a socket too small for `bytes` is an error, never a partial parcel.
#[cfg(target_os = "macos")]
pub(in crate::authority::sandbox) fn parcel(_name: &str, bytes: &[u8]) -> io::Result<OwnedFd> {
    use rustix::net::sockopt::{set_socket_recv_buffer_size, set_socket_send_buffer_size};
    const SLACK: usize = 64 << 10;
    let (reader, mut writer) = crate::process::cloexec_socketpair()?;
    let unfit = |e: io::Error| {
        io::Error::new(
            e.kind(),
            format!("a {}-byte parcel does not fit a socket: {e}", bytes.len()),
        )
    };
    let room = bytes.len() + SLACK;
    set_socket_send_buffer_size(&writer, room).map_err(|e| unfit(e.into()))?;
    set_socket_recv_buffer_size(&reader, room).map_err(|e| unfit(e.into()))?;
    writer.set_nonblocking(true)?;
    writer.write_all(bytes).map_err(unfit)?;
    Ok(reader.into())
}

/// The child's half of [`parcel`]'s promise, checked before a byte is read.
#[cfg(target_os = "linux")]
pub(super) fn unwritable(fd: &OwnedFd) -> Result<(), String> {
    let seals = rustix::fs::fcntl_get_seals(fd)
        .map_err(|e| format!("the warrant is not a sealed memfd: {e}"))?;
    if !seals.contains(SEALS) {
        return Err("the warrant's memfd is not sealed against writes; refusing it".to_string());
    }
    rustix::fs::seek(fd, rustix::fs::SeekFrom::Start(0))
        .map(drop)
        .map_err(|e| format!("cannot rewind the warrant: {e}"))
}

#[cfg(target_os = "macos")]
pub(super) fn unwritable(fd: &OwnedFd) -> Result<(), String> {
    use std::os::fd::AsFd;
    match crate::process::report::is_socket(fd.as_fd()) {
        Ok(true) => Ok(()),
        Ok(false) => Err("the warrant is not a socket; refusing it".to_string()),
        Err(e) => Err(format!("cannot stat the warrant: {e}")),
    }
}

/// What a confined launch hands its child: copies lifted to free numbers
/// from [`Slot::FIRST`] up, each crossing at the number it holds here.
/// Lending displaces nothing. Stream capture fills reserved slots only, so
/// no descriptor the child still needs — `std`'s exec-error pipe, a later
/// hook's — can be displaced.
#[derive(Default)]
pub(in crate::authority::sandbox) struct Handoff {
    lent: Vec<OwnedFd>,
    kept: Vec<Slot>,
}

impl Handoff {
    /// Lend the child a copy of `fd`, owned here until the spawn.
    ///
    /// # Errors
    /// A lift the descriptor limit refuses.
    pub(in crate::authority::sandbox) fn lend(
        &mut self,
        fd: BorrowedFd<'_>,
    ) -> Result<Slot, String> {
        let (copy, at) = lift(fd)?;
        self.lent.push(copy);
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
    ) -> Result<(OwnedFd, Slot), String> {
        let (copy, at) = lift(fd)?;
        self.kept.push(at);
        Ok((copy, at))
    }

    /// Reserve the payload's slots now, fill them only after `std` lowers
    /// stdio at spawn. bwrap's monitor and init sweep all extra fds except
    /// their own event/status/sync fds (`close_extra_fds` in bubblewrap);
    /// only its exec branch retains these slots. Never lend them as 0–2.
    #[cfg(target_os = "linux")]
    pub(in crate::authority::sandbox) fn streams(
        &mut self,
    ) -> Result<(crate::process::launch::EnvelopeStdio, [Slot; 3]), String> {
        use std::os::fd::AsFd;
        let null = crate::path::open_discard()
            .map_err(|e| format!("sandbox: cannot open envelope null streams: {e}"))?;
        let (input, input_at) = self.keep(null.as_fd())?;
        let (output, output_at) = self.keep(null.as_fd())?;
        let (error, error_at) = self.keep(null.as_fd())?;
        // Unlike a pipe, this remains writable after a detached launch's
        // parent is gone. It carries only envelope startup diagnostics;
        // the bounded read in file_said does not bound the file's storage.
        let diagnostics = std::fs::File::from(
            rustix::fs::memfd_create("ral-envelope-stderr", rustix::fs::MemfdFlags::CLOEXEC)
                .map_err(|e| format!("sandbox: cannot open envelope diagnostics: {e}"))?,
        );
        // Replacement endpoints stay CLOEXEC: only their dup2 copies at 0–2
        // belong to bwrap. Lift them away from closed inherited stdio too.
        let (null, _) = lift(null.as_fd())?;
        let (writer, _) = lift(diagnostics.as_fd())?;
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
    pub(in crate::authority::sandbox) fn install(self, cmd: &mut Command) {
        let Self { lent, kept } = self;
        // SAFETY: post-fork, `fcntl` alone, which is async-signal-safe, on
        // numbers open here at the fork: the lent copies, which the hook
        // owns, and the kept ones, which their owners hold past the spawn.
        unsafe {
            cmd.pre_exec(move || {
                let lent = lent.iter().map(AsRawFd::as_raw_fd);
                for at in lent.chain(kept.iter().map(|at| at.0)) {
                    if libc::fcntl(at, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
}

/// A close-on-exec copy of `fd` at the lowest free number from
/// [`Slot::FIRST`] up, and that number.
fn lift(fd: BorrowedFd<'_>) -> Result<(OwnedFd, Slot), String> {
    let copy = rustix::io::fcntl_dupfd_cloexec(fd, Slot::FIRST).map_err(|e| {
        format!(
            "sandbox: cannot copy a descriptor to {} or above for the confined child: \
             {e}; is the open-file limit below that? Check it with `sh -c 'ulimit -n'`, and raise it for the session with the rc's `limits: [open-files: N]`",
            Slot::FIRST
        )
    })?;
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
pub(super) fn close_inherited(keep: Slot) -> Result<(), String> {
    let at = libc::c_uint::try_from(keep.fd()).expect("a slot is positive");
    close_range(3, at - 1)?;
    close_range(at + 1, libc::c_uint::MAX)?;
    cloexec(keep)
}

#[cfg(target_os = "linux")]
fn close_range(first: libc::c_uint, last: libc::c_uint) -> Result<(), String> {
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
        "this kernel has no close_range(2) (Linux 5.9); the handoff cannot be swept, so the \
         program would inherit it; refusing to run"
            .to_string()
    } else {
        format!(
            "cannot close inherited descriptors above stderr with close_range(2): {e}; refusing to run"
        )
    })
}

/// The report's slot, closed by the program's `execve`.
fn cloexec(keep: Slot) -> Result<(), String> {
    // SAFETY: `F_SETFD` only sets the slot's close-on-exec flag.
    if unsafe { libc::fcntl(keep.fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(format!(
            "cannot set the report socket at fd {keep} close-on-exec: {}; refusing to run",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Close every descriptor above stderr but `keep`, which stays close-on-exec;
/// the warrant role must open nothing before this.
#[cfg(target_os = "macos")]
pub(super) fn close_inherited(keep: Slot) -> Result<(), String> {
    let mut entries = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        64
    ];
    let count = loop {
        let size = c_int::try_from(std::mem::size_of_val(entries.as_slice()))
            .map_err(|e| format!("cannot size the proc_pidinfo(PROC_PIDLISTFDS) buffer: {e}"))?;
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
            return Err(format!(
                "proc_pidinfo(PROC_PIDLISTFDS): {}; refusing to run",
                io::Error::last_os_error()
            ));
        }
        let listed = usize::try_from(listed).expect("a positive descriptor-list size fits usize");
        let width = std::mem::size_of::<libc::proc_fdinfo>();
        if !listed.is_multiple_of(width) {
            return Err(format!(
                "proc_pidinfo(PROC_PIDLISTFDS) returned a partial descriptor record ({listed} bytes); refusing to run"
            ));
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
                return Err(format!(
                    "close({}): {error}; refusing to run",
                    entry.proc_fd
                ));
            }
        }
    }
    cloexec(keep)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use std::io::Read;

    /// One descriptor cannot be both the warrant and the report: each is
    /// taken as an owner of its slot, so both would close it.
    #[test]
    fn a_warrant_and_a_report_cannot_share_a_slot() {
        let args = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        let err = Slots::parse(&args(&["101", "101"])).expect_err("one slot twice");
        assert!(err.contains("fd 101"), "{err}");
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
        handoff.install(&mut cmd);
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
            "/bin/sh",
            None,
            Report::new(reader, writer.into()),
        );
        let result = launch.spawn(
            Placement::Inherit,
            &crate::process::DurableRoot::default().child(),
        );
        let Err(Unstarted::Refused(why)) = result else {
            panic!("a closed stream must refuse preparation");
        };
        assert!(why.describe().contains("could not prepare"), "{why}");
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
        handoff.install(&mut cmd);
        let (reader, writer) = crate::process::cloexec_socketpair().expect("report socket");
        let witness = Witness::new(writer.try_clone().expect("clone report writer").into())
            .expect("a report is a socket");
        drop(witness.started());
        let mut launch = EnvelopedLaunch::new(
            cmd,
            stdio,
            "/bin/sh",
            None,
            Report::new(reader, writer.into()),
        );
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
            .keep(writer.as_fd())
            .expect("keep report until fork");
        drop(writer);
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "printf ENVELOPE-REFUSED >&2; exit 1"]);
        handoff.install(&mut cmd);
        let mut launch =
            EnvelopedLaunch::new(cmd, stdio, "/bin/sh", None, Report::new(reader, kept));
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
        let at = handoff.lend(lent.as_fd()).expect("lends");
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &format!("cat /dev/fd/{at}")]);
        handoff.install(&mut cmd);
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
        handoff.lend(stderr.as_fd()).expect("lends");
        let mut cmd = Command::new("/ral/no/such/program");
        handoff.install(&mut cmd);
        let spawned = crate::process::spawn(&mut cmd).map(|mut child| child.wait());
        drop(held);
        let err = spawned.expect_err("a program that cannot run must not spawn");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
