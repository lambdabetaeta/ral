//! The console pipe exists before the machine starts; `OpenVMM` connects to it as a client.
//! Stdout failure leaves the bounded log and tail running; failed boots retain their logs.
//! Dropping the console connects and closes, releasing a pump waiting for a machine that never
//! started.
#![allow(
    clippy::disallowed_macros,
    reason = "[silent:windows-host] a host diagnostic with no ral shell to print through"
)]

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use super::win;
use crate::error::{Act, Error, HostFile};
use crate::lifecycle::aside_spelling;
use crate::site::{Silent, site};

use windows_sys::Win32::Foundation::{
    ERROR_NO_DATA, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// `PolledPipe` refuses an unbuffered pipe.
const PIPE_BUFFER: u32 = 8 * 1024;

const RETAINED_LINES: usize = 6;

/// Lines without newlines are split at this bound so guest output cannot grow retained memory.
const LINE_LIMIT: usize = 512;

const LOG_LIMIT: u64 = 256 * 1024;

/// One day lets a failed boot be diagnosed the next morning without keeping logs indefinitely.
const LOG_LIFETIME: Duration = Duration::from_hours(24);

const LOG_SUFFIX: &str = ".log";

/// A machine's two logs: what the guest said, and what `OpenVMM` said about it.
#[derive(Clone, Copy)]
pub(super) enum Log {
    Console,
    Vmm,
}

impl Log {
    const ALL: [Self; 2] = [Self::Console, Self::Vmm];

    /// [`sweep`] recognises only these names and leaves every other file in the cache alone.
    const fn prefix(self) -> &'static str {
        match self {
            Self::Console => "synod-console-",
            Self::Vmm => "synod-openvmm-",
        }
    }

    const fn file(self) -> HostFile {
        match self {
            Self::Console => HostFile::ConsoleLog,
            Self::Vmm => HostFile::VmmLog,
        }
    }
}

/// Written where a log stops growing, so a reader who finds one ending
/// mid-boot knows the ending is synod's and not the guest's.
const TRUNCATED: &str = "[synod: this console log reached its size limit; what the guest said \
                         after this point was not kept]";

#[derive(Debug)]
pub(super) struct Console {
    id: String,
    /// An absent log is reported as lost diagnostics rather than naming a nonexistent file.
    log: Option<PathBuf>,
    tail: Arc<Mutex<Tail>>,
}

impl Console {
    /// # Errors
    /// Returns the pipe or pump refusal; a missing log or console does not refuse the machine.
    pub(super) fn create(id: &str, cache: &Path) -> Result<Self, Error> {
        let name = pipe_name(id);
        let refused = |cause: std::io::Error| Error::HostFile {
            file: HostFile::ConsolePipe,
            act: Act::Create,
            path: PathBuf::from(&name),
            cause: cause.to_string(),
        };
        let owner = win::user_sid()
            .and_then(|sid| win::Descriptor::new(&format!("D:P(A;;FA;;;{sid})")))
            .map_err(refused)?;
        let attributes = owner.attributes();
        let wide = win::wide(name.as_ref());
        // SAFETY: a live NUL-terminated name and descriptor; one blocking byte-mode instance, which
        // OpenVMM's serial port opens for both reading and writing.
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                &raw const attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(refused(std::io::Error::last_os_error()));
        }
        // SAFETY: a fresh pipe handle this function owns; ownership moves to the
        // `OwnedHandle` the pump thread takes, and is closed exactly once there.
        let server = unsafe { OwnedHandle::from_raw_handle(handle) };

