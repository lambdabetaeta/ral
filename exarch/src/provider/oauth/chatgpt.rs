//! `OpenAI`'s devday token-sharing protocol, retaining its issued-client flow.

use super::{
    Grant, LoginPhase, OAuthToken, RefreshError, RefreshResponse, RefreshUpdate, Revocation,
    SignIn, json_or_error, jwt_payload, loopback, revoke_at, submit_refresh,
};
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const DISCOVERY_URL: &str = "https://auth.openai.com/.well-known/openid-configuration";
/// The first-registration client; the callback answers with the issued one.
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const AGENT_NAME: &str = "exarch";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The one scope that authorises plan usage; a grant without it is refused.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const RESOURCE: &str = "https://api.openai.com/v1";

#[derive(Deserialize)]
// The suffix is the token-endpoint wire format.
#[allow(clippy::struct_field_names)]
struct RawTokens {
    id_token: String,
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    scope: String,
}

struct Granted {
    raw: RawTokens,
    client_id: String,
    nonce: String,
}

pub(super) async fn sign_in(
    client: &reqwest::Client,
    host_id: &str,
    sign_in: &SignIn,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<OAuthToken, String> {
    let (verifier, challenge) = super::pkce();
    let state = super::random_b64url(32);
    let nonce = super::random_b64url(32);
    let listener = loopback::bind(&["127.0.0.1:1455", "127.0.0.1:0"])?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("could not read listener address: {e}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{}", loopback::CALLBACK_PATH);
    let url = authorize_url(&redirect_uri, &challenge, &state, &nonce, host_id, sign_in)?;
    // Launch success does not establish that a remote user's browser is visible.
    let _ = loopback::launch_browser(&url);
    on_phase(LoginPhase::AwaitingBrowser { url });

    let cancel = Arc::clone(cancel);
    let callback =
        tokio::task::spawn_blocking(move || loopback::accept_callback(&listener, &state, &cancel))
            .await
            .map_err(|e| format!("callback listener panicked: {e}"))??;
    let client_id = callback_client(sign_in, callback.client_id)?;
    on_phase(LoginPhase::ExchangingCode);
    let raw = exchange_code(client, &client_id, &redirect_uri, &callback.code, &verifier).await?;
    finalize(Granted {
        raw,
        client_id,
        nonce,
    })
}

/// A renewal retains the account's issued client and ID-token hint.
fn previous_grant(sign_in: &SignIn) -> Result<Option<(&str, &str)>, String> {
    match sign_in {
        SignIn::New(_, _) => Ok(None),
        SignIn::Renew(token, _) => match &token.grant {
            Grant::Chatgpt {
                id_token,
                client_id,
            } => Ok(Some((id_token, client_id))),
            Grant::Codex { .. } | Grant::Grok => {
                Err("this account does not use the ChatGPT token-sharing flow".to_string())
            }
        },
    }
}

fn callback_client(sign_in: &SignIn, issued: Option<String>) -> Result<String, String> {
    match (previous_grant(sign_in)?, issued) {
        (None, Some(issued)) => Ok(issued),
        (None, None) => Err("the sign-in callback carried no issued client id; OpenAI's registration did not complete".to_string()),
        (Some((_, client_id)), Some(issued)) if issued != client_id => Err("the sign-in callback names a different client than this account's; nothing was changed".to_string()),
        (Some((_, client_id)), _) => Ok(client_id.to_string()),
    }
}

fn authorize_url(
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    nonce: &str,
    host_id: &str,
    sign_in: &SignIn,
) -> Result<String, String> {
    let previous = previous_grant(sign_in)?;
    let client_id = previous.map_or(DYNAMIC_CLIENT, |(_, client_id)| client_id);
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
            SignIn::New(_, _) => {
                pairs.append_pair("agent_name_hint", AGENT_NAME);
            }
            SignIn::Renew(token, _) => {
                if let Some((id_token, _)) = previous {
                    pairs.append_pair("id_token_hint", id_token);
                }
                if let Some(email) = &token.email {
                    pairs.append_pair("login_hint", email);
                }
            }
        }
    }
    Ok(url.into())
}

