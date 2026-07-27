//! The CONNECT door: the one HTTP surface a guest speaks to this proxy.
//!
//! [`decode`] is a pure decision over bytes an adversary chose — no I/O, no
//! DNS, no policy lookup. It admits `CONNECT host:443 HTTP/1.1` with a
//! matching `Host` header and refuses everything else, including the
//! userinfo-confusion form (`user@ip`) that a naive `Authority` parse would
//! decode cleanly. This is the crate's `cargo-fuzz` target.

/// The head cap: past this many bytes without a complete head, the
/// connection is refused rather than read further.
pub const HEAD_MAX: usize = 8 * 1024;

/// Why a CONNECT was refused, and the status a pre-tunnel HTTP error may
/// carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub reason: &'static str,
}

/// One admitted CONNECT request head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connect {
    pub host: exarch::net_policy::Host,
}

/// Decode a CONNECT head. `Ok(None)` means the head is not complete yet —
/// read more. Any `Err` is final: close the connection.
///
/// # Errors
/// Returns [`Refusal`] for a malformed head, a non-`CONNECT` method, a
/// target naming userinfo, an IP literal, a non-443 port, a missing/
/// repeated/disagreeing `Host` header, a surplus body, or an over-long head.
pub fn decode(buf: &[u8]) -> Result<Option<Connect>, Refusal> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    let head_len = match req.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => {
            return if buf.len() > HEAD_MAX {
                Err(Refusal {
                    status: 431,
                    reason: "CONNECT head exceeds 8 KiB",
                })
            } else {
                Ok(None)
            };
        }
        Err(_) => {
            return Err(Refusal {
                status: 400,
                reason: "malformed request head",
            });
        }
    };
    if head_len > HEAD_MAX {
        return Err(Refusal {
            status: 431,
            reason: "CONNECT head exceeds 8 KiB",
        });
    }
    if buf.len() > head_len {
        return Err(Refusal {
            status: 400,
            reason: "surplus bytes after CONNECT head",
        });
    }
    if req.version != Some(1) {
        return Err(Refusal {
            status: 400,
            reason: "HTTP/1.1 required",
        });
    }
    if req.method != Some("CONNECT") {
        return Err(Refusal {
            status: 405,
            reason: "only CONNECT is accepted",
        });
    }

    // `http::uri::Authority` parses userinfo successfully, so this refusal
    // must happen on the raw target, never delegated to a parse failure.
    let target = req.path.ok_or(Refusal {
        status: 400,
        reason: "missing request target",
    })?;
    if target.as_bytes().contains(&b'@') {
        return Err(Refusal {
            status: 403,
            reason: "userinfo not accepted in CONNECT target",
        });
    }
    let authority: http::uri::Authority = target.parse().map_err(|_| Refusal {
        status: 400,
        reason: "malformed CONNECT target",
    })?;

    let host = authority.host();
    let is_ip_literal = match host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(v6) => v6.parse::<std::net::Ipv6Addr>().is_ok(),
        None => host.parse::<std::net::IpAddr>().is_ok(),
    };
    if is_ip_literal {
        return Err(Refusal {
            status: 403,
            reason: "IP literal target refused",
        });
    }
    match authority.port_u16() {
        Some(443) => {}
        Some(_) => {
            return Err(Refusal {
                status: 403,
                reason: "only port 443 is accepted",
            });
        }
        None => {
            return Err(Refusal {
                status: 403,
                reason: "target is missing a port",
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
                    reason: "repeated Host header",
                });
            }
            host_header = Some(
                std::str::from_utf8(h.value)
                    .map_err(|_| Refusal {
                        status: 400,
                        reason: "Host header is not UTF-8",
                    })?
                    .trim()
                    .to_ascii_lowercase(),
            );
        }
    }
    let host_header = host_header.ok_or(Refusal {
        status: 400,
        reason: "missing Host header",
    })?;
    if host_header != target_host && host_header != format!("{target_host}:443") {
        return Err(Refusal {
            status: 400,
            reason: "Host header disagrees with target",
        });
    }

    let host = exarch::net_policy::Host::parse(&target_host).map_err(|_| Refusal {
        status: 403,
        reason: "host rejected by policy shape",
    })?;
    Ok(Some(Connect { host }))
}

