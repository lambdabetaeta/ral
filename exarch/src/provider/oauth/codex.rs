//! Codex CLI's browser and proprietary device authorization protocols.
use super::{Grant, LoginPhase, OAuthToken, SignIn};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Deserializer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const ORIGINATOR: &str = "codex_cli_rs";
const ISSUER: &str = "https://auth.openai.com";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub(crate) const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
// Model visibility depends on minimal_client_version: never use exarch's version here.
const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.164.0";
const MAX_WAIT: Duration = Duration::from_mins(15);

pub(crate) fn codex_client_version() -> String {
    version(ral_core::host::var("EXARCH_CODEX_CLIENT_VERSION").as_deref())
}

fn version(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_CODEX_CLIENT_VERSION)
        .to_owned()
}

pub(crate) fn models_url() -> String {
    models_url_for(&codex_client_version())
}

fn models_url_for(version: &str) -> String {
    let mut url = reqwest::Url::parse(MODELS_URL).expect("constant models URL");
    url.query_pairs_mut().append_pair("client_version", version);
    url.into()
}

pub(super) async fn browser(
    client: &reqwest::Client,
    _sign_in: &SignIn,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<OAuthToken, String> {
    let (verifier, challenge) = super::pkce();
    let state = super::random_b64url(32);
    let listener = super::loopback::bind(&["127.0.0.1:1455", "127.0.0.1:1457"])?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("could not read listener address: {e}"))?
        .port();
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let url = authorize_url(&redirect_uri, &challenge, &state)?;
    let _ = super::loopback::launch_browser(&url);
    on_phase(LoginPhase::AwaitingBrowser { url });
    let flag = Arc::clone(cancel);
    let callback = tokio::task::spawn_blocking(move || {
        super::loopback::accept_callback(&listener, &state, &flag)
    })
    .await
    .map_err(|e| format!("callback listener panicked: {e}"))??;
    on_phase(LoginPhase::ExchangingCode);
    finalize(exchange_code(client, ISSUER, &redirect_uri, &callback.code, &verifier).await?)
}

fn authorize_url(redirect: &str, challenge: &str, state: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(&format!("{ISSUER}/oauth/authorize"))
        .map_err(|e| format!("could not build authorize URL: {e}"))?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect),
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

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct RawTokens {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

fn exchange_request(
    client: &reqwest::Client,
    issuer: &str,
    redirect: &str,
    code: &str,
    verifier: &str,
) -> reqwest::RequestBuilder {
    client.post(format!("{issuer}/oauth/token")).form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ])
}

async fn exchange_code(
    client: &reqwest::Client,
    issuer: &str,
    redirect: &str,
    code: &str,
    verifier: &str,
) -> Result<RawTokens, String> {
    let response = exchange_request(client, issuer, redirect, code, verifier)
        .send()
        .await
        .map_err(|e| format!("token exchange request failed: {e}"))?;
    super::json_or_error(response, "token exchange").await
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct UserCode {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default = "default_interval", deserialize_with = "interval_secs")]
    interval: u64,
}
fn default_interval() -> u64 {
    5
}
fn interval_secs<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Interval {
        Num(u64),
        Str(String),
    }
    let value = match Interval::deserialize(d)? {
        Interval::Num(n) => n,
        Interval::Str(s) => s.trim().parse().map_err(serde::de::Error::custom)?,
    };
    Ok(if value == 0 {
        default_interval()
    } else {
        value
    })
}
#[derive(Deserialize)]
struct PollSuccess {
    authorization_code: String,
    code_verifier: String,
}