        let (log, file) = match open_log(Log::Console, cache, id) {
            Ok((path, file)) => (Some(path), Some(file)),
            Err(why) => {
                eprintln!(
                    "synod: the guest's console will not be kept on disk: {}",
                    why.worded(aside_spelling)
                );
                (None, None)
            }
        };
        let tail = Arc::new(Mutex::new(Tail::default()));
        // A pump that never started has dropped the server, so no pipe is advertised.
        spawn_pump(server, file, Arc::clone(&tail)).map_err(|cause| {
            discard(log.as_deref());
            refused(cause)
        })?;
        Ok(Self {
            id: id.to_owned(),
            log,
            tail,
        })
    }

    /// The spelling `OpenVMM` routes to its named-pipe client (`serial_io.rs`).
    pub(super) fn pipe(&self) -> String {
        format!("//./pipe/{}{}", Log::Console.prefix(), self.id)
    }

    pub(super) fn log(&self) -> Option<&Path> {
        self.log.as_deref()
    }

    /// Poison cannot erase the words retained before a pump panic.
    pub(super) fn tail(&self) -> Vec<String> {
        site!(Silent::ConsoleRingLock, &self.tail => lock())
            .unwrap_or_else(PoisonError::into_inner)
            .lines()
    }
}

impl Drop for Console {
    /// Connecting and closing wakes a parked pump; a missing or busy server is harmless.
    fn drop(&mut self) {
        let wide = win::wide(pipe_name(&self.id).as_ref());
        // SAFETY: the name is live and NUL-terminated; zero access connects without reading or writing.
        let client = unsafe {
            site!(
                Silent::ConsolePipeWake,
                CreateFileW(
                    wide.as_ptr(),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            )
        };
        if client != INVALID_HANDLE_VALUE && !client.is_null() {
            // SAFETY: a fresh handle owned here and dropped at once, which is
            // the whole point — the connection existing is the signal.
            drop(unsafe { OwnedHandle::from_raw_handle(client) });
        }
    }
}

/// The ring bounds retained lines; the unfinished line is separately bounded.
#[derive(Debug, Default)]
struct Tail {
    lines: VecDeque<String>,
    partial: String,
}

impl Tail {
    /// Serial carriage returns are dropped so quoting cannot overwrite the surrounding error sentence.
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

    /// Blank lines are dropped so newlines cannot evict the diagnostic from the ring.
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

fn pipe_name(id: &str) -> String {
    format!(r"\\.\pipe\{}{id}", Log::Console.prefix())
}

/// Only a clean stop discards a log; a refusal may have named it for a future reader.
pub(super) fn discard(log: Option<&Path>) {
    if let Some(path) = log {
        let _ = site!(Silent::ConsoleLogDiscard, std::fs::remove_file(path));
    }
}

/// # Errors
/// Returns the cache or log creation refusal; the caller continues without a log.
pub(super) fn open_log(log: Log, cache: &Path, id: &str) -> Result<(PathBuf, File), Error> {
    site!(Silent::ConsoleLogOpen, std::fs::create_dir_all(cache)).map_err(Error::host_file(
        HostFile::MachineCache,
        Act::Make,
        cache,
    ))?;
    sweep(cache);
    let path = cache.join(format!("{}{id}{LOG_SUFFIX}", log.prefix()));
    let file = site!(Silent::ConsoleLogOpen, File::create(&path)).map_err(Error::host_file(
        log.file(),
        Act::Create,
        &path,
    ))?;
    Ok((path, file))
}

/// Only our named files older than `LOG_LIFETIME` are swept; failure leaves them for the next boot.
fn sweep(cache: &Path) {
    let Ok(entries) = site!(Silent::ConsoleLogSweep, std::fs::read_dir(cache)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = site!(Silent::ConsoleLogSweep, &name => to_str()) else {
            continue;
        };
        let ours = Log::ALL
            .iter()
            .any(|log| name.starts_with(log.prefix()) && name.ends_with(LOG_SUFFIX));
        if !ours {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|when| SystemTime::now().duration_since(when).ok())
            .is_some_and(|age| age > LOG_LIFETIME);
        if stale {
            let _ = site!(Silent::ConsoleLogSweep, std::fs::remove_file(entry.path()));
        }
    }
}

/// The unjoined pump ends on machine EOF, or on the console's drop when the machine never started.
fn spawn_pump(
    server: OwnedHandle,
    log: Option<File>,
    tail: Arc<Mutex<Tail>>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("synod-guest-console".to_string())
        .spawn(move || {
            use std::os::windows::io::AsRawHandle;
            let handle: HANDLE = server.as_raw_handle();
            // SAFETY: this thread owns the live server; null overlapped selects blocking connection.
            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            // PIPE_CONNECTED and NO_DATA both permit draining: even a client that already closed may have left words.
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
            // A failed stdout drops only that sink; the log and ring must keep receiving.
            let mut out = Some(std::io::stdout());
            let mut sink = log.map(|file| (file, 0u64));
            loop {
                let mut read = 0u32;
                // SAFETY: buffer and read are writable; null overlapped selects blocking reads.
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

                // Both output sinks receive the original bytes; only the quoted ring decodes them.
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
                // Only quotations decode lossily: a chunk may split a character, and dropping the line loses more.
                site!(Silent::ConsoleRingLock, &tail => lock())
                    .unwrap_or_else(PoisonError::into_inner)
                    .absorb(&String::from_utf8_lossy(chunk));
            }
        })
        .map(drop)
}

/// Keep the log head and mark truncation explicitly; the ring retains the tail.
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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn each_machine_gets_its_own_pipe_and_its_own_log() {
        let dir = tempfile::tempdir().expect("a cache");
        let a = Console::create("0000-a", dir.path()).expect("a pipe is created");
        let b = Console::create("0000-b", dir.path()).expect("a second pipe is created");
        assert!(a.pipe().starts_with("//./pipe/synod-console-"));
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
    }

