//! The report socket: what a confined launch's trampoline says of its program.
//!
//! A record is a tag byte and its payload, each in one `write`.  The parent
//! reads the first to learn whether the program started and, on Linux, where
//! the trampoline supervises, the last for how it ended.  A socket, not a
//! pipe: no same-uid process can reopen it through `/proc/<pid>/fd` to forge
//! a record.

use super::launch::{Unstarted, refused};
use crate::data::json::{from_datum_slice, to_datum_vec};
use crate::error::Error;
use crate::fact::{LaunchFailure, LauncherFailure, LauncherFailureReportEncodingTooLarge};
use crate::process::Scope;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

/// What the trampoline says, each at most once, in this order.
#[derive(Debug, PartialEq, Eq)]
enum Record {
    /// Nothing ran: the warrant could not be received or its confinement entered.
    Refused(LaunchFailure),
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
                let mut bytes = to_datum_vec(why);
                if bytes.len() > usize::from(u16::MAX) {
                    bytes = to_datum_vec(&LaunchFailure::Launcher(
                        LauncherFailure::ReportEncodingTooLarge(
                            LauncherFailureReportEncodingTooLarge {
                                actual: bytes.len().into(),
                                limit: usize::from(u16::MAX).into(),
                            },
                        ),
                    ));
                }
                let len =
                    u16::try_from(bytes.len()).expect("the overflow refusal fits the envelope");
                [&[REFUSED][..], &len.to_ne_bytes(), &bytes].concat()
            }
            Self::Unstarted(errno) => [&[UNSTARTED][..], &errno.to_ne_bytes()].concat(),
            Self::Started => vec![STARTED],
            Self::Ended(status) => [&[ENDED][..], &status.into_raw().to_ne_bytes()].concat(),
        }
    }

    /// The next record; `None` at EOF before one.
    fn read(from: &mut impl Read) -> Result<Option<Self>, LauncherFailure> {
        fn read<const N: usize>(from: &mut impl Read) -> Result<[u8; N], LauncherFailure> {
            let mut bytes = [0; N];
            from.read_exact(&mut bytes)
                .map_err(|error| report_read(&error))?;
            Ok(bytes)
        }
        fn int(from: &mut impl Read) -> Result<i32, LauncherFailure> {
            read(from).map(i32::from_ne_bytes)
        }
        let mut tag = [0];
        match from.read_exact(&mut tag) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            result => result.map_err(|error| report_read(&error))?,
        }
        Ok(Some(match tag[0] {
            REFUSED => {
                let len = u16::from_ne_bytes(read(from)?);
                let mut why = vec![0; usize::from(len)];
                from.read_exact(&mut why)
                    .map_err(|error| report_read(&error))?;
                Self::Refused(from_datum_slice(&why).map_err(LauncherFailure::ReportDecode)?)
            }
            UNSTARTED => Self::Unstarted(int(from)?),
            STARTED => Self::Started,
            ENDED => Self::Ended(ExitStatus::from_raw(int(from)?)),
            other => return Err(LauncherFailure::ReportUnknownTag(other)),
        }))
    }
}