pub(super) async fn device(
    client: &reqwest::Client,
    _sign_in: &SignIn,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<OAuthToken, String> {
    let response = user_code_request(client, ISSUER)
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;
    let code: UserCode = super::json_or_error(response, "device code request").await?;
    on_phase(LoginPhase::AwaitingDevice {
        user_code: code.user_code.clone(),
        url: format!("{ISSUER}/codex/device"),
        expires_in: "15 minutes".into(),
    });
    let poll = poll_for_code(client, ISSUER, &code, cancel, MAX_WAIT).await?;
    on_phase(LoginPhase::ExchangingCode);
    finalize(
        exchange_code(
            client,
            ISSUER,
            &format!("{ISSUER}/deviceauth/callback"),
            &poll.authorization_code,
            &poll.code_verifier,
        )
        .await?,
    )
}

fn user_code_request(client: &reqwest::Client, issuer: &str) -> reqwest::RequestBuilder {
    client
        .post(format!("{issuer}/api/accounts/deviceauth/usercode"))
        .json(&serde_json::json!({"client_id": CLIENT_ID}))
}
fn poll_request(
    client: &reqwest::Client,
    issuer: &str,
    code: &UserCode,
) -> reqwest::RequestBuilder {
    client.post(format!("{issuer}/api/accounts/deviceauth/token"))
        .json(&serde_json::json!({"device_auth_id": code.device_auth_id, "user_code": code.user_code}))
}
fn remaining(
    start: Instant,
    lifetime: Duration,
    cancel: &Arc<AtomicBool>,
) -> Result<Duration, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("sign-in cancelled".into());
    }
    lifetime
        .checked_sub(start.elapsed())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "device sign-in timed out after 15 minutes".into())
}
fn pending(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 403 | 404)
}
async fn poll_for_code(
    client: &reqwest::Client,
    issuer: &str,
    code: &UserCode,
    cancel: &Arc<AtomicBool>,
    lifetime: Duration,
) -> Result<PollSuccess, String> {
    let start = Instant::now();
    loop {
        let left = remaining(start, lifetime, cancel)?;
        let response = poll_request(client, issuer, code)
            .timeout(left.min(Duration::from_secs(30)))
            .send()
            .await
            .map_err(|e| format!("device token poll failed: {e}"))?;
        if pending(response.status()) {
            let left = remaining(start, lifetime, cancel)?;
            super::wait(Duration::from_secs(code.interval).min(left), cancel).await?;
            continue;
        }
        let success = super::json_or_error(response, "device token poll").await?;
        remaining(start, lifetime, cancel)?;
        return Ok(success);
    }
}

