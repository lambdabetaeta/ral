//! The guest's console, on a named pipe.
//!
//! The daemon's one reporting channel is a console→host log, and that log
//! is the only thing that speaks during the seconds before a guest can speak
//! for itself.  Without it a boot that fails — a kernel that cannot find its
//! disks, an initramfs that refuses, a daemon that will not accept the command
//! line — is indistinguishable from a boot that is merely slow: both are a
//! timeout waiting for a control connection that never comes.  With it, the
//! reason is on synod's own output, in the guest's own words.
//!
//! The macOS backend gets this almost for free — a virtio console attached to
//! the host's `stdout`.  Hyper-V has no virtio, so the guest gets an emulated
//! serial port (`ttyS0`) whose far end is a named pipe: the compute service
//! connects to it as a *client* when the machine starts, which means the pipe's
//! server end must exist first.  Hence the order in [`Hypervisor::boot`](crate::Hypervisor::boot):
//! create the pipe, create the machine, start it, then read.
//!
//! # Why `stdout` alone was a diagnostic that reached nobody
//!
//! On an installed synod the process that owns the machine is
//! `SynodMachineBroker`, a `LocalSystem` service, and a service has no console:
//! its standard output is a handle that writes nowhere at all.  So the one line
//! that explained why a guest refused to come up was written, and discarded,
//! every time — and the boot failure went on to say the reason was "above",
//! where there was no above.  This module therefore *tees*: `stdout` for a
//! developer running `synod-machine-broker.exe --console`, a per-machine log
//! file for everyone else, and a short ring of the last lines in memory
//! ([`Tail`]) so the boot failure can quote the guest instead of pointing at a
//! place the reader has to go and look.
//!
//! A failing write to `stdout` is therefore no longer a reason to stop pumping:
//! under a service it fails on the very first chunk, and stopping there is
//! exactly how the durable copy would be lost again.
//!
//! # Three bounds, so a diagnostic does not become litter
//!
//! Nothing here may accumulate the way session disks once did, so each of the
//! three sinks is bounded by construction:
//!
//! - the **ring** keeps [`RETAINED_LINES`] lines, and no line longer than
//!   [`LINE_LIMIT`] characters;
//! - the **log** stops at [`LOG_LIMIT`] bytes, saying so where it stops.  It is
//!   the *head* that is kept, deliberately: the beginning of a boot is where a
//!   boot's reasons are, and the end is already quoted in the failure;
//! - the **directory** is swept on every boot of logs older than
//!   [`LOG_LIFETIME`] ([`sweep`]), and a log whose session ended cleanly is
//!   removed at once ([`Console::discard`]).  The only logs that survive a day
//!   are the ones a failed boot left behind and named in its own error, which
//!   is the one case where the file is still worth something.
//!
//! # Waking a pump that is waiting for a client that will never come
//!
//! A server end parked in `ConnectNamedPipe` is parked until *something*
//! connects.  If the machine fails to start there is no client, and the pump
//! thread would sit there for the life of the process.  The remedy is the
//! standard one and needs no handle sharing across threads: [`Console::wake`]
//! connects to the pipe itself and immediately closes, which is a connection as
//! far as the parked server is concerned.  The pump then reads an immediate
//! end-of-file and exits.  This is why a [`Console`] keeps only the pipe's
//! *name* after handing its server end to the pump.
//!
//! # On the filesystem calls below
//!
//! Every one of them is host-side diagnostic plumbing around a machine the
//! guest has not booted yet: making the cache directory, opening this machine's
//! log, sweeping the ones nobody kept.  There is no `Shell` to route it through
//! and no run to raise a card in — the same standing [`super::vhd`]'s disk
//! writing has, and hence the same module-scoped allow.
#![allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: host-side diagnostic plumbing before any engine exists — the \
              guest's console log and the sweep of the ones nobody kept; no shell, no run, no \
              card. See the module docs."
)]

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use windows_sys::Win32::Foundation::{
    ERROR_NO_DATA, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// The pipe's buffer, in bytes.  A console produces lines, not bulk data, so
/// this is sized to be comfortable rather than fast.
const PIPE_BUFFER: u32 = 8 * 1024;

/// `PIPE_ACCESS_DUPLEX`.  The pipe is opened both ways although synod only ever
/// reads: the emulated serial port is a two-way device, and a service that
/// opens it for read/write must find a pipe that permits both or the machine
/// fails to start with a message about the COM port rather than about this.
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;

/// How many of the guest's last lines are kept for a boot failure to quote.
///
/// A handful, because a handful is what diagnoses a boot: the panic and the two
/// or three lines that led to it.  More would turn one error sentence into a
/// dump, and the log on disk is where a reader goes for the whole story.
const RETAINED_LINES: usize = 6;

/// The longest line the ring will hold, in characters.  A guest emitting a
/// megabyte without a newline — a kernel dumping memory, a daemon in a loop —
/// must not cost the host a megabyte of retained text, so a line this long is
/// treated as ended and the rest begins another.
const LINE_LIMIT: usize = 512;

/// How much of one guest's console is kept on disk (256 KiB).
///
/// A whole boot's console is a few tens of kilobytes, so this is generous for
/// the thing it is for and still small enough that a runaway guest cannot fill
/// a disk with the log of its own noise.
const LOG_LIMIT: u64 = 256 * 1024;

/// How long a console log left behind by a boot nobody tore down cleanly may
/// sit in the cache before a later boot sweeps it (a day).
///
/// A day rather than an hour because the reader of a failed boot's log is a
/// person who may only get to it tomorrow morning; a day rather than a week
/// because by then nobody is debugging that boot.
const LOG_LIFETIME: Duration = Duration::from_hours(24);

/// How a console log is named, so [`sweep`] can recognise its own kind and
/// leave every other file in the cache — the wrapped rootfs above all — alone.
const LOG_PREFIX: &str = "synod-console-";

/// And its extension.
const LOG_SUFFIX: &str = ".log";

/// Written where a log stops growing, so a reader who finds one ending
/// mid-boot knows the ending is synod's and not the guest's.
const TRUNCATED: &str = "[synod: this console log reached its size limit; what the guest said \
                         after this point was not kept]";

/// One machine's console: the pipe it writes to, the name by which a stuck
/// reader can be woken, the log its words are kept in, and the last few of
/// them.
#[derive(Debug)]
pub(super) struct Console {
    name: String,
    /// Where the guest's words are also written.  `None` is a diagnostic
    /// already half lost, and a boot failure says so rather than naming a file
    /// that is not there.
    log: Option<PathBuf>,
    /// The last few lines, shared with the pump thread that fills it.
    tail: Arc<Mutex<Tail>>,
}

impl Console {
    /// Create the server end of one machine's console pipe, and the log its
    /// output is kept in under `cache`.
    ///
    /// Must happen before the machine is created, since the machine's document
    /// names this pipe and the service dials it at start.
    ///
    /// # Errors
    /// Returns a sentence if Windows would not create the *pipe*.  A console is
    /// a diagnostic, so [`Hypervisor::boot`](crate::Hypervisor::boot) treats
    /// this as a loss to report rather than a reason to refuse a session — and
    /// a log that cannot be opened is a smaller loss again, reported on
    /// `stderr` and carried on with.
    pub(super) fn create(id: &str, cache: &Path) -> Result<Self, String> {
        let name = format!(r"\\.\pipe\synod-console-{id}");
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is a NUL-terminated pipe name alive for the call; one
        // instance, byte mode, blocking, default timeout, and the service's own
        // default security (null), which grants the creating user.
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(format!(
                "the guest console pipe {name} could not be created: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: a fresh pipe handle this function owns; ownership moves to the
        // `OwnedHandle` the pump thread takes, and is closed exactly once there.
        let server = unsafe { OwnedHandle::from_raw_handle(handle) };

        // The log, like the pipe, before the machine that will fill it — and on
        // the same terms: a machine whose console cannot be kept still boots.
        let (log, file) = match open_log(cache, id) {
            Ok((path, file)) => (Some(path), Some(file)),
            Err(why) => {
                eprintln!("synod: the guest's console will not be kept on disk: {why}");
                (None, None)
            }
        };
        let tail = Arc::new(Mutex::new(Tail::default()));
        spawn_pump(server, file, Arc::clone(&tail));
        Ok(Self { name, log, tail })
    }

    /// The name the machine's document must carry.
    pub(super) fn pipe(&self) -> &str {
        &self.name
    }

    /// Where the guest's console is being kept, when it is being kept at all.
    pub(super) fn log(&self) -> Option<&Path> {
        self.log.as_deref()
    }

    /// The last few lines the guest said, oldest first.
    ///
    /// A pump that panicked mid-line poisons the lock, and its words are read
    /// out anyway: whatever the guest managed to say before the reader broke is
    /// exactly what a failing boot needs quoted.
    pub(super) fn tail(&self) -> Vec<String> {
        self.tail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lines()
    }

    /// Forget the log: a session that ended cleanly has nothing left to
    /// explain, and its console is litter from that moment on.
    ///
    /// Best-effort, and deliberately not called on the failing path — there the
    /// error names this file, so it has a reader still to come.
    pub(super) fn discard(&self) {
        if let Some(path) = &self.log {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Unblock a pump still waiting for the machine to connect.
    ///
    /// Called on every teardown path, including the one where the machine never
    /// started.  Harmless when the pump has already moved on: the client handle
    /// is opened and dropped, and a pipe with no server left to accept it
    /// simply fails to open.
    pub(super) fn wake(&self) {
        let wide: Vec<u16> = self.name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is a NUL-terminated pipe name alive for the call.  A
        // zero access mask asks for a connection without read or write rights,
        // which is all a wake-up needs, and the returned handle is adopted so it
        // closes immediately.
        let client = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if client != INVALID_HANDLE_VALUE && !client.is_null() {
            // SAFETY: a fresh handle owned here and dropped at once, which is
            // the whole point — the connection existing is the signal.
            drop(unsafe { OwnedHandle::from_raw_handle(client) });
        }
    }
}

/// The last few lines the guest said, and the line it is part-way through.
///
/// A ring rather than a buffer: the interesting end of a console is the end,
/// and a boot that says a great deal must cost no more memory than a boot that
/// says three words.
#[derive(Debug, Default)]
struct Tail {
    lines: VecDeque<String>,
    partial: String,
}

impl Tail {
    /// Fold one chunk of the guest's console into the ring.
    ///
    /// Carriage returns are dropped rather than kept: a serial console ends its
    /// lines `\r\n`, and a trailing `\r` in a quoted error would put the rest
    /// of the sentence back at the start of the line.
    fn absorb(&mut self, chunk: &str) {
        for character in chunk.chars() {
            match character {
                '\n' => {
                    let line = std::mem::take(&mut self.partial);
                    self.finish(line);
                }
                '\r' => {}
                _ => {
                    self.partial.push(character);
                    if self.partial.chars().count() >= LINE_LIMIT {
                        let line = std::mem::take(&mut self.partial);
                        self.finish(line);
                    }
                }
            }
        }
    }

    /// Retain one completed line, dropping the oldest if the ring is full.
    ///
    /// A blank line is not a word, and a console emits plenty of them; keeping
    /// them would let three newlines push the panic out of the quotation.
    fn finish(&mut self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        if self.lines.len() == RETAINED_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// The retained lines, oldest first, including a line the guest began and
    /// never ended — a kernel that dies mid-sentence says the most.
    fn lines(&self) -> Vec<String> {
        self.lines
            .iter()
            .cloned()
            .chain(Some(self.partial.clone()).filter(|line| !line.trim().is_empty()))
            .skip(usize::from(
                self.lines.len() == RETAINED_LINES && !self.partial.trim().is_empty(),
            ))
            .collect()
    }
}

/// Open this machine's console log, sweeping the ones earlier boots left.
///
/// # Errors
/// Returns a sentence if the cache cannot be made or the file cannot be
/// created.  Its caller carries on without a log.
fn open_log(cache: &Path, id: &str) -> Result<(PathBuf, File), String> {
    std::fs::create_dir_all(cache).map_err(|e| {
        format!(
            "the machine cache {} could not be made: {e}",
            cache.display()
        )
    })?;
    sweep(cache);
    let path = cache.join(format!("{LOG_PREFIX}{id}{LOG_SUFFIX}"));
    let file = File::create(&path).map_err(|e| {
        format!(
            "the console log {} could not be opened: {e}",
            path.display()
        )
    })?;
    Ok((path, file))
}

/// Remove the console logs of boots nobody is debugging any more.
///
/// Only files of this module's own naming, only those older than
/// [`LOG_LIFETIME`], and every failure ignored: a sweep is housekeeping done on
/// the way past, and a log that will not go is a log to try again next boot.
fn sweep(cache: &Path) {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(LOG_PREFIX) || !name.ends_with(LOG_SUFFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|when| SystemTime::now().duration_since(when).ok())
            .is_some_and(|age| age > LOG_LIFETIME);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Read the guest's console onto synod's own output, into its log, and into the
/// ring the boot failure quotes, until the machine stops.
///
/// The thread is never joined: it ends when the machine's end of the pipe
/// closes (a stopped machine), or when [`Console::wake`] gives it the
/// end-of-file that lets a never-started machine's pump retire.
fn spawn_pump(server: OwnedHandle, log: Option<File>, tail: Arc<Mutex<Tail>>) {
    std::thread::Builder::new()
        .name("synod-guest-console".to_string())
        .spawn(move || {
            use std::os::windows::io::AsRawHandle;
            let handle: HANDLE = server.as_raw_handle();
            // SAFETY: `handle` is the live server end this thread owns.  A null
            // overlapped pointer is the blocking form, which is what a
            // dedicated thread wants.
            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            // Two codes are reported as failure and are success, for opposite
            // reasons: a client that connected between `CreateNamedPipeW` and
            // here (`ERROR_PIPE_CONNECTED`), and one that connected *and closed
            // again* before this thread parked (`ERROR_NO_DATA`).  The second
            // matters as much as the first — the words still sit in the pipe's
            // buffer, and they are the words of a guest that died as soon as it
            // had spoken, which is precisely the boot this log exists for.
            let ready = connected != 0
                || std::io::Error::last_os_error()
                    .raw_os_error()
                    .is_some_and(|code| {
                        code == ERROR_PIPE_CONNECTED.cast_signed()
                            || code == ERROR_NO_DATA.cast_signed()
                    });
            if !ready {
                return;
            }

            let mut buffer = [0u8; PIPE_BUFFER as usize];
            // `None` once the output has refused a write, which under a service
            // is on the first chunk: there is no console attached, and giving
            // up on the *pump* for that reason is the whole bug this teeing
            // exists to fix.
            let mut out = Some(std::io::stdout());
            // The log, and how much of its allowance it has spent.
            let mut sink = log.map(|file| (file, 0u64));
            loop {
                let mut read = 0u32;
                // SAFETY: `buffer` is writable for the length passed, `read` is
                // a writable slot, and the null overlapped pointer keeps the
                // read blocking.
                let ok = unsafe {
                    ReadFile(
                        handle,
                        buffer.as_mut_ptr(),
                        PIPE_BUFFER,
                        &raw mut read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || read == 0 {
                    break;
                }
                let chunk = &buffer[..read as usize];

                // A console line the guest wrote is the guest's own ink, so it
                // reaches both sinks as bytes and is parsed nowhere but the
                // ring, whose only purpose is to be quoted.
                if let Some(stdout) = &mut out
                    && stdout
                        .write_all(chunk)
                        .and_then(|()| stdout.flush())
                        .is_err()
                {
                    out = None;
                }
                if !keep_logging(&mut sink, chunk) {
                    sink = None;
                }
                // Lossily, and only here: the guest's kernel writes whatever
                // encoding it was built with, a chunk may end mid-character,
                // and a replacement character in one quoted word is a far
                // smaller loss than dropping the line it was in.
                tail.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .absorb(&String::from_utf8_lossy(chunk));
            }
        })
        .ok();
}

/// Write as much of `chunk` to the log as its allowance leaves, and answer
/// whether the log is still worth holding.
///
/// The allowance is spent from the front, so what survives is the *head* of the
/// boot; where it runs out, [`TRUNCATED`] says so in the file itself rather
/// than leaving a reader to wonder whether the guest fell silent there.
fn keep_logging(sink: &mut Option<(File, u64)>, chunk: &[u8]) -> bool {
    let Some((file, written)) = sink else {
        return false;
    };
    let room = usize::try_from(LOG_LIMIT.saturating_sub(*written)).unwrap_or(usize::MAX);
    let taken = room.min(chunk.len());
    let wrote = file
        .write_all(&chunk[..taken])
        .and_then(|()| file.flush())
        .is_ok();
    *written += u64::try_from(taken).expect("a chunk of a few kilobytes");
    if taken < chunk.len() {
        let _ = writeln!(file, "{TRUNCATED}");
        return false;
    }
    wrote
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console names a pipe under the local pipe namespace, per machine, so
    /// two sessions never share one — and keeps its log beside the session
    /// disks, named after the same machine, so a reader can tell whose it is.
    #[test]
    fn each_machine_gets_its_own_pipe_and_its_own_log() {
        let dir = tempfile::tempdir().expect("a cache");
        let a = Console::create("0000-a", dir.path()).expect("a pipe is created");
        let b = Console::create("0000-b", dir.path()).expect("a second pipe is created");
        assert!(a.pipe().starts_with(r"\\.\pipe\synod-console-"));
        assert_ne!(a.pipe(), b.pipe());

        let log = a.log().expect("a log was opened");
        assert_eq!(
            log.parent(),
            Some(dir.path()),
            "a console log lives in the cache the backend owns"
        );
        assert_eq!(
            log.file_name().and_then(|name| name.to_str()),
            Some("synod-console-0000-a.log")
        );
        assert_ne!(a.log(), b.log());
        a.wake();
        b.wake();
    }

    /// The same name twice is refused rather than silently shared: a second
    /// instance of an existing pipe would have two machines writing one
    /// console.
    #[test]
    fn a_name_already_in_use_is_refused() {
        let dir = tempfile::tempdir().expect("a cache");
        let held = Console::create("0000-clash", dir.path()).expect("first");
        let err = Console::create("0000-clash", dir.path()).expect_err("the name is taken");
        assert!(err.contains("could not be created"), "{err}");
        held.wake();
    }

    /// Waking a pipe nobody is serving is a no-op, not a panic — the teardown
    /// path calls it unconditionally.  So is discarding a log that was never
    /// opened.
    #[test]
    fn waking_an_absent_pipe_is_harmless() {
        let console = Console {
            name: r"\\.\pipe\synod-console-never-existed".to_string(),
            log: None,
            tail: Arc::new(Mutex::new(Tail::default())),
        };
        console.wake();
        console.discard();
        assert!(
            console.tail().is_empty(),
            "a console nobody wrote to is mute"
        );
    }

    /// The ring answers with the guest's last lines, oldest first, the
    /// unfinished final one included, and nothing a guest can send makes it
    /// grow: this is the invariant a boot failure's quotation rests on, since a
    /// kernel that panics says the reason last — sometimes mid-sentence — and
    /// the whole boot before it.  A serial console's carriage returns and its
    /// blank lines are not words; a line without end is cut at [`LINE_LIMIT`].
    #[test]
    fn the_ring_answers_with_the_guests_last_words_and_never_grows() {
        let mut tail = Tail::default();
        tail.absorb("\n\n   \n\n");
        assert!(tail.lines().is_empty(), "blank lines are not words");

        for line in 0..100 {
            tail.absorb(&format!("line {line}\r\n"));
        }
        let kept = tail.lines();
        assert_eq!(kept.len(), RETAINED_LINES);
        assert_eq!(kept.first().map(String::as_str), Some("line 94"));
        assert_eq!(kept.last().map(String::as_str), Some("line 99"));
        assert!(
            kept.iter().all(|line| !line.contains('\r')),
            "a serial console's carriage returns must not reach a quotation: {kept:?}"
        );

        tail.absorb("and then, mid-word");
        let kept = tail.lines();
        assert_eq!(kept.len(), RETAINED_LINES, "the bound holds: {kept:?}");
        assert_eq!(kept.last().map(String::as_str), Some("and then, mid-word"));

        tail.absorb(&"x".repeat(LINE_LIMIT * 4));
        let kept = tail.lines();
        assert_eq!(
            kept.len(),
            RETAINED_LINES,
            "the bound still holds: {kept:?}"
        );
        for line in kept {
            assert!(line.chars().count() <= LINE_LIMIT, "{}", line.len());
        }
    }

    /// The guest's words reach the log on disk *and* the ring in memory — the
    /// whole point of the tee, exercised over a real pipe by standing in for
    /// the machine's end of it.  A clean teardown then takes the log with it.
    #[test]
    fn the_guests_words_reach_both_the_log_and_the_ring() {
        let dir = tempfile::tempdir().expect("a cache");
        let console = Console::create("0000-tee", dir.path()).expect("a pipe and a log");
        let log = console.log().expect("a log was opened").to_path_buf();

        let mut machine = speak(
            &console,
            b"EXT4-fs (sda): mounted filesystem\r\nral-daemon: no ral.port\r\n",
        );
        assert_eq!(
            heard(&console, 2),
            vec![
                "EXT4-fs (sda): mounted filesystem".to_string(),
                "ral-daemon: no ral.port".to_string(),
            ]
        );
        let kept = std::fs::read_to_string(&log).expect("the log is readable");
        assert!(kept.contains("ral-daemon: no ral.port"), "{kept}");
        machine.flush().expect("the machine's end is still open");
        drop(machine);

        console.discard();
        assert!(
            !log.exists(),
            "a session that ended cleanly leaves no console log behind"
        );
    }

    /// A guest that speaks and dies before the pump has even parked is quoted
    /// anyway: its words are in the pipe's buffer, and a boot that fails that
    /// fast is exactly the boot whose one line explains everything.
    #[test]
    fn a_guest_that_dies_as_soon_as_it_speaks_is_still_quoted() {
        let dir = tempfile::tempdir().expect("a cache");
        let console = Console::create("0000-brief", dir.path()).expect("a pipe and a log");
        drop(speak(&console, b"Kernel panic - not syncing: VFS\r\n"));
        assert_eq!(
            heard(&console, 1),
            vec!["Kernel panic - not syncing: VFS".to_string()]
        );
    }

    /// Stand in for the compute service: dial this console as the machine does
    /// and say something on it.  The handle is returned so a caller can choose
    /// when the guest falls silent.
    fn speak(console: &Console, words: &[u8]) -> File {
        let mut machine = File::options()
            .write(true)
            .open(console.pipe())
            .expect("the machine's end of the console");
        machine.write_all(words).expect("the guest speaks");
        machine.flush().expect("and is heard");
        machine
    }

    /// The console's ring once it holds `lines` of them.
    ///
    /// The pump is a thread, so the words arrive when they arrive; anything
    /// longer than this wait is a pump that is not running at all.
    fn heard(console: &Console, lines: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while console.tail().len() < lines && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        console.tail()
    }
}