fn report_read(error: &io::Error) -> LauncherFailure {
    LauncherFailure::ReportRead(Box::new(crate::io::failure(error)))
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
    /// A stop on `scope` ends the wait: a Linux program can stop its
    /// supervisor before it says `started`.
    pub(crate) fn first(
        &mut self,
        scope: &Scope,
        vanished: impl FnOnce() -> Error,
    ) -> Result<(), Unstarted> {
        use rustix::event::PollFlags;
        let refuse = |why| Err(Unstarted::Refused(refused(why)));
        // A readable report holds its first record whole, or soon: only
        // `started`, one byte, is written once a program runs.
        let heard = loop {
            scope.poll().map_err(Unstarted::Stopped)?;
            match crate::io::poll_slice(&self.0, PollFlags::IN) {
                Ok(None) => {}
                ready => {
                    break ready
                        .map_err(|error| report_read(&error))
                        .and_then(|_| Record::read(&mut self.0));
                }
            }
        };
        match heard {
            Ok(Some(Record::Started)) => Ok(()),
            Ok(None) if cfg!(target_os = "macos") => Ok(()),
            Ok(None) => Err(Unstarted::Refused(vanished())),
            Ok(Some(Record::Unstarted(errno))) => {
                Err(Unstarted::Program(io::Error::from_raw_os_error(errno)))
            }
            Ok(Some(Record::Refused(why))) => refuse(why),
            Ok(Some(Record::Ended(_))) => refuse(LaunchFailure::Launcher(
                LauncherFailure::ReportEndedBeforeStarted,
            )),
            Err(e) => refuse(LaunchFailure::Launcher(e)),
        }
    }

    /// The program's end as its supervisor told it, else `envelope`, bwrap's
    /// own: a `128 + n` there with nothing told is the supervisor's death by
    /// `n`, and nothing else exits so silently.  Never waits, being read on
    /// the reaper's thread: a writer still open leaves `envelope` as it is.
    #[cfg(target_os = "linux")]
    pub(crate) fn amend(mut self, envelope: super::WaitOutcome) -> super::WaitOutcome {
        use super::{Signal, WaitOutcome};
        if self.0.set_nonblocking(true).is_err() {
            return envelope;
        }
        match Record::read(&mut self.0) {
            Ok(Some(Record::Ended(status))) => WaitOutcome::from_exit_status(status),
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

/// Whether `fd` is a socket: what a report is, and a macOS parcel.
pub(crate) fn is_socket(fd: BorrowedFd<'_>) -> io::Result<bool> {
    use rustix::fs::{FileType, fstat};
    Ok(FileType::from_raw_mode(fstat(fd)?.st_mode) == FileType::Socket)
}

/// The trampoline's end of the report socket, before its program runs.  A
/// write that fails finds the parent gone, and so nobody to tell.
pub(crate) struct Witness(UnixStream);

impl Witness {
    /// `fd`, if it is a socket: no other file is ral's report.
    pub(crate) fn new(fd: OwnedFd) -> Option<Self> {
        is_socket(fd.as_fd())
            .unwrap_or(false)
            .then(|| Self(fd.into()))
    }

    fn say(&mut self, record: &Record) {
        let _ = self.0.write_all(&record.bytes());
    }

    pub(crate) fn refused(mut self, why: &LaunchFailure) {
        self.say(&Record::Refused(why.clone()));
    }

    /// An `e` the OS did not raise is ral's own failure to build the exec.
    pub(crate) fn unstarted(mut self, e: &io::Error, subject: crate::fact::LaunchImageSubject) {
        self.say(&e.raw_os_error().map_or_else(
            || {
                Record::Refused(LaunchFailure::Launcher(
                    LauncherFailure::NonNativeExecError(
                        crate::fact::LauncherFailureNonNativeExecError {
                            subject,
                            cause: Box::new(crate::io::failure(e)),
                        },
                    ),
                ))
            },
            Record::Unstarted,
        ));
    }

    /// The program runs: all that is left to tell is how it ends.
    #[cfg(target_os = "linux")]
    pub(crate) fn started(mut self) -> Running {
        self.say(&Record::Started);
        Running(self)
    }
}

/// Linux: the report once its program runs.
#[cfg(target_os = "linux")]
pub(crate) struct Running(Witness);

#[cfg(target_os = "linux")]
impl Running {
    pub(crate) fn ended(mut self, status: ExitStatus) {
        self.0.say(&Record::Ended(status));
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
            Record::Refused(LaunchFailure::Warrant(crate::fact::WarrantFailure::Magic)),
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
        let scope = DurableRoot::default().child();
        let stopper = scope.clone();
        let stop = std::thread::spawn(move || {
            std::thread::sleep(3 * POLL_SLICE);
            stopper.cancel(CancelCause::Interrupted);
        });
        let heard = Testimony(reader).first(&scope, || {
            panic!("a report whose writer is open has not vanished")
        });
        stop.join().expect("the stopper");
        assert!(
            matches!(&heard, Err(Unstarted::Stopped(stop)) if stop.cause == CancelCause::Interrupted),
            "{heard:?}"
        );
    }

    /// The end is read without waiting: what the supervisor told, else a
    /// silent supervisor's own signal death, else, with a writer still open,
    /// bwrap's status as it is.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_end_is_amended_by_what_was_told_and_never_waited_for() {
        use crate::process::{Signal, WaitOutcome};
        let told = |records: &[Record]| {
            let (reader, mut writer) = crate::process::cloexec_socketpair().expect("socketpair");
            for record in records {
                writer
                    .write_all(&record.bytes())
                    .expect("a record is written");
            }
            (Testimony(reader), writer)
        };
        let bwrap = WaitOutcome::Exited(128 + libc::SIGKILL);
        let (ended, writer) = told(&[Record::Ended(ExitStatus::from_raw(3 << 8))]);
        drop(writer);
        assert_eq!(ended.amend(bwrap), WaitOutcome::Exited(3));
        let (silent, writer) = told(&[]);
        drop(writer);
        assert_eq!(
            silent.amend(bwrap),
            WaitOutcome::Signaled(Signal::new(libc::SIGKILL))
        );
        let (open, _writer) = told(&[]);
        assert_eq!(open.amend(bwrap), bwrap);
    }

    #[test]
    fn an_overlong_refusal_is_replaced_whole() {
        let failure = LaunchFailure::BatchImage("é".repeat(usize::from(u16::MAX)));
        let actual = to_datum_vec(&failure).len();
        assert_eq!(
            round_trip(&Record::Refused(failure)),
            Some(Record::Refused(LaunchFailure::Launcher(
                LauncherFailure::ReportEncodingTooLarge(LauncherFailureReportEncodingTooLarge {
                    actual: actual.into(),
                    limit: usize::from(u16::MAX).into(),
                })
            )))
        );
    }

    #[test]
    fn a_near_limit_whole_refusal_crosses_a_socket_with_concurrent_reading() {
        let mut failure = LaunchFailure::BatchImage(String::new());
        let overhead = to_datum_vec(&failure).len();
        let length = usize::from(u16::MAX) - overhead;
        failure = LaunchFailure::BatchImage("x".repeat(length));
        assert_eq!(to_datum_vec(&failure).len(), usize::from(u16::MAX));
        let (mut reader, writer) = crate::process::cloexec_socketpair().unwrap();
        let expected = failure.clone();
        let sender = std::thread::spawn(move || {
            Witness::new(writer.into()).unwrap().refused(&failure);
        });
        assert_eq!(
            Record::read(&mut reader).unwrap(),
            Some(Record::Refused(expected))
        );
        sender.join().unwrap();
        assert_eq!(Record::read(&mut reader).unwrap(), None);
    }

    #[test]
    fn malformed_refusals_keep_decode_evidence() {
        let bytes = [REFUSED, 1, 0, b'{'];
        assert!(matches!(
            Record::read(&mut bytes.as_slice()),
            Err(LauncherFailure::ReportDecode(_))
        ));
        assert_eq!(
            Record::read(&mut [9].as_slice()),
            Err(LauncherFailure::ReportUnknownTag(9))
        );
    }
}