async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<RawTokens, String> {
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("resource", RESOURCE),
        ])
        .send()
        .await
        .map_err(|e| format!("token exchange request failed: {e}"))?;
    json_or_error(resp, "token exchange").await
}

/// The unverified payload arrives directly from the issuer over TLS; it is
/// used for account naming and this attempt's nonce, not as an auth verifier.
#[derive(Deserialize)]
struct IdClaims {
    sub: String,
    nonce: Option<String>,
    email: Option<String>,
}

fn finalize(granted: Granted) -> Result<OAuthToken, String> {
    let Granted {
        raw,
        client_id,
        nonce,
    } = granted;
    if !raw
        .scope
        .split_whitespace()
        .any(|scope| scope == PLAN_SCOPE)
    {
        return Err(format!(
            "the sign-in granted no ChatGPT plan usage (scope: {}); does this account's plan allow sharing with apps?",
            raw.scope
        ));
    }
    let claims: IdClaims = jwt_payload(&raw.id_token)?;
    if claims.nonce.as_deref() != Some(nonce.as_str()) {
        return Err("the sign-in's ID token answers a different attempt; try again".to_string());
    }
    if claims.sub.is_empty() {
        return Err("the sign-in's ID token carried no account subject".to_string());
    }
    Ok(OAuthToken {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        issued: claims.sub,
        email: claims.email,
        expires_at: crate::app::now_secs() + raw.expires_in,
        grant: Grant::Chatgpt {
            id_token: raw.id_token,
            client_id,
        },
    })
}

pub(super) async fn refresh(
    client: &reqwest::Client,
    current: &OAuthToken,
    client_id: &str,
    id_token: &str,
) -> Result<RefreshUpdate<String>, RefreshError> {
    let request = client.post(TOKEN_URL).form(&[
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", current.refresh_token.as_str()),
        ("resource", RESOURCE),
    ]);
    let resp = submit_refresh(request, current).await?;
    Ok(renewed(current, id_token, resp))
}

fn renewed(current: &OAuthToken, id_token: &str, resp: RefreshResponse) -> RefreshUpdate<String> {
    let email = resp
        .id_token
        .as_deref()
        .and_then(|jwt| jwt_payload::<IdClaims>(jwt).ok())
        .and_then(|claims| claims.email)
        .or_else(|| current.email.clone());
    RefreshUpdate {
        access_token: resp.access_token,
        refresh_token: resp
            .refresh_token
            .unwrap_or_else(|| current.refresh_token.clone()),
        email,
        expires_at: crate::app::now_secs() + resp.expires_in.unwrap_or(3600),
        metadata: resp.id_token.unwrap_or_else(|| id_token.to_string()),
    }
}