    #[test]
    fn a_name_already_in_use_is_refused() {
        let dir = tempfile::tempdir().expect("a cache");
        let held = Console::create("0000-clash", dir.path()).expect("first");
        let err = Console::create("0000-clash", dir.path()).expect_err("the name is taken");
        assert!(
            matches!(
                err,
                Error::HostFile {
                    file: HostFile::ConsolePipe,
                    act: Act::Create,
                    ..
                }
            ),
            "{err:?}"
        );
        drop(held);
    }

    #[test]
    fn dropping_a_console_without_a_pipe_is_harmless() {
        let console = Console {
            id: "never-existed".to_string(),
            log: None,
            tail: Arc::new(Mutex::new(Tail::default())),
        };
        discard(console.log());
        assert!(
            console.tail().is_empty(),
            "a console nobody wrote to is mute"
        );
        drop(console);
    }

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

    #[test]
    fn the_guests_words_reach_both_the_log_and_the_ring() {
        let dir = tempfile::tempdir().expect("a cache");
        let console = Console::create("0000-tee", dir.path()).expect("a pipe and a log");
        let log = console.log().expect("a log was opened").to_path_buf();

        let mut machine = speak(
            &console,
            b"EXT4-fs (sda): mounted filesystem\r\nral-guest: no ral.port\r\n",
        );
        assert_eq!(
            heard(&console, 2),
            vec![
                "EXT4-fs (sda): mounted filesystem".to_string(),
                "ral-guest: no ral.port".to_string(),
            ]
        );
        let kept = std::fs::read_to_string(&log).expect("the log is readable");
        assert!(kept.contains("ral-guest: no ral.port"), "{kept}");
        machine.flush().expect("the machine's end is still open");
        drop(machine);

        discard(console.log());
        assert!(
            !log.exists(),
            "a session that ended cleanly leaves no console log behind"
        );
    }

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

    fn speak(console: &Console, words: &[u8]) -> File {
        let mut machine = File::options()
            .write(true)
            .open(console.pipe())
            .expect("the machine's end of the console");
        machine.write_all(words).expect("the guest speaks");
        machine.flush().expect("and is heard");
        machine
    }

    fn heard(console: &Console, lines: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while console.tail().len() < lines && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        console.tail()
    }
}
