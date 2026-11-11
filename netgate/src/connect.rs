//! Pure CONNECT decoding: no I/O, DNS or policy lookup.
//! Only `host:443` with one agreeing `Host` header is admitted.

use std::borrow::Cow;

use ral_core::egress::Host;

use crate::refusal::Refusal;

pub const HEAD_MAX: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connect {
    pub host: Host,
}

/// # Errors
/// Incomplete heads return `Ok(None)`; invalid CONNECT heads return a final [`Refusal`].
pub fn decode(buf: &[u8]) -> Result<Option<Connect>, Refusal> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    let head_len = match req.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => {
            return if buf.len() > HEAD_MAX {
                Err(Refusal {
                    status: 431,
                    reason: Cow::Borrowed("CONNECT head exceeds 8 KiB"),
                })
            } else {
                Ok(None)
            };
        }
        Err(_) => {
            return Err(Refusal {
                status: 400,
                reason: Cow::Borrowed("malformed request head"),
            });
        }
    };
    if head_len > HEAD_MAX {
        return Err(Refusal {
            status: 431,
            reason: Cow::Borrowed("CONNECT head exceeds 8 KiB"),
        });
    }
    if buf.len() > head_len {
        return Err(Refusal {
            status: 400,
            reason: Cow::Borrowed("surplus bytes after CONNECT head"),
        });
    }
    if req.version != Some(1) {
        return Err(Refusal {
            status: 400,
            reason: Cow::Borrowed("HTTP/1.1 required"),
        });
    }
    if req.method != Some("CONNECT") {
        return Err(Refusal {
            status: 405,
            reason: Cow::Borrowed("only CONNECT is accepted"),
        });
    }

    // Reject raw userinfo before `Authority` parsing, which accepts it.
    let target = req.path.ok_or(Refusal {
        status: 400,
        reason: Cow::Borrowed("missing request target"),
    })?;
    if target.as_bytes().contains(&b'@') {
        return Err(Refusal {
            status: 403,
            reason: Cow::Borrowed("userinfo not accepted in CONNECT target"),
        });
    }
    let authority: http::uri::Authority = target.parse().map_err(|_| Refusal {
        status: 400,
        reason: Cow::Borrowed("malformed CONNECT target"),
    })?;

    let host = authority.host();
    let is_ip_literal = match host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(v6) => v6.parse::<std::net::Ipv6Addr>().is_ok(),
        None => host.parse::<std::net::IpAddr>().is_ok(),
    };
    if is_ip_literal {
        return Err(Refusal {
            status: 403,
            reason: Cow::Borrowed("IP literal target refused"),
        });
    }
    match authority.port_u16() {
        Some(443) => {}
        Some(_) => {
            return Err(Refusal {
                status: 403,
                reason: Cow::Borrowed("only port 443 is accepted"),
            });
        }
        None => {
            return Err(Refusal {
                status: 403,
                reason: Cow::Borrowed("target is missing a port"),
            });
        }
    }
    let target_host = host.to_ascii_lowercase();

    let mut host_header = None;
    for h in req.headers.iter() {
        if h.name.is_empty() {
            break;
        }
        if h.name.eq_ignore_ascii_case("host") {
            if host_header.is_some() {
                return Err(Refusal {
                    status: 400,
                    reason: Cow::Borrowed("repeated Host header"),
                });
            }
            host_header = Some(
                std::str::from_utf8(h.value)
                    .map_err(|_| Refusal {
                        status: 400,
                        reason: Cow::Borrowed("Host header is not UTF-8"),
                    })?
                    .trim()
                    .to_ascii_lowercase(),
            );
        }
    }
    let host_header = host_header.ok_or(Refusal {
        status: 400,
        reason: Cow::Borrowed("missing Host header"),
    })?;
    if host_header != target_host && host_header != format!("{target_host}:443") {
        return Err(Refusal {
            status: 400,
            reason: Cow::Borrowed("Host header disagrees with target"),
        });
    }

    let host = Host::parse(&target_host).map_err(|_| Refusal {
        status: 403,
        reason: Cow::Borrowed("host rejected by policy shape"),
    })?;
    Ok(Some(Connect { host }))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn head(lines: &[&str]) -> Vec<u8> {
        format!("{}\r\n\r\n", lines.join("\r\n")).into_bytes()
    }

    fn connect(target: &str, host_header: &str) -> Vec<u8> {
        head(&[
            &format!("CONNECT {target} HTTP/1.1"),
            &format!("Host: {host_header}"),
        ])
    }

    #[test]
    fn a_valid_connect_with_a_matching_host_is_admitted() {
        let c = decode(&connect("example.com:443", "example.com:443"))
            .unwrap()
            .unwrap();
        assert_eq!(c.host.as_str(), "example.com");
    }

    #[test]
    fn non_connect_methods_are_refused_with_405() {
        for method in [
            "GET", "POST", "HEAD", "PUT", "DELETE", "OPTIONS", "PATCH", "TRACE", "connect",
        ] {
            let buf = head(&[
                &format!("{method} example.com:443 HTTP/1.1"),
                "Host: example.com",
            ]);
            assert_eq!(decode(&buf).unwrap_err().status, 405, "{method}");
        }
    }

    #[test]
    fn a_missing_host_header_is_refused() {
        let buf = head(&["CONNECT example.com:443 HTTP/1.1"]);
        let e = decode(&buf).unwrap_err();
        assert_eq!(e.status, 400);
        assert_eq!(e.reason, "missing Host header");
    }

    #[test]
    fn a_repeated_host_header_is_refused() {
        let buf = head(&[
            "CONNECT example.com:443 HTTP/1.1",
            "Host: example.com:443",
            "Host: example.com:443",
        ]);
        let e = decode(&buf).unwrap_err();
        assert_eq!(e.status, 400);
        assert_eq!(e.reason, "repeated Host header");
    }

    #[test]
    fn a_disagreeing_host_header_is_refused() {
        let buf = connect("example.com:443", "other.example:443");
        let e = decode(&buf).unwrap_err();
        assert_eq!(e.status, 400);
        assert_eq!(e.reason, "Host header disagrees with target");
    }

    #[test]
    fn a_missing_port_is_refused() {
        let buf = connect("example.com", "example.com");
        assert_eq!(decode(&buf).unwrap_err().status, 403);
    }

    #[test]
    fn a_non_443_port_is_refused() {
        let buf = connect("example.com:8443", "example.com:8443");
        assert_eq!(decode(&buf).unwrap_err().status, 403);
    }

    #[test]
    fn an_ipv4_literal_target_is_refused() {
        let buf = connect("169.254.169.254:443", "169.254.169.254:443");
        let e = decode(&buf).unwrap_err();
        assert_eq!(e.status, 403);
        assert_eq!(e.reason, "IP literal target refused");
    }

    #[test]
    fn a_bracketed_ipv6_literal_target_is_refused() {
        let buf = connect("[::1]:443", "[::1]:443");
        let e = decode(&buf).unwrap_err();
        assert_eq!(e.status, 403);
        assert_eq!(e.reason, "IP literal target refused");
    }

    #[test]
    fn userinfo_confusion_fails_on_the_userinfo_check_not_a_later_one() {
        let buf = connect(
            "archive.ubuntu.com@169.254.169.254:443",
            "169.254.169.254:443",
        );
        let e = decode(&buf).unwrap_err();
        assert_eq!(e.status, 403);
        assert_eq!(e.reason, "userinfo not accepted in CONNECT target");
    }

    #[test]
    fn malformed_targets_are_refused_with_400() {
        for target in [
            "https://example.com/:443",
            "/index.html",
            "example.com:443#frag",
            "example.com:44\x013",
        ] {
            let buf = connect(target, "example.com:443");
            assert_eq!(decode(&buf).unwrap_err().status, 400, "{target:?}");
        }
    }

    #[test]
    fn a_partial_head_returns_ok_none() {
        let buf = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example".to_vec();
        assert_eq!(decode(&buf), Ok(None));
    }

    #[test]
    fn an_overlong_incomplete_head_is_refused() {
        let buf = vec![b'a'; HEAD_MAX + 1];
        let e = decode(&buf).unwrap_err();
        assert_eq!(e.status, 431);
    }

    #[test]
    fn a_head_with_a_surplus_body_is_refused() {
        let mut buf = connect("example.com:443", "example.com:443");
        buf.extend_from_slice(b"extra bytes after the head");
        let e = decode(&buf).unwrap_err();
        assert_eq!(e.status, 400);
        assert_eq!(e.reason, "surplus bytes after CONNECT head");
    }

    #[test]
    fn mixed_case_hostnames_normalize_to_the_same_host() {
        let a = decode(&connect("ExAmPlE.CoM:443", "ExAmPlE.CoM:443"))
            .unwrap()
            .unwrap();
        let b = decode(&connect("example.com:443", "example.com:443"))
            .unwrap()
            .unwrap();
        assert_eq!(a.host, b.host);
    }

    #[test]
    fn a_host_header_without_an_explicit_port_matches() {
        let c = decode(&connect("example.com:443", "example.com"))
            .unwrap()
            .unwrap();
        assert_eq!(c.host.as_str(), "example.com");
    }

    #[test]
    fn user_agent_and_proxy_connection_are_ignored_not_refused() {
        let buf = head(&[
            "CONNECT example.com:443 HTTP/1.1",
            "Host: example.com:443",
            "User-Agent: curl/8.0",
            "Proxy-Connection: Keep-Alive",
        ]);
        assert!(decode(&buf).unwrap().is_some());
    }

    #[test]
    fn http_1_0_is_refused() {
        let buf = head(&["CONNECT example.com:443 HTTP/1.0", "Host: example.com:443"]);
        let e = decode(&buf).unwrap_err();
        assert_eq!(e.status, 400);
        assert_eq!(e.reason, "HTTP/1.1 required");
    }

    #[test]
    fn more_than_32_headers_are_refused_with_400() {
        let mut lines = vec![
            "CONNECT example.com:443 HTTP/1.1".to_string(),
            "Host: example.com".to_string(),
        ];
        lines.extend((0..32).map(|i| format!("X-{i}: v")));
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(decode(&head(&lines)).unwrap_err().status, 400);
    }

    #[test]
    fn a_complete_head_over_the_cap_is_refused_with_431() {
        let pad = format!("X-Pad: {}", "a".repeat(HEAD_MAX));
        let buf = head(&[
            "CONNECT example.com:443 HTTP/1.1",
            "Host: example.com",
            &pad,
        ]);
        assert_eq!(decode(&buf).unwrap_err().status, 431);
    }

    #[test]
    fn a_non_utf8_host_header_is_refused_with_400() {
        let buf = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.\xff\r\n\r\n";
        assert_eq!(decode(buf).unwrap_err().status, 400);
    }

    #[test]
    fn a_host_header_on_another_port_is_refused() {
        let buf = connect("example.com:443", "example.com:8443");
        assert_eq!(decode(&buf).unwrap_err().status, 400);
    }

    #[test]
    fn whitespace_around_the_host_header_is_trimmed() {
        let c = decode(&connect("example.com:443", "  example.com:443  "))
            .unwrap()
            .unwrap();
        assert_eq!(c.host.as_str(), "example.com");
    }

    #[test]
    fn hosts_failing_the_policy_shape_are_refused_with_403() {
        for target in ["under_score.example:443", "example.com.:443"] {
            let buf = connect(target, target);
            assert_eq!(decode(&buf).unwrap_err().status, 403, "{target}");
        }
    }
}
