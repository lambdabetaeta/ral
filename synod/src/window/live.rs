//! Live pages run in a sealed frame that holds no bridge and reaches no network.

use std::borrow::Cow;
use tauri::http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode, header};

pub(crate) const SCHEME: &str = "synod-live";
const SHELL: &str = "<!doctype html><script src=\"/js/live-frame.js\"></script>";
const DNS_PREFETCH_CONTROL: HeaderName = HeaderName::from_static("x-dns-prefetch-control");
const PRELUDE_FILES: [&str; 4] = [
    "js/live-frame.js",
    "js/live-sizer.js",
    "css/tokens.css",
    "css/live-frame.css",
];

macro_rules! policy {
    ($own:literal, $app:literal) => {
        concat!(
            "sandbox allow-scripts; default-src 'none'; ",
            "script-src 'unsafe-inline' ",
            $own,
            "; ",
            "style-src 'unsafe-inline' ",
            $own,
            "; ",
            "img-src data: blob:; font-src data: ",
            $own,
            "; ",
            "base-uri 'none'; form-action 'none'; frame-ancestors ",
            $app
        )
    };
}
#[cfg(target_os = "macos")]
const POLICY: &str = policy!("synod-live://localhost", "tauri://localhost");
#[cfg(windows)]
const POLICY: &str = policy!("http://synod-live.localhost", "http://tauri.localhost");

#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Shell,
    Asset(&'a str),
    Refused,
}

fn route(path: &str) -> Route<'_> {
    if path == "/" {
        return Route::Shell;
    }
    let Some(rel) = path.strip_prefix('/') else {
        return Route::Refused;
    };
    let plain = rel.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    });
    if plain && (rel.starts_with("vendor/") || PRELUDE_FILES.contains(&rel)) {
        Route::Asset(rel)
    } else {
        Route::Refused
    }
}

pub(crate) fn serve<R: tauri::Runtime>(
    ctx: tauri::UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
) -> Response<Cow<'static, [u8]>> {
    if request.method() != Method::GET {
        return plain(
            StatusCode::METHOD_NOT_ALLOWED,
            format!("synod-live serves GET only; not {}", request.method()),
        );
    }
    let path = request.uri().path();
    match route(path) {
        Route::Shell => sealed(
            StatusCode::OK,
            HeaderValue::from_static("text/html; charset=utf-8"),
            SHELL.as_bytes().into(),
        ),
        Route::Asset(p) => ctx
            .app_handle()
            .asset_resolver()
            .get(p.to_owned())
            // Tauri types a fallback by the requested path; only sniffing marks index.html as text/html.
            .filter(|a| !a.mime_type.starts_with("text/html"))
            .and_then(|a| {
                HeaderValue::from_str(&a.mime_type)
                    .ok()
                    .map(|content_type| sealed(StatusCode::OK, content_type, a.bytes.into()))
            })
            .unwrap_or_else(|| {
                plain(
                    StatusCode::NOT_FOUND,
                    format!(
                        "synod-live has no {path}; a page may load only the vendor/ files its prompt names"
                    ),
                )
            }),
        Route::Refused => plain(
            StatusCode::NOT_FOUND,
            format!(
                "synod-live serves a live page its shell, its prelude, and vendor/ only; not {path}"
            ),
        ),
    }
}

fn plain(status: StatusCode, body: String) -> Response<Cow<'static, [u8]>> {
    sealed(
        status,
        HeaderValue::from_static("text/plain; charset=utf-8"),
        body.into_bytes().into(),
    )
}

fn sealed(
    status: StatusCode,
    content_type: HeaderValue,
    body: Cow<'static, [u8]>,
) -> Response<Cow<'static, [u8]>> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Module scripts and CSS fonts are CORS fetches from an opaque origin.
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    // CSP does not govern `rel=dns-prefetch`.
    headers.insert(DNS_PREFETCH_CONTROL, HeaderValue::from_static("off"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_and_the_vendored_prelude_are_served() {
        assert_eq!(route("/"), Route::Shell);
        let vendored = ["vendor/chartjs/chart.umd.min.js"];
        for file in PRELUDE_FILES.into_iter().chain(vendored) {
            let path = format!("/{file}");
            assert_eq!(route(&path), Route::Asset(file), "{path}");
        }
    }

    #[test]
    fn everything_else_is_refused() {
        for path in [
            "",
            "/index.html",
            "/js/app.js",
            "/css/accounts.css",
            "/vendor",
            "/vendor/",
            "/vendor/../js/app.js",
            "/vendor/%2e%2e/js/app.js",
            "/vendor/./x.js",
            "//vendor/x.js",
        ] {
            assert_eq!(route(path), Route::Refused, "{path:?}");
        }
    }

    #[test]
    fn the_policy_grants_no_remote_source() {
        assert!(!POLICY.contains("'unsafe-eval'"));
        for directive in POLICY.split(';').map(str::trim) {
            let (name, sources) = directive.split_once(' ').unwrap_or((directive, ""));
            match name {
                "sandbox" => assert_eq!(sources, "allow-scripts"),
                "default-src" => assert_eq!(sources, "'none'"),
                _ if name.ends_with("-src") => {
                    for source in sources.split(' ') {
                        assert!(
                            ["'none'", "'unsafe-inline'", "data:", "blob:"].contains(&source)
                                || source.contains(SCHEME),
                            "{name} grants {source}"
                        );
                    }
                }
                _ => {}
            }
        }
    }
}
