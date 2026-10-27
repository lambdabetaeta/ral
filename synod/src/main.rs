//! The assistant edits the granted folder in place; the window accounts for changes without undo.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]
#![cfg_attr(
    not(any(target_os = "macos", windows)),
    allow(
        dead_code,
        unused_imports,
        clippy::unused_self,
        reason = "only the macOS/Windows window calls L0–L3; `just lint-cross` is the dead-code authority"
    )
)]

mod assistant;
mod folder;
mod seam;
mod site;
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod test_fixture;
#[cfg(any(target_os = "macos", windows))]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri command handlers receive deserialized arguments and injected state by value"
)]
mod window;
mod wire;

#[cfg(any(target_os = "macos", windows))]
use window::{accounts, conversation, files, highlight, live, state};

#[cfg(any(target_os = "macos", windows))]
use exarch::provider::Holdings;
#[cfg(any(target_os = "macos", windows))]
use tauri::Manager;

#[cfg(any(target_os = "macos", windows))]
fn main() {
    exarch::exit_if_re_exec_child();
    let accounts = state::Accounts::new(
        crate::assistant::accounts::prepare().map(|store| Holdings::new(store, exarch::app::SYNOD)),
    );

    #[expect(
        clippy::disallowed_methods,
        clippy::disallowed_macros,
        reason = "Tauri's codegen: its expansion exits the process, and prints, when building the context panics; `site!` wraps calls, not a macro invocation"
    )]
    let context = tauri::generate_context!();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // Only the checked open_url command may open transcript links; disable injected click handling.
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .plugin(navigation_guard())
        .register_uri_scheme_protocol(live::SCHEME, live::serve)
        .manage(state::Running::default())
        .manage(accounts)
        .manage(state::Review::default())
        .manage(state::SignIn::default())
        .invoke_handler(tauri::generate_handler![
            conversation::choose_folder,
            conversation::list_models,
            conversation::start_conversation,
            conversation::send_message,
            files::open_file,
            files::open_url,
            highlight::highlight_ral,
            accounts::list_accounts,
            accounts::save_key,
            accounts::forget_key,
            accounts::save_endpoint,
            accounts::forget_endpoint,
            accounts::sign_in,
            accounts::cancel_sign_in,
        ])
        // One close owner waits with bounded grace; repeated requests cannot bypass it.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Some(close) = window.state::<state::Running>().begin_close() {
                    let window = window.clone();
                    std::thread::spawn(move || {
                        close.wait();
                        let _ = window.destroy();
                    });
                }
            }
        })
        .run(context)
        .expect("synod's window could not start");
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() -> std::process::ExitCode {
    ral_core::errln!("synod: a window for macOS and Windows; this is neither.");
    std::process::ExitCode::FAILURE
}

/// A plugin guards every webview, including the one declared in tauri.conf.json.
#[cfg(any(target_os = "macos", windows))]
fn navigation_guard() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("navigation-guard")
        .on_navigation(|webview, url| {
            is_own_page(url)
                || is_live_frame(url)
                || is_dev_server(url, webview.config().build.dev_url.as_ref())
        })
        .build()
}

/// Accept only Tauri's embedded origins, without a port.
#[cfg(any(target_os = "macos", windows))]
fn is_own_page(url: &tauri::Url) -> bool {
    let host = url.host_str();
    url.port().is_none()
        && match url.scheme() {
            "tauri" => host == Some("localhost"),
            "http" | "https" => host == Some("tauri.localhost"),
            _ => false,
        }
}

/// `WebKit` asks the guard about subframe loads, so a live frame must pass in `WebKit`'s
/// spelling; `WebView2` asks only about top-level loads, so its spelling never does.
/// A top-level live frame is inert (`live-frame.js`).
#[cfg(any(target_os = "macos", windows))]
fn is_live_frame(url: &tauri::Url) -> bool {
    url.scheme() == live::SCHEME && url.host_str() == Some("localhost") && url.port().is_none()
}

/// The configured dev origin is trusted only in debug builds.
#[cfg(any(target_os = "macos", windows))]
fn is_dev_server(url: &tauri::Url, dev_url: Option<&tauri::Url>) -> bool {
    tauri::is_dev() && dev_url.is_some_and(|dev| dev.origin() == url.origin())
}

#[cfg(all(test, any(target_os = "macos", windows)))]
mod tests {
    use super::{is_live_frame, is_own_page};

    fn own(url: &str) -> bool {
        is_own_page(&url.parse().expect("a well-formed test URL"))
    }

    fn live(url: &str) -> bool {
        is_live_frame(&url.parse().expect("a well-formed test URL"))
    }

    #[test]
    fn the_app_origin_is_admitted_on_every_platform() {
        assert!(own("tauri://localhost/index.html"));
        assert!(own("http://tauri.localhost/"));
        assert!(own("https://tauri.localhost/index.html#top"));
    }

    #[test]
    fn every_other_origin_is_refused() {
        for url in [
            "https://example.com/",
            "http://tauri.localhost.example.com/",
            "http://tauri.localhost:8080/",
            "http://localhost/",
            "tauri://evil/",
            "file:///C:/Windows/win.ini",
            "data:text/html,<p>hi</p>",
            "javascript:alert(1)",
            "about:blank",
        ] {
            assert!(!own(url), "{url} should be refused");
        }
    }

    #[test]
    fn a_live_frame_is_admitted_in_webkits_spelling_only() {
        assert!(live("synod-live://localhost/"));
        for url in [
            "http://synod-live.localhost/",
            "https://synod-live.localhost/",
            "synod-live://evil/",
            "synod-live://localhost:8080/",
        ] {
            assert!(!live(url), "{url} should be refused");
        }
    }
}
