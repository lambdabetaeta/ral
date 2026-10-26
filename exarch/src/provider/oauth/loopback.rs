//! Shared loopback callback listener and platform browser launchers.

use crate::site::{Silent, site};
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) const CALLBACK_PATH: &str = "/auth/callback";

#[derive(Clone, Copy)]
struct Policy {
    flow: Duration,
    request: Duration,
    response: Duration,
    poll: Duration,
    connections: usize,
    line_bytes: usize,
}

const POLICY: Policy = Policy {
    flow: Duration::from_mins(15),
    request: Duration::from_secs(2),
    response: Duration::from_millis(100),
    poll: Duration::from_millis(50),
    connections: 8,
    line_bytes: 16 * 1024,
};

/// Try the registered callback addresses in order.
pub(super) fn bind(addrs: &[&str]) -> Result<TcpListener, String> {
    let mut last = None;
    for addr in addrs {
        match site!(Silent::CallbackListener, TcpListener::bind(addr)) {
            Ok(listener) => return Ok(listener),
            Err(error) => last = Some(error),
        }
    }
    Err(match last {
        Some(error) => format!("could not bind a callback listener: {error}"),
        None => "no callback listener addresses were supplied".to_string(),
    })
}

#[cfg(target_os = "macos")]
pub(super) fn launch_browser(url: &str) -> Result<(), String> {
    let mut cmd = site!(Silent::BrowserLaunch, std::process::Command::new("open"));
    cmd.arg(url);
    ral_core::process::spawn(&mut cmd)
        .map(|_| ())
        .map_err(|e| format!("could not open browser with `open`: {e}"))
}

#[cfg(target_os = "linux")]
pub(super) fn launch_browser(url: &str) -> Result<(), String> {
    let mut cmd = site!(
        Silent::BrowserLaunchLinux,
        std::process::Command::new("xdg-open")
    );
    cmd.arg(url);
    ral_core::process::spawn(&mut cmd)
        .map(|_| ())
        .map_err(|e| format!("could not open browser with `xdg-open`: {e}"))
}