#[derive(Deserialize)]
struct AuthClaims {
    chatgpt_account_id: Option<String>,
    chatgpt_plan_type: Option<String>,
}
#[derive(Deserialize)]
struct IdClaims {
    sub: Option<String>,
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
    organizations: Option<Vec<Organization>>,
}
#[derive(Deserialize)]
struct Organization {
    title: Option<String>,
    is_default: Option<bool>,
}
fn workspace_title(claims: &IdClaims) -> Option<String> {
    claims
        .organizations
        .as_ref()?
        .iter()
        .find(|o| o.is_default == Some(true))?
        .title
        .clone()
}
fn decode_claims(jwt: &str) -> Option<IdClaims> {
    super::jwt_payload(jwt).ok()
}
#[derive(Deserialize)]
struct ExpClaims {
    exp: Option<i64>,
}
fn jwt_exp(jwt: &str) -> Option<u64> {
    u64::try_from(super::jwt_payload::<ExpClaims>(jwt).ok()?.exp?).ok()
}
fn expiry_secs(access: &str, id: Option<&str>) -> u64 {
    jwt_exp(access)
        .or_else(|| id.and_then(jwt_exp))
        .unwrap_or_else(|| crate::app::now_secs() + 3600)
}
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|s| !s.trim().is_empty())
}
fn finalize(raw: RawTokens) -> Result<OAuthToken, String> {
    if nonempty(Some(&raw.access_token)).is_none() || nonempty(Some(&raw.refresh_token)).is_none() {
        return Err(
            "Codex sign-in returned an empty access or refresh token; sign in again".into(),
        );
    }
    let claims = decode_claims(&raw.id_token)
        .ok_or_else(|| "Codex sign-in returned an invalid ID token; sign in again".to_owned())?;
    let subject = nonempty(claims.sub.as_deref())
        .ok_or_else(|| "Codex sign-in did not return a subject; sign in again".to_owned())?;
    let auth = claims.auth.as_ref();
    let account_id =
        nonempty(auth.and_then(|a| a.chatgpt_account_id.as_deref())).ok_or_else(|| {
            "Codex sign-in did not return a backend account id; sign in again".to_owned()
        })?;
    let issued = format!(
        "{}:{}",
        URL_SAFE_NO_PAD.encode(subject),
        URL_SAFE_NO_PAD.encode(account_id)
    );
    Ok(OAuthToken {
        expires_at: expiry_secs(&raw.access_token, Some(&raw.id_token)),
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        issued,
        email: claims.email.clone(),
        grant: Grant::Codex {
            account_id: account_id.into(),
            workspace: workspace_title(&claims),
            plan: auth.and_then(|a| a.chatgpt_plan_type.clone()),
        },
    })
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
}
fn refresh_request(
    client: &reqwest::Client,
    issuer: &str,
    current: &OAuthToken,
) -> reqwest::RequestBuilder {
    client.post(format!("{issuer}/oauth/token")).json(&serde_json::json!({
        "client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": current.refresh_token,
    }))
}
pub(super) async fn refresh(
    client: &reqwest::Client,
    current: &OAuthToken,
) -> Result<OAuthToken, String> {
    let response = refresh_request(client, ISSUER, current)
        .send()
        .await
        .map_err(|e| format!("token refresh request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(super::refresh_failure(status, &body, current));
    }
    let response: RefreshResponse = super::json_or_error(response, "token refresh").await?;
    refresh_success(current, response)
}

fn refresh_success(current: &OAuthToken, response: RefreshResponse) -> Result<OAuthToken, String> {
    let access = response
        .access_token
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "token refresh did not return an access token".to_owned())?;
    if response
        .refresh_token
        .as_deref()
        .is_some_and(|s| s.trim().is_empty())
    {
        return Err("token refresh returned an empty refresh token".into());
    }
    Ok(renewed(
        current,
        access,
        response.refresh_token,
        response.id_token.as_deref(),
    ))
}
fn renewed(
    current: &OAuthToken,
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<&str>,
) -> OAuthToken {
    let claims = id_token.and_then(decode_claims);
    let auth = claims.as_ref().and_then(|c| c.auth.as_ref());
    let Grant::Codex {
        account_id,
        workspace,
        plan,
    } = &current.grant
    else {
        unreachable!("Codex refresh dispatch")
    };
    OAuthToken {
        expires_at: expiry_secs(&access_token, id_token),
        access_token,
        refresh_token: refresh_token.unwrap_or_else(|| current.refresh_token.clone()),
        issued: current.issued.clone(),
        email: claims
            .as_ref()
            .and_then(|c| c.email.clone())
            .or_else(|| current.email.clone()),
        grant: Grant::Codex {
            account_id: account_id.clone(),
            workspace: claims
                .as_ref()
                .and_then(workspace_title)
                .or_else(|| workspace.clone()),
            plan: auth
                .and_then(|a| a.chatgpt_plan_type.clone())
                .or_else(|| plan.clone()),
        },
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    fn jwt(value: &serde_json::Value) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(value.to_string())
        )
    }
    fn raw(subject: &str, account: &str) -> RawTokens {
        RawTokens {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            id_token: jwt(&serde_json::json!({
                "sub":subject, "email":"alex@example.com", "https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_plan_type":"pro"},
                "organizations":[{"title":"Acme","is_default":true}]
            })),
        }
    }
    #[test]
    fn identity_is_the_injective_subject_backend_pair() {
        let first = finalize(raw("person:a", "workspace:b")).unwrap();
        assert_eq!(
            first.issued,
            finalize(raw("person:a", "workspace:b")).unwrap().issued
        );
        assert_ne!(
            first.issued,
            finalize(raw("person:b", "workspace:b")).unwrap().issued
        );
        assert_ne!(
            first.issued,
            finalize(raw("person:a", "workspace:c")).unwrap().issued
        );
        assert_ne!(
            finalize(raw("a:b", "c")).unwrap().issued,
            finalize(raw("a", "b:c")).unwrap().issued
        );
        assert_eq!(first.handle(), "alex@example.com (Acme)");
    }
    #[test]
    fn missing_subject_or_backend_is_rejected() {
        for (sub, account) in [("", "a"), ("a", ""), (" ", "a")] {
            assert!(finalize(raw(sub, account)).is_err());
        }
        let mut value = raw("a", "b");
        value.id_token = jwt(&serde_json::json!({}));
        assert!(finalize(value).is_err());
    }
    #[test]
    fn refresh_pins_both_identity_and_raw_header() {
        let current = finalize(raw("subject", "backend")).unwrap();
        let changed_claims = jwt(&serde_json::json!({
            "sub": "other", "email": "new@example.com", "exp": 12345,
            "https://api.openai.com/auth": { "chatgpt_account_id": "other-backend", "chatgpt_plan_type": "team" },
            "organizations": [{ "title": "New workspace", "is_default": true }],
        }));
        let fresh = renewed(&current, "new-access".into(), None, Some(&changed_claims));
        assert_eq!(fresh.email.as_deref(), Some("new@example.com"));
        assert_eq!(fresh.expires_at, 12345);
        assert_eq!(fresh.handle(), "new@example.com (New workspace)");
        assert_eq!(fresh.issued, current.issued);
        assert_eq!(fresh.refresh_token, "refresh");
        assert!(
            fresh
                .request_headers("application/json")
                .contains(&("chatgpt-account-id".into(), "backend".into()))
        );
        let retained = renewed(&current, "new".into(), Some("rotated".into()), None);
        assert_eq!(retained.grant, current.grant);
        assert_eq!(retained.email, current.email);
        assert_eq!(retained.refresh_token, "rotated");
    }
    #[test]
    fn handle_qualifiers_fall_back_to_plan_then_issued() {
        let mut token = finalize(raw("a", "b")).unwrap();
        token.email = None;
        let Grant::Codex {
            workspace, plan, ..
        } = &mut token.grant
        else {
            unreachable!()
        };
        *workspace = None;
        *plan = Some("plus".into());
        assert_eq!(token.handle(), format!("{} (plus)", token.issued));
        let Grant::Codex { plan, .. } = &mut token.grant else {
            unreachable!()
        };
        *plan = None;
        assert_eq!(token.handle(), token.issued);
    }
    #[test]
    fn expiry_uses_access_then_id_then_one_hour() {
        let access = jwt(&serde_json::json!({"exp":123}));
        let id = jwt(&serde_json::json!({"exp":456}));
        assert_eq!(expiry_secs(&access, Some(&id)), 123);
        assert_eq!(expiry_secs("opaque", Some(&id)), 456);
        let before = crate::app::now_secs() + 3600;
        let expiry = expiry_secs("opaque", None);
        assert!((before..=crate::app::now_secs() + 3600).contains(&expiry));
    }
    #[test]
    fn intervals_accept_wire_variants_and_default_zero() {
        for (interval, expected) in [
            (None, 5),
            (Some("0"), 5),
            (Some("3"), 3),
            (Some("\" 4 \""), 4),
        ] {
            let suffix = interval
                .map(|v| format!(",\"interval\":{v}"))
                .unwrap_or_default();
            let code: UserCode = serde_json::from_str(&format!(
                "{{\"device_auth_id\":\"d\",\"usercode\":\"u\"{suffix}}}"
            ))
            .unwrap();
            assert_eq!(code.interval, expected);
        }
        for invalid in ["-1", "1.5", "\"bad\"", "null"] {
            assert!(
                serde_json::from_str::<UserCode>(&format!(
                    "{{\"device_auth_id\":\"d\",\"user_code\":\"u\",\"interval\":{invalid}}}"
                ))
                .is_err()
            );
        }
    }
    #[test]
    fn only_403_and_404_are_pending() {
        for status in [200, 400, 401, 403, 404, 429, 500] {
            assert_eq!(
                pending(reqwest::StatusCode::from_u16(status).unwrap()),
                matches!(status, 403 | 404)
            );
        }
    }
    #[test]
    fn deadline_and_cancel_prevent_new_poll() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(remaining(Instant::now(), Duration::ZERO, &flag).is_err());
        flag.store(true, Ordering::Relaxed);
        assert!(
            remaining(Instant::now(), MAX_WAIT, &flag)
                .unwrap_err()
                .contains("cancelled")
        );
    }
    #[test]
    fn version_override_is_one_encoded_query_value() {
        assert_eq!(version(Some("  ")), DEFAULT_CODEX_CLIENT_VERSION);
        assert_eq!(version(Some(" 1.2.3 ")), "1.2.3");
        let url = reqwest::Url::parse(&models_url_for("1&extra=value +")).unwrap();
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            [("client_version".into(), "1&extra=value +".into())]
        );
    }
    #[test]
    fn authorize_parameters_preserve_the_codex_protocol() {
        let url = reqwest::Url::parse(
            &authorize_url("http://localhost:1457/auth/callback", "challenge", "state").unwrap(),
        )
        .unwrap();
        let pairs: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(pairs.len(), 10);
        for (key, value) in [
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", "http://localhost:1457/auth/callback"),
            ("scope", SCOPE),
            ("code_challenge", "challenge"),
            ("code_challenge_method", "S256"),
            ("state", "state"),
            ("originator", ORIGINATOR),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
        ] {
            assert_eq!(pairs.get(key).map(AsRef::as_ref), Some(value));
        }
    }
    #[test]
    fn requests_use_exact_paths_and_wire_formats() {
        let client = reqwest::Client::new();
        let request = exchange_request(&client, "http://localhost", "redirect", "code", "verifier")
            .build()
            .unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(request.url().path(), "/oauth/token");
        assert_eq!(
            request.headers()["content-type"],
            "application/x-www-form-urlencoded"
        );
        assert_eq!(
            std::str::from_utf8(request.body().unwrap().as_bytes().unwrap()).unwrap(),
            "grant_type=authorization_code&code=code&redirect_uri=redirect&client_id=app_EMoamEEZ73f0CkXaXp7hrann&code_verifier=verifier"
        );
        let current = finalize(raw("subject", "backend")).unwrap();
        let request = refresh_request(&client, "http://localhost", &current)
            .build()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                request.body().unwrap().as_bytes().unwrap()
            )
            .unwrap(),
            serde_json::json!({"client_id":CLIENT_ID,"grant_type":"refresh_token","refresh_token":"refresh"})
        );
        let request = user_code_request(&client, "http://localhost")
            .build()
            .unwrap();
        assert_eq!(request.url().path(), "/api/accounts/deviceauth/usercode");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                request.body().unwrap().as_bytes().unwrap()
            )
            .unwrap(),
            serde_json::json!({"client_id":CLIENT_ID})
        );
        let code = UserCode {
            device_auth_id: "device".into(),
            user_code: "user".into(),
            interval: 5,
        };
        let request = poll_request(&client, "http://localhost", &code)
            .build()
            .unwrap();
        assert_eq!(request.url().path(), "/api/accounts/deviceauth/token");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                request.body().unwrap().as_bytes().unwrap()
            )
            .unwrap(),
            serde_json::json!({"device_auth_id":"device","user_code":"user"})
        );
    }
    #[tokio::test]
    async fn pending_wait_never_polls_after_expiry_or_cancellation() {
        use std::io::{Read, Write};
        for cancel_after_reply in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let issuer = format!("http://{}", listener.local_addr().unwrap());
            let cancel = Arc::new(AtomicBool::new(false));
            let server_cancel = Arc::clone(&cancel);
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = [0; 4096];
                assert!(stream.read(&mut bytes).unwrap() > 0);
                if cancel_after_reply {
                    server_cancel.store(true, Ordering::Relaxed);
                }
                stream
                    .write_all(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                listener.set_nonblocking(true).unwrap();
                listener
            });
            let code = UserCode {
                device_auth_id: "d".into(),
                user_code: "u".into(),
                interval: 60,
            };
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            let error = poll_for_code(&client, &issuer, &code, &cancel, Duration::from_millis(100))
                .await
                .err()
                .unwrap();
            assert!(
                error.contains(if cancel_after_reply {
                    "cancelled"
                } else {
                    "timed out"
                }),
                "{error}"
            );
            let listener = server.join().unwrap();
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }
    #[test]
    fn empty_credentials_never_enter_the_live_token() {
        for empty in ["", "  "] {
            let mut value = raw("subject", "backend");
            value.access_token = empty.into();
            assert!(finalize(value).is_err());
            let mut value = raw("subject", "backend");
            value.refresh_token = empty.into();
            assert!(finalize(value).is_err());
            let current = finalize(raw("subject", "backend")).unwrap();
            assert!(
                refresh_success(
                    &current,
                    RefreshResponse {
                        access_token: Some("new".into()),
                        refresh_token: Some(empty.into()),
                        id_token: None,
                    }
                )
                .is_err()
            );
            assert!(
                refresh_success(
                    &current,
                    RefreshResponse {
                        access_token: Some(empty.into()),
                        refresh_token: None,
                        id_token: None,
                    }
                )
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn successful_poll_rechecks_cancel_after_body() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = [0; 4096];
            assert!(stream.read(&mut bytes).unwrap() > 0);
            let body = r#"{"authorization_code":"code","code_verifier":"verifier"}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            flag.store(true, Ordering::Relaxed);
            stream.write_all(body.as_bytes()).unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let code = UserCode {
            device_auth_id: "d".into(),
            user_code: "u".into(),
            interval: 5,
        };
        let result = poll_for_code(&client, &issuer, &code, &cancel, MAX_WAIT).await;
        assert!(result.err().unwrap().contains("cancelled"));
        server.join().unwrap();
    }
}