#[cfg(test)]
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
    fn get_is_refused() {
        let buf = head(&["GET example.com:443 HTTP/1.1", "Host: example.com"]);
        assert_eq!(decode(&buf).unwrap_err().status, 405);
    }

    #[test]
    fn post_is_refused() {
        let buf = head(&["POST example.com:443 HTTP/1.1", "Host: example.com"]);
        assert_eq!(decode(&buf).unwrap_err().status, 405);
    }

    #[test]
    fn head_method_is_refused() {
        let buf = head(&["HEAD example.com:443 HTTP/1.1", "Host: example.com"]);
        assert_eq!(decode(&buf).unwrap_err().status, 405);
    }

    #[test]
    fn put_is_refused() {
        let buf = head(&["PUT example.com:443 HTTP/1.1", "Host: example.com"]);
        assert_eq!(decode(&buf).unwrap_err().status, 405);
    }

    #[test]
    fn delete_is_refused() {
        let buf = head(&["DELETE example.com:443 HTTP/1.1", "Host: example.com"]);
        assert_eq!(decode(&buf).unwrap_err().status, 405);
    }

    #[test]
    fn options_is_refused() {
        let buf = head(&["OPTIONS example.com:443 HTTP/1.1", "Host: example.com"]);
        assert_eq!(decode(&buf).unwrap_err().status, 405);
    }

    #[test]
    fn patch_is_refused() {
        let buf = head(&["PATCH example.com:443 HTTP/1.1", "Host: example.com"]);
        assert_eq!(decode(&buf).unwrap_err().status, 405);
    }

    #[test]
    fn trace_is_refused() {
        let buf = head(&["TRACE example.com:443 HTTP/1.1", "Host: example.com"]);
        assert_eq!(decode(&buf).unwrap_err().status, 405);
    }

    #[test]
    fn lowercase_connect_is_refused() {
        let buf = head(&["connect example.com:443 HTTP/1.1", "Host: example.com"]);
        assert_eq!(decode(&buf).unwrap_err().status, 405);
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

    /// The permanent regression: `Authority` parses userinfo cleanly, so a
    /// hostname-shaped username in front of a metadata-service IP must be
    /// refused on the userinfo check — never fall through to a later one.
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
    fn a_plain_user_at_host_target_is_refused_on_userinfo() {
        let buf = connect("user@example.com:443", "example.com:443");
        let e = decode(&buf).unwrap_err();
        assert_eq!(e.reason, "userinfo not accepted in CONNECT target");
    }

    #[test]
    fn an_absolute_url_target_is_refused() {
        let buf = head(
            [
                "CONNECT https://example.com/:443 HTTP/1.1",
                "Host: example.com",
            ]
            .as_ref(),
        );
        assert!(decode(&buf).is_err());
    }

    #[test]
    fn a_path_target_is_refused() {
        let buf = head(&["CONNECT /index.html HTTP/1.1", "Host: example.com"]);
        assert!(decode(&buf).is_err());
    }

    #[test]
    fn a_fragment_in_the_target_is_refused() {
        let buf = connect("example.com:443#frag", "example.com:443");
        assert!(decode(&buf).is_err());
    }

    #[test]
    fn control_bytes_in_the_target_are_refused() {
        let buf = connect("example.com:44\x013", "example.com:443");
        assert!(decode(&buf).is_err());
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
    fn a_host_header_with_an_explicit_443_matches() {
        let c = decode(&connect("example.com:443", "example.com:443"))
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

    /// The seed corpus `cargo-fuzz` would run this target against, replayed
    /// here so the same inputs are exercised by the normal test suite.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "a test-only replay of the crate's own checked-in fuzz corpus, not turn-time model I/O"
    )]
    fn the_fuzz_corpus_never_panics_the_parser() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/connect");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let bytes = std::fs::read(entry.path()).expect("readable corpus file");
            let _ = decode(&bytes);
        }
    }
}
