//! Browser login: the authorize page is opened in the user's browser and
//! redirects to a loopback HTTP listener that captures the authorization
//! code, which is then exchanged for tokens.

use super::{CLIENT_ID, ISSUER, ORIGINATOR, SCOPE};
use std::io::BufRead;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// The loopback callback wait is bounded so an abandoned sign-in cannot hang
/// the CLI forever; it mirrors the device flow's authorization-code expiry.
const MAX_WAIT: Duration = Duration::from_mins(15);

/// Drive the browser flow to completion and return the issued tokens.
pub(super) async fn run(client: &reqwest::Client) -> Result<super::RawTokens, String> {
    let (verifier, challenge) = super::pkce();
    let state = super::random_b64url(32);

    let listener = bind_listener()?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("could not read listener address: {e}"))?
        .port();
    let redirect_uri = format!("http://localhost:{port}/auth/callback");

    let url = authorize_url(&redirect_uri, &challenge, &state)?;
    open_browser(&url);

    eprintln!("Waiting for sign-in to complete...");
    let expected_state = state.clone();
    let code = tokio::task::spawn_blocking(move || accept_callback(&listener, &expected_state))
        .await
        .map_err(|e| format!("callback listener panicked: {e}"))??;

    super::exchange_code(client, &redirect_uri, &code, &verifier).await
}

/// Bind the loopback callback listener, trying the primary port first and
/// the fallback second.
fn bind_listener() -> Result<TcpListener, String> {
    TcpListener::bind("127.0.0.1:1455")
        .or_else(|_| TcpListener::bind("127.0.0.1:1457"))
        .map_err(|e| format!("could not bind callback listener on 127.0.0.1:1455 or :1457: {e}"))
}

/// Build the authorize URL with all query parameters percent-encoded.
fn authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(&format!("{ISSUER}/oauth/authorize"))
        .map_err(|e| format!("could not build authorize URL: {e}"))?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", ORIGINATOR),
    ]);
    Ok(url.into())
}

/// Open the authorize URL in the platform browser. On failure the URL is
/// printed for the user to open manually rather than treated as an error.
fn open_browser(url: &str) {
    if let Err(e) = launch_browser(url) {
        eprintln!("{e}\nOpen this URL in your browser to sign in:\n  {url}");
    }
}

#[cfg(target_os = "macos")]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:browser-launch] opens the OAuth authorize URL in the platform browser; not turn-time data I/O"
)]
fn launch_browser(url: &str) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open browser with `open`: {e}"))
}

#[cfg(target_os = "linux")]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:browser-launch-linux] opens the OAuth authorize URL via xdg-open; not turn-time data I/O"
)]
fn launch_browser(url: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open browser with `xdg-open`: {e}"))
}

#[cfg(target_os = "windows")]
fn launch_browser(url: &str) -> Result<(), String> {
    use std::ptr;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let file = windows_shell_target(url);
    // Avoid `cmd /C start`: cmd treats '&' in the OAuth query as command
    // separators, truncating the URL before OpenAI receives the parameters.
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
fn launch_browser(_url: &str) -> Result<(), String> {
    Err("opening a browser is not supported on this platform".to_string())
}

/// Accept one connection, parse the authorization code from the callback
/// request, reply with a small confirmation page, and return the code.
fn accept_callback(listener: &TcpListener, expected_state: &str) -> Result<String, String> {
    let mut stream = accept_within(listener, MAX_WAIT)?;
    let request_line = read_request_line(&mut stream)?;

    let path_and_query = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "malformed callback request".to_string())?;
    let url = reqwest::Url::parse(&format!("http://localhost{path_and_query}"))
        .map_err(|e| format!("could not parse callback URL: {e}"))?;

    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        write_page(&mut stream, "Sign-in failed. You can close this tab.");
        return Err(format!("sign-in failed: {error}"));
    }
    if state.as_deref() != Some(expected_state) {
        write_page(&mut stream, "Sign-in failed. You can close this tab.");
        return Err("state mismatch".to_string());
    }
    let code = code.ok_or_else(|| "callback did not carry an authorization code".to_string())?;

    write_page(&mut stream, "Signed in to exarch. You can close this tab.");
    Ok(code)
}

/// Accept one connection, giving up after `timeout` so an abandoned sign-in
/// cannot block the CLI forever. Polls the listener in nonblocking mode
/// rather than parking the spawn-blocking thread on `accept` with no way to
/// time out, then hands back a blocking stream for the read/write below.
fn accept_within(listener: &TcpListener, timeout: Duration) -> Result<TcpStream, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("could not configure callback listener: {e}"))?;
    let start = Instant::now();
    loop {
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

/// Read the first request line — the only line that carries the callback's
/// query. Bytes the reader buffers past it are discarded with it, which is
/// fine: the request body is not needed before the response is written.
fn read_request_line(stream: &mut TcpStream) -> Result<String, String> {
    let mut line = String::new();
    std::io::BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("could not read callback request: {e}"))?;
    Ok(line.trim_end().to_string())
}

/// Write a minimal HTTP 200 response with an HTML body.
fn write_page(stream: &mut TcpStream, message: &str) {
    let body = format!("<html><body>{message}</body></html>");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_required_parameters() {
        let url =
            authorize_url("http://localhost:1455/auth/callback", "challenge", "state").unwrap();
        let url = reqwest::Url::parse(&url).unwrap();

        for (key, expected) in [
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", "http://localhost:1455/auth/callback"),
            ("scope", SCOPE),
            ("code_challenge", "challenge"),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", "state"),
            ("originator", ORIGINATOR),
        ] {
            assert_eq!(query_value(&url, key).as_deref(), Some(expected), "{key}");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_shell_target_preserves_query_ampersands() {
        let url =
            authorize_url("http://localhost:1455/auth/callback", "challenge", "state").unwrap();
        let target = windows_shell_target(&url);
        assert_eq!(target.last().copied(), Some(0));

        let decoded = String::from_utf16(&target[..target.len() - 1]).unwrap();
        assert_eq!(decoded, url);
        assert!(decoded.contains("&client_id="));
        assert!(decoded.contains("&redirect_uri="));
    }

    fn query_value(url: &reqwest::Url, key: &str) -> Option<String> {
        url.query_pairs()
            .find_map(|(name, value)| (name == key).then(|| value.into_owned()))
    }
}