pub(super) async fn revoke(token: &OAuthToken, client_id: &str) -> Result<Revocation, String> {
    revoke_at(DISCOVERY_URL, token, client_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::identity::Login;
    use crate::provider::oauth::Method;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn token() -> OAuthToken {
        OAuthToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            issued: "acct-1".into(),
            email: Some("old@work".into()),
            expires_at: 0,
            grant: Grant::Chatgpt {
                id_token: "retained-id".into(),
                client_id: "oaiapp_test".into(),
            },
        }
    }

    fn fake_id_token(payload: &serde_json::Value) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(payload.to_string())
        )
    }

    fn response(id_token: Option<&str>, refresh_token: Option<&str>) -> RefreshResponse {
        RefreshResponse {
            access_token: "fresh-at".into(),
            refresh_token: refresh_token.map(str::to_string),
            id_token: id_token.map(str::to_string),
            expires_in: Some(3600),
        }
    }

    #[test]
    fn a_refresh_updates_the_name_and_id_token() {
        let current = token();
        let id_token = fake_id_token(&json!({"sub": "other", "email": "new@work"}));
        let fresh = renewed(&current, "retained-id", response(Some(&id_token), None));
        assert_eq!(fresh.email.as_deref(), Some("new@work"));
        assert_eq!(fresh.refresh_token, "rt");
        assert_eq!(fresh.metadata, id_token);
    }

    #[test]
    fn a_refresh_without_claims_keeps_the_name_and_id_token() {
        let current = token();
        let fresh = renewed(&current, "retained-id", response(None, Some("fresh-rt")));
        assert_eq!(fresh.email, current.email);
        assert_eq!(fresh.metadata, "retained-id");
        assert_eq!(fresh.refresh_token, "fresh-rt");
    }

    fn granted(scope: &str, claims: &serde_json::Value) -> Granted {
        Granted {
            raw: RawTokens {
                id_token: fake_id_token(claims),
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_in: 3600,
                scope: scope.into(),
            },
            client_id: "oaiapp_test".into(),
            nonce: "n1".into(),
        }
    }

    const FULL_SCOPE: &str = "openid email chatgpt.tokens.use.direct";

    #[test]
    fn a_grant_without_plan_scope_is_refused() {
        let error = finalize(granted("openid email", &json!({"sub": "a", "nonce": "n1"})))
            .expect_err("no plan scope");
        assert!(error.contains("plan"), "{error}");
    }

    #[test]
    fn a_mismatched_nonce_is_refused() {
        let error = finalize(granted(FULL_SCOPE, &json!({"sub": "a", "nonce": "other"})))
            .expect_err("wrong attempt");
        assert!(error.contains("different attempt"), "{error}");
    }

    #[test]
    fn missing_or_empty_subject_is_refused() {
        for claims in [json!({"nonce": "n1"}), json!({"sub": "", "nonce": "n1"})] {
            assert!(finalize(granted(FULL_SCOPE, &claims)).is_err());
        }
    }

    #[test]
    fn initial_registration_requires_an_issued_client() {
        let new = SignIn::New(Login::Chatgpt, Method::Browser);
        assert!(callback_client(&new, None).is_err());
        assert_eq!(
            callback_client(&new, Some("issued".into())).unwrap(),
            "issued"
        );
    }

    #[test]
    fn renewal_pins_the_issued_client() {
        let renew = SignIn::Renew(token(), Method::Browser);
        assert!(callback_client(&renew, Some("other".into())).is_err());
        assert_eq!(callback_client(&renew, None).unwrap(), "oaiapp_test");
        assert_eq!(
            callback_client(&renew, Some("oaiapp_test".into())).unwrap(),
            "oaiapp_test"
        );
    }

    #[test]
    fn authorize_urls_preserve_registration_and_renewal_protocol() {
        for sign_in in [
            SignIn::New(Login::Chatgpt, Method::Browser),
            SignIn::Renew(token(), Method::Browser),
        ] {
            let url = reqwest::Url::parse(
                &authorize_url(
                    "http://127.0.0.1:1455/auth/callback",
                    "challenge",
                    "state",
                    "nonce",
                    "host",
                    &sign_in,
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(url.path(), "/api/accounts/authorize");
            let pairs: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
            for (key, value) in [
                ("response_type", "code"),
                ("scope", SCOPE),
                ("resource", RESOURCE),
                ("state", "state"),
                ("nonce", "nonce"),
                ("code_challenge", "challenge"),
                ("code_challenge_method", "S256"),
                ("ext_agent_host_id", "host"),
            ] {
                assert_eq!(pairs[key], value);
            }
            match sign_in {
                SignIn::New(_, _) => {
                    assert_eq!(pairs["client_id"], DYNAMIC_CLIENT);
                    assert_eq!(pairs["agent_name_hint"], AGENT_NAME);
                    assert!(!pairs.contains_key("id_token_hint"));
                    assert!(!pairs.contains_key("login_hint"));
                }
                SignIn::Renew(_, _) => {
                    assert_eq!(pairs["client_id"], "oaiapp_test");
                    assert_eq!(pairs["id_token_hint"], "retained-id");
                    assert_eq!(pairs["login_hint"], "old@work");
                    assert!(!pairs.contains_key("agent_name_hint"));
                }
            }
        }
    }
}
