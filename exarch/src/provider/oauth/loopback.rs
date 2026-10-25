//! Shared loopback callback listener and platform browser launchers.

use crate::site::{Silent, site};
use std::io::BufRead;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Bounds an abandoned sign-in's wait.
const MAX_WAIT: Duration = Duration::from_mins(15);

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
    let mut stream = accept_within(listener, MAX_WAIT, cancel)?;
    let request_line = read_request_line(&mut stream)?;

    let path_and_query = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "malformed callback request".to_string())?;
    let url = reqwest::Url::parse(&format!("http://localhost{path_and_query}"))
        .map_err(|e| format!("could not parse callback URL: {e}"))?;

    let mut code = None;
    let mut client_id = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "client_id" => client_id = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }

    // `state` guards an open loopback port: only this flow's minted value passes,
    // and it is checked first so a stray request cannot abort the sign-in.
    if state.as_deref() != Some(expected_state) {
        write_page(&mut stream, "Sign-in failed. You can close this tab.");
        return Err("state mismatch".to_string());
    }
    if let Some(error) = error {
        write_page(&mut stream, "Sign-in failed. You can close this tab.");
        return Err(match description {
            Some(d) => format!("sign-in failed: {error}: {d}"),
            None => format!("sign-in failed: {error}"),
        });
    }
    let code = code.ok_or_else(|| "callback did not carry an authorization code".to_string())?;

    write_page(&mut stream, "Signed in to exarch. You can close this tab.");
    Ok(Callback { code, client_id })
}

/// Accept one connection, giving up on `timeout` or `cancel`; a blocking
/// `accept` could honour neither, hence the poll. The stream handed back
/// blocks again, for the read and write that follow.
fn accept_within(
    listener: &TcpListener,
    timeout: Duration,
    cancel: &Arc<AtomicBool>,
) -> Result<TcpStream, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("could not configure callback listener: {e}"))?;
    let start = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("sign-in cancelled".to_string());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .map_err(|e| format!("could not configure callback connection: {e}"))?;
                return Ok(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() >= timeout {
                    return Err(format!(
                        "browser sign-in timed out after {} minutes",
                        timeout.as_secs() / 60
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("could not accept callback connection: {e}")),
        }
    }
}

/// The request line carries the callback's query; whatever the reader buffers
/// past it dies with the reader, the rest of the request being of no interest.
fn read_request_line(stream: &mut TcpStream) -> Result<String, String> {
    let mut line = String::new();
    std::io::BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("could not read callback request: {e}"))?;
    Ok(line.trim_end().to_string())
}

fn write_page(stream: &mut TcpStream, message: &str) {
    let body = format!("<html><body>{message}</body></html>");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn callback(query: &str) -> Result<Callback, String> {
        let listener = bind(&["127.0.0.1:0"]).unwrap();
        let addr = listener.local_addr().unwrap();
        let receiver = std::thread::spawn(move || {
            accept_callback(&listener, "expected", &Arc::new(AtomicBool::new(false)))
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(
            stream,
            "GET /auth/callback?{query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )
        .unwrap();
        receiver.join().unwrap()
    }

    #[test]
    fn callback_retains_code_and_optional_issued_client() {
        let received = callback("state=expected&code=auth-code&client_id=oaiapp_issued").unwrap();
        assert_eq!(received.code, "auth-code");
        assert_eq!(received.client_id.as_deref(), Some("oaiapp_issued"));
        assert!(
            callback("state=expected&code=auth-code")
                .unwrap()
                .client_id
                .is_none()
        );
    }

    #[test]
    fn callback_checks_state_before_issuer_errors() {
        assert_eq!(
            callback("state=wrong&error=denied").err().unwrap(),
            "state mismatch"
        );
        assert_eq!(
            callback("state=expected&error=denied&error_description=No+thanks")
                .err()
                .unwrap(),
            "sign-in failed: denied: No thanks"
        );
        assert!(
            callback("state=expected")
                .err()
                .unwrap()
                .contains("authorization code")
        );
    }

    #[test]
    fn binding_tries_addresses_in_order_and_cancellation_precedes_accept() {
        let listener = bind(&["not-an-address", "127.0.0.1:0"]).unwrap();
        let error = accept_callback(&listener, "state", &Arc::new(AtomicBool::new(true)))
            .err()
            .unwrap();
        assert_eq!(error, "sign-in cancelled");
        assert!(bind(&[]).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_launch_target_preserves_the_entire_url() {
        let url = "https://example.test/auth?state=a&code=b";
        let wide = windows_shell_target(url);
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(String::from_utf16(&wide[..wide.len() - 1]).unwrap(), url);
    }
}
