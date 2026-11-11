use super::*;
use ral_core::fact::{JsonInputFailure, RefreshOutcome};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn current() -> OAuthToken {
    OAuthToken {
        access_token: "access-secret".into(),
        refresh_token: "refresh-secret".into(),
        issued: "account".into(),
        email: None,
        expires_at: 0,
        grant: Grant::Grok,
    }
}

async fn reply(
    status: u16,
    body: &'static str,
    extra_length: usize,
) -> Result<RefreshResponse, RefreshError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/token", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        let response = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len() + extra_length
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let result = submit_refresh(client.post(url).body("fixture"), &current()).await;
    server.await.unwrap();
    result
}

fn outcome(error: &RefreshError) -> RefreshOutcome {
    match error {
        RefreshError::NotSubmitted(_) => RefreshOutcome::NotSubmitted,
        RefreshError::Rejected(_) => RefreshOutcome::Rejected,
        RefreshError::Ambiguous(_) => RefreshOutcome::Ambiguous,
    }
}

#[tokio::test]
async fn refresh_non_success_retains_status_based_outcome_even_when_body_read_fails() {
    for (status, expected) in [
        (304, RefreshOutcome::Rejected),
        (400, RefreshOutcome::Rejected),
        (401, RefreshOutcome::Rejected),
        (503, RefreshOutcome::Ambiguous),
    ] {
        let error = reply(status, "", 0).await.err().unwrap();
        assert_eq!(outcome(&error), expected);
        let cause = match error {
            RefreshError::Rejected(cause) | RefreshError::Ambiguous(cause) => cause,
            RefreshError::NotSubmitted(_) => panic!("response was received"),
        };
        let RefreshFailure::Service(service) = cause else {
            panic!("service response")
        };
        assert_eq!(service.operation, ServiceOperation::RefreshResponse);
        assert_eq!(service.service, current().login().service().name.as_str());
        let ServiceCause::RefreshResponse(response) = service.cause.as_ref() else {
            panic!("response evidence")
        };
        assert_eq!(response.status, u32::from(status));
        assert!(matches!(response.body, RefreshResponseBody::Detail(_)));
    }
    for status in [400, 503] {
        let error = reply(status, "incomplete", 20).await.err().unwrap();
        assert_eq!(
            outcome(&error),
            if status == 503 {
                RefreshOutcome::Ambiguous
            } else {
                RefreshOutcome::Rejected
            }
        );
        let cause = match error {
            RefreshError::Rejected(cause) | RefreshError::Ambiguous(cause) => cause,
            RefreshError::NotSubmitted(_) => panic!("response was received"),
        };
        assert!(matches!(
            cause,
            RefreshFailure::Service(ServiceFailure {
                cause,
                ..
            }) if matches!(cause.as_ref(), ServiceCause::RefreshResponse(RefreshServiceResponse {
                body: RefreshResponseBody::ReadFailed(_),
                ..
            }))
        ));
    }
}

#[tokio::test]
async fn refresh_success_transport_syntax_and_local_shape_failures_remain_ambiguous_and_distinct() {
    assert!(matches!(
        reply(200, "incomplete", 20).await,
        Err(RefreshError::Ambiguous(RefreshFailure::Service(
            ServiceFailure {
                operation: ServiceOperation::RefreshResponse,
                cause,
                ..
            }
        ))) if matches!(cause.as_ref(), ServiceCause::Reqwest(_))
    ));
    assert!(matches!(
        reply(200, "{", 0).await,
        Err(RefreshError::Ambiguous(RefreshFailure::Response(
            RefreshFailureResponse {
                status: 200,
                cause: JsonInputFailure::Syntax(_),
                ..
            }
        )))
    ));
    let error = reply(200, r#"{"access_token":false}"#, 0)
        .await
        .err()
        .unwrap();
    let RefreshError::Ambiguous(RefreshFailure::Response(response)) = error else {
        panic!("local decode failure")
    };
    let JsonInputFailure::Decode(failure) = response.cause else {
        panic!("shape, not syntax")
    };
    assert_eq!(
        failure.path,
        [ral_core::fact::DataStep::Field("access_token".into())]
    );
    assert!(matches!(
        *failure.cause,
        ral_core::fact::DecodeCause::JsonShape(_)
    ));
}

#[test]
fn refresh_reply_detail_is_sanitized_bounded_and_not_local_prose() {
    let current = current();
    let body = format!(
        r#"{{"error":{{"code":"nested","message":"{} access-secret refresh-secret"}},"error_code":"later"}}"#,
        "x".repeat(201)
    );
    let response = refresh_failure(reqwest::StatusCode::BAD_REQUEST, &body, &current);
    let RefreshResponseBody::Detail(detail) = response.body else {
        panic!("detail")
    };
    assert_eq!(detail.code.as_deref(), Some("nested"));
    let RefreshResponseDetail::Text(text) = detail.detail else {
        panic!("text")
    };
    assert_eq!(text.preview.chars().count(), 200);
    assert!(text.truncated);
    for (body, expected) in [
        (" <html>secret", RefreshResponseDetail::Html),
        ("  ", RefreshResponseDetail::Empty),
    ] {
        let response = refresh_failure(reqwest::StatusCode::BAD_REQUEST, body, &current);
        let RefreshResponseBody::Detail(detail) = response.body else {
            panic!("detail")
        };
        assert_eq!(detail.detail, expected);
    }
}

#[tokio::test]
async fn refresh_builder_failure_is_not_submitted_and_preserves_the_request_operation() {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let result = submit_refresh::<RefreshResponse>(client.post(":invalid-url"), &current()).await;
    let Err(RefreshError::NotSubmitted(RefreshFailure::Service(service))) = result else {
        panic!("builder failure was not submitted")
    };
    assert_eq!(service.operation, ServiceOperation::RefreshRequest);
    let ServiceCause::Reqwest(cause) = service.cause.as_ref() else {
        panic!("reqwest builder cause")
    };
    assert!(matches!(
        cause.summary.class,
        ral_core::fact::ReqwestClass::Builder
    ));
}