#[cfg(target_os = "windows")]
pub(super) fn launch_browser(url: &str) -> Result<(), String> {
    use std::ptr;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let file = windows_shell_target(url);
    // Not `cmd /C start`: cmd reads the query's '&' as a separator and truncates the URL.
    let status = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            WINDOWS_OPEN.as_ptr(),
            file.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;

    if status <= 32 {
        return Err(format!(
            "could not open browser through ShellExecuteW ({status})"
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
const WINDOWS_OPEN: [u16; 5] = [b'o' as u16, b'p' as u16, b'e' as u16, b'n' as u16, 0];

#[cfg(target_os = "windows")]
fn windows_shell_target(url: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    std::ffi::OsStr::new(url).encode_wide().chain([0]).collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub(super) fn launch_browser(_url: &str) -> Result<(), String> {
    Err("opening a browser is not supported on this platform".to_string())
}

pub(super) struct Callback {
    pub code: String,
    pub client_id: Option<String>,
}

pub(super) fn accept_callback(
    listener: &TcpListener,
    expected_state: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<Callback, String> {
    receive(listener, expected_state, cancel, POLICY)
}

fn receive(
    listener: &TcpListener,
    expected_state: &str,
    cancel: &AtomicBool,
    policy: Policy,
) -> Result<Callback, String> {
    let deadline = Instant::now() + policy.flow;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("could not configure callback listener: {e}"))?;
    let mut pending = Vec::new();
    let mut settled = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("sign-in cancelled".into());
        }
        if Instant::now() >= deadline {
            return settled
                .unwrap_or_else(|| Err("browser sign-in timed out; please try again".into()));
        }
        if settled.is_none() {
            for _ in 0..policy.connections {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if pending.len() < policy.connections
                            && stream.set_nonblocking(true).is_ok()
                        {
                            pending.push(Connection {
                                stream,
                                deadline: (Instant::now() + policy.request).min(deadline),
                                phase: Phase::Reading(Vec::new()),
                            });
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => return Err(format!("could not accept callback connection: {e}")),
                }
            }
        }
        for mut connection in std::mem::take(&mut pending) {
            match connection.advance(Instant::now(), policy.line_bytes) {
                Progress::Closed => continue,
                Progress::Pending => {}
                Progress::Line(line) => {
                    let (status, result) = parse_request(&line, expected_state);
                    connection.respond(status, policy.response, deadline);
                    if let Some(result) = result {
                        // Once settled, only the acknowledgement may delay returning.
                        settled = Some(result);
                        pending.clear();
                        pending.push(connection);
                        break;
                    }
                }
            }
            pending.push(connection);
        }
        if pending.is_empty()
            && let Some(result) = settled.take()
        {
            return result;
        }
        let next = pending.iter().map(|c| c.deadline).min().unwrap_or(deadline);
        std::thread::sleep(
            policy
                .poll
                .min(next.min(deadline).saturating_duration_since(Instant::now())),
        );
    }
}

struct Connection<S> {
    stream: S,
    deadline: Instant,
    phase: Phase,
}

enum Phase {
    Reading(Vec<u8>),
    Writing { response: Vec<u8>, sent: usize },
}

enum Progress {
    Pending,
    Closed,
    Line(Vec<u8>),
}

impl<S: Read + Write> Connection<S> {
    fn advance(&mut self, now: Instant, limit: usize) -> Progress {
        if now >= self.deadline {
            return Progress::Closed;
        }
        let error = match &mut self.phase {
            Phase::Reading(line) => {
                let mut bytes = [0; 1024];
                let available = bytes.len().min(limit - line.len());
                match self.stream.read(&mut bytes[..available]) {
                    Ok(0) => return Progress::Closed,
                    Ok(n) => {
                        line.extend_from_slice(&bytes[..n]);
                        if let Some(end) = line.iter().position(|&b| b == b'\n') {
                            line.truncate(end + 1);
                            return Progress::Line(std::mem::take(line));
                        }
                        if line.len() == limit {
                            return Progress::Closed;
                        }
                        return Progress::Pending;
                    }
                    Err(e) => e,
                }
            }
            Phase::Writing { response, sent } => match self.stream.write(&response[*sent..]) {
                Ok(0) => return Progress::Closed,
                Ok(n) => {
                    *sent += n;
                    return if *sent == response.len() {
                        Progress::Closed
                    } else {
                        Progress::Pending
                    };
                }
                Err(e) => e,
            },
        };
        if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) {
            Progress::Pending
        } else {
            Progress::Closed
        }
    }

    fn respond(&mut self, status: &'static str, timeout: Duration, flow_deadline: Instant) {
        let body = if status == "200 OK" {
            "Callback received. You can close this tab."
        } else {
            "This request was not accepted."
        };
        self.phase = Phase::Writing {
            response: format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ).into_bytes(),
            sent: 0,
        };
        self.deadline = (Instant::now() + timeout).min(flow_deadline);
    }
}

type CallbackResult = Result<Callback, String>;

fn parse_request(line: &[u8], expected_state: &str) -> (&'static str, Option<CallbackResult>) {
    let bad = ("400 Bad Request", None);
    let Ok(line) = std::str::from_utf8(line) else {
        return bad;
    };
    let Some(line) = line.strip_suffix("\r\n") else {
        return bad;
    };
    let parts: Vec<_> = line.split(' ').collect();
    let [method, target, version] = parts.as_slice() else {
        return bad;
    };
    if !matches!(*version, "HTTP/1.0" | "HTTP/1.1")
        || !target.starts_with('/')
        || target.starts_with("//")
        || target
            .bytes()
            .any(|b| b.is_ascii_control() || matches!(b, b'\\' | b'#'))
    {
        return bad;
    }
    if *method != "GET" || target.split('?').next() != Some(CALLBACK_PATH) {
        return ("404 Not Found", None);
    }
    let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else {
        return bad;
    };
    let mut fields = std::collections::BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if matches!(
            key.as_ref(),
            "state" | "code" | "error" | "client_id" | "error_description"
        ) && fields
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return bad;
        }
    }
    if fields.get("state").map(String::as_str) != Some(expected_state) {
        return bad;
    }
    let result = match (fields.remove("code"), fields.remove("error")) {
        (Some(code), None) if !code.is_empty() => Ok(Callback {
            code,
            client_id: fields.remove("client_id"),
        }),
        (None, Some(error)) if !error.is_empty() => {
            let detail = fields.remove("error_description").unwrap_or_default();
            let safe: String = format!("{error}: {detail}")
                .chars()
                .filter(|c| !c.is_control())
                .take(256)
                .collect();
            Err(format!("sign-in failed: {safe}"))
        }
        _ => Err("callback must carry an authorization code or an issuer error, not both".into()),
    };
    ("200 OK", Some(result))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
