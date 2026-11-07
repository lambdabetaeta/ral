//! The report socket: what a confined launch's trampoline says of its program.
//!
//! A record is a tag byte and its payload, each in one `write`.  The parent
//! reads the first to learn whether the program started and, on Linux, where
//! the trampoline supervises, the last for how it ended.  A socket, not a
//! pipe: no same-uid process can reopen it through `/proc/<pid>/fd` to forge
//! a record.

use super::launch::Unstarted;
use crate::authority::sandbox::Refusal;
use crate::process::Scope;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

/// What the trampoline says, each at most once, in this order.
#[derive(Debug, PartialEq, Eq)]
enum Record {
    /// Nothing ran: the warrant could not be received or its confinement entered.
    Refused(String),
    /// The confinement was entered; the program's spawn or `execve` failed.
    Unstarted(i32),
    /// Linux: the program runs.
    Started,
    /// Linux: the program ended so.
    Ended(ExitStatus),
}

const REFUSED: u8 = 1;
const UNSTARTED: u8 = 2;
const STARTED: u8 = 3;
const ENDED: u8 = 4;

impl Record {
    fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Refused(why) => {
                let why = &why.as_bytes()[..why.len().min(usize::from(u16::MAX))];
                let len = u16::try_from(why.len()).expect("cut to a u16 above");
                [&[REFUSED][..], &len.to_ne_bytes(), why].concat()
            }
            Self::Unstarted(errno) => [&[UNSTARTED][..], &errno.to_ne_bytes()].concat(),
            Self::Started => vec![STARTED],
            Self::Ended(status) => [&[ENDED][..], &status.into_raw().to_ne_bytes()].concat(),
        }
    }

    /// The next record; `None` at EOF before one.
    fn read(from: &mut impl Read) -> io::Result<Option<Self>> {
        fn int(from: &mut impl Read) -> io::Result<i32> {
            let mut bytes = [0; 4];
            from.read_exact(&mut bytes)?;
            Ok(i32::from_ne_bytes(bytes))
        }
        let mut tag = [0];
        match from.read_exact(&mut tag) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            read => read?,
        }
        Ok(Some(match tag[0] {
            REFUSED => {
                let mut len = [0; 2];
                from.read_exact(&mut len)?;
                let mut why = vec![0; usize::from(u16::from_ne_bytes(len))];
                from.read_exact(&mut why)?;
                Self::Refused(String::from_utf8_lossy(&why).into_owned())
            }
            UNSTARTED => Self::Unstarted(int(from)?),
            STARTED => Self::Started,
            ENDED => Self::Ended(ExitStatus::from_raw(int(from)?)),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a record of unknown kind {other}"),
                ));
            }
        }))
    }
}

/// The parent's ends of the report socket: the reader, and its copy of the
/// writer, held only until the fork hands the child its own.
pub(crate) struct Report {
    reader: UnixStream,
    writer: OwnedFd,
}

impl Report {
    pub(crate) fn new(reader: UnixStream, writer: OwnedFd) -> Self {
        Self { reader, writer }
    }

    /// Our writer goes once the child holds its own: EOF then means every
    /// copy in the child's tree is closed.
    pub(crate) fn forked(self) -> Testimony {
        drop(self.writer);
        Testimony(self.reader)
    }
}

/// The report's read end, the parent's only end once the child is forked.
pub(crate) struct Testimony(UnixStream);

impl Testimony {
    /// The trampoline's first word: `Ok` once its program runs.  On macOS
    /// EOF says so, the writer being close-on-exec; on Linux it is an
    /// envelope gone before its trampoline spoke, which `vanished` words.
    /// A stop on `scope` ends the wait.
    pub(crate) fn first(
        &mut self,
        scope: Option<&Scope>,
        vanished: impl FnOnce() -> Refusal,
    ) -> Result<(), Unstarted> {
        let refused = |why: String| Err(Unstarted::Refused(Refusal::Unavailable(why)));
        match self.heard(scope)?.and_then(|()| Record::read(&mut self.0)) {
            Ok(Some(Record::Started)) => Ok(()),
            Ok(None) if cfg!(target_os = "macos") => Ok(()),
            Ok(None) => Err(Unstarted::Refused(vanished())),
            Ok(Some(Record::Unstarted(errno))) => {
                Err(Unstarted::Program(io::Error::from_raw_os_error(errno)))
            }
            Ok(Some(Record::Refused(why))) => refused(why),
            Ok(Some(Record::Ended(_))) => {
                refused("the confined launch reported an end before a start".into())
            }
            Err(e) => refused(format!("cannot read the confined launch's report: {e}")),
        }
    }

    /// Wait until the report is readable, reading `scope` between slices: a
    /// Linux program can stop its supervisor before it says `started`.  A
    /// readable report holds its first record whole, or soon: only `started`,
    /// one byte, is written once a program runs.
    fn heard(&self, scope: Option<&Scope>) -> Result<io::Result<()>, Unstarted> {
        use rustix::event::PollFlags;
        loop {
            if let Some(Err(stop)) = scope.map(Scope::poll) {
                return Err(Unstarted::Stopped(stop));
            }
            match crate::io::poll_slice(&self.0, PollFlags::IN) {
                Ok(None) => {}
                ready => return Ok(ready.map(drop)),
            }
        }
    }

