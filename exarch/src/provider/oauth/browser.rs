//! Browser login: the authorize page opens in the user's browser and redirects
//! to a loopback listener here, whose captured code is exchanged for tokens.

use super::{
    AGENT_NAME, AUTHORIZE_URL, DYNAMIC_CLIENT, Granted, LoginPhase, RESOURCE, SCOPE, SignIn,
};
use std::io::BufRead;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Bounds an abandoned sign-in's wait.
const MAX_WAIT: Duration = Duration::from_mins(15);

/// Drive the browser flow to completion and return the granted tokens.
pub(super) async fn run(
    client: &reqwest::Client,
    host_id: &str,
    sign_in: &SignIn,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<Granted, String> {
    let (verifier, challenge) = super::pkce();
    let state = super::random_b64url(32);
    let nonce = super::random_b64url(32);

    let listener = bind_listener()?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("could not read listener address: {e}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");

    let url = authorize_url(&redirect_uri, &challenge, &state, &nonce, host_id, sign_in)?;
    // Best effort, and its outcome is not worth reporting: a launcher that
    // reports success has still shown the user nothing when it started a text
    // browser on a box they reached over ssh. The phase carries the URL.
    let _ = launch_browser(&url);
    on_phase(LoginPhase::AwaitingBrowser { url });

    // Cloned because `spawn_blocking` needs `'static`; the caller keeps the flag it trips.
    let expected_state = state.clone();
    let cancel = Arc::clone(cancel);
    let (code, issued) =
        tokio::task::spawn_blocking(move || accept_callback(&listener, &expected_state, &cancel))
            .await
            .map_err(|e| format!("callback listener panicked: {e}"))??;

    let client_id = match (sign_in, issued) {
        (SignIn::Register, Some(issued)) => issued,
        (SignIn::Register, None) => {
            return Err("the sign-in callback carried no issued client id; OpenAI's registration did not complete".to_string());
        }
        (SignIn::Reauthorize(token), Some(issued)) if issued != token.client_id => {
            return Err("the sign-in callback names a different client than this account's; nothing was changed".to_string());
        }
        (SignIn::Reauthorize(token), _) => token.client_id.clone(),
    };

    on_phase(LoginPhase::ExchangingCode);
    let raw = super::exchange_code(client, &client_id, &redirect_uri, &code, &verifier).await?;
    Ok(Granted {
        raw,
        client_id,
        nonce,
    })
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:callback-listener] binds the loopback port the authorize page redirects back to: 1455 by convention, else any free one; only the port may vary. Sign-in machinery the user started, on this machine only, and no turn is running to card it: the browser handoff beside it is silent on the same ground."
)]
fn bind_listener() -> Result<TcpListener, String> {
    TcpListener::bind("127.0.0.1:1455")
        .or_else(|_| TcpListener::bind("127.0.0.1:0"))
        .map_err(|e| format!("could not bind a callback listener on 127.0.0.1: {e}"))
}

fn authorize_url(
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    nonce: &str,
    host_id: &str,
    sign_in: &SignIn,
) -> Result<String, String> {
    let client_id = match sign_in {
        SignIn::Register => DYNAMIC_CLIENT,
        SignIn::Reauthorize(token) => &token.client_id,
    };
    let mut url = reqwest::Url::parse(AUTHORIZE_URL)
        .map_err(|e| format!("could not build authorize URL: {e}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.extend_pairs([
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("scope", SCOPE),
            ("resource", RESOURCE),
            ("state", state),
            ("nonce", nonce),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("ext_agent_host_id", host_id),
        ]);
        match sign_in {
            SignIn::Register => {
                pairs.append_pair("agent_name_hint", AGENT_NAME);
            }
            SignIn::Reauthorize(token) => {
                pairs.append_pair("id_token_hint", &token.id_token);
                if let Some(email) = &token.email {
                    pairs.append_pair("login_hint", email);
                }
            }
        }
    }
    Ok(url.into())
}

#[cfg(target_os = "macos")]
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:browser-launch] opens the OAuth authorize URL in the platform browser; not turn-time data I/O"
)]
fn launch_browser(url: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new("open");
    cmd.arg(url);
    ral_core::process::spawn(&mut cmd)
        .map(|_| ())
        .map_err(|e| format!("could not open browser with `open`: {e}"))
}

#[cfg(target_os = "linux")]
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:browser-launch-linux] opens the OAuth authorize URL via xdg-open; not turn-time data I/O"
)]
fn launch_browser(url: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(url);
    ral_core::process::spawn(&mut cmd)
        .map(|_| ())
        .map_err(|e| format!("could not open browser with `xdg-open`: {e}"))
}

#[cfg(target_os = "windows")]
fn launch_browser(url: &str) -> Result<(), String> {
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
fn launch_browser(_url: &str) -> Result<(), String> {
    Err("opening a browser is not supported on this platform".to_string())
}

fn accept_callback(
    listener: &TcpListener,
    expected_state: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<(String, Option<String>), String> {
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
    Ok((code, client_id))
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