    /// The program's end as its supervisor told it, else `envelope`, bwrap's
    /// own: a `128 + n` there with nothing told is the supervisor's death by
    /// `n`, and nothing else exits so silently.  Never waits, being read on
    /// the reaper's thread: a writer still open leaves `envelope` as it is.
    #[cfg(target_os = "linux")]
    pub(crate) fn amend(mut self, envelope: super::WaitOutcome) -> super::WaitOutcome {
        use super::{Signal, WaitOutcome};
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let mut fds = [PollFd::new(&self.0, PollFlags::IN)];
        if !poll(&mut fds, Some(&now)).is_ok_and(|ready| ready > 0) {
            return envelope;
        }
        match Record::read(&mut self.0) {
            Ok(Some(Record::Ended(status))) => match (status.code(), status.signal()) {
                (Some(code), _) => WaitOutcome::Exited(code),
                (None, Some(signal)) => WaitOutcome::Signaled(Signal::new(signal)),
                (None, None) => envelope,
            },
            Ok(None) => match envelope {
                WaitOutcome::Exited(code) => {
                    Signal::of_status(code).map_or(envelope, WaitOutcome::Signaled)
                }
                signaled @ WaitOutcome::Signaled(_) => signaled,
            },
            _ => envelope,
        }
    }
}

/// The trampoline's end of the report socket.  A write that fails finds the
/// parent gone, and so nobody to tell.
pub(crate) struct Witness(UnixStream);

impl Witness {
    /// `fd`, if it is a socket: no other file is ral's report.
    pub(crate) fn new(fd: OwnedFd) -> Option<Self> {
        use rustix::fs::{FileType, fstat};
        let stat = fstat(&fd).ok()?;
        (FileType::from_raw_mode(stat.st_mode) == FileType::Socket).then(|| Self(fd.into()))
    }

    fn say(&mut self, record: &Record) {
        let _ = self.0.write_all(&record.bytes());
    }

    pub(crate) fn refused(mut self, why: &str) {
        self.say(&Record::Refused(why.to_owned()));
    }

    /// An `e` the OS did not raise is ral's own failure to build the exec.
    pub(crate) fn unstarted(mut self, e: &io::Error) {
        self.say(&e.raw_os_error().map_or_else(
            || Record::Refused(format!("cannot start the program: {e}")),
            Record::Unstarted,
        ));
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn started(&mut self) {
        self.say(&Record::Started);
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn ended(mut self, status: ExitStatus) {
        self.say(&Record::Ended(status));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(record: &Record) -> Option<Record> {
        Record::read(&mut record.bytes().as_slice()).expect("a written record reads")
    }

    #[test]
    fn every_record_reads_back_as_written() {
        for record in [
            Record::Refused("cannot enter the Seatbelt sandbox: \u{fffd}".into()),
            Record::Unstarted(libc::ENOENT),
            Record::Started,
            Record::Ended(ExitStatus::from_raw(139 << 8)),
            Record::Ended(ExitStatus::from_raw(libc::SIGSEGV)),
        ] {
            assert_eq!(round_trip(&record).as_ref(), Some(&record));
        }
        assert_eq!(Record::read(&mut [].as_slice()).unwrap(), None);
    }

    #[test]
    fn a_cut_or_unknown_record_is_an_error_not_an_end() {
        let ended = Record::Ended(ExitStatus::from_raw(0)).bytes();
        for bytes in [&ended[..3], &[9][..], &[REFUSED, 4, 0][..]] {
            assert!(Record::read(&mut &bytes[..]).is_err(), "{bytes:?}");
        }
    }

    /// A program can stop its supervisor before `started`, leaving the
    /// report open and silent: the scope's stop ends the wait.
    #[test]
    fn a_stop_ends_the_wait_on_an_open_silent_report() {
        use crate::process::{CancelCause, DurableRoot, POLL_SLICE};
        let (reader, _writer) = crate::process::cloexec_socketpair().expect("socketpair");
        let scope = DurableRoot::default().worker();
        let stopper = scope.clone();
        let stop = std::thread::spawn(move || {
            std::thread::sleep(3 * POLL_SLICE);
            stopper.cancel(CancelCause::Interrupted);
        });
        let heard = Testimony(reader).first(Some(&scope), || {
            panic!("a report whose writer is open has not vanished")
        });
        stop.join().expect("the stopper");
        assert!(
            matches!(&heard, Err(Unstarted::Stopped(stop)) if stop.cause == CancelCause::Interrupted),
            "{heard:?}"
        );
    }

    #[test]
    fn an_overlong_refusal_is_cut_not_refused() {
        let long = "x".repeat(usize::from(u16::MAX) + 7);
        let Some(Record::Refused(why)) = round_trip(&Record::Refused(long)) else {
            panic!("a refusal reads back as one");
        };
        assert_eq!(why.len(), usize::from(u16::MAX));
    }
}
