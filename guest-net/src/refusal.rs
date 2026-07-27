//! Why a guest connection did not go through: one type shared by the audit
//! ledger and the intercepted session's own 403 page.
//!
//! [`Refusal::sentence`] is both the audit note and the text a person reads
//! in a transcript; [`Refusal::page`] is the HTTP response the guest's own
//! client parses, carrying `X-Synod-Blocked: {host}` so nothing downstream
//! has to scrape a body to learn what happened.
//!
//! **The asymmetry is inherent, not an oversight.** [`Refusal::NotOnList`],
//! [`Refusal::VerbDenied`], [`Refusal::TooMany`], [`Refusal::TooBig`] and
//! [`Refusal::NotTrusted`] all arise once a TLS session is already open
//! between the guest and this process — the guest already trusts the
//! session CA, so a 403 page is just another response on a connection that
//! already exists. [`Refusal::RawAddress`] and [`Refusal::WrongName`] arise
//! *before* that: an address DNS never issued has no name to mint a leaf
//! for, and a disagreeing SNI name is refused by [`crate::ca`] simply
//! declining to mint one (`ResolvesServerCert::resolve` returning `None`),
//! so the handshake itself fails. There is no session, so
//! [`Refusal::page`] returns `None` for both — the user learns of these
//! only from the ledger and the card [`Refusal::sentence`] renders.

use std::net::IpAddr;

/// Why a guest connection was refused.
#[derive(Debug, Clone)]
pub enum Refusal {
    /// The guest dialled an address DNS never minted for any name — the
    /// raw-IP bypass the accept gate exists to close.
    RawAddress { addr: IpAddr },
    /// `host` admits no rule on the network policy at all.
    NotOnList { host: String },
    /// The name the request itself names (`Host:` on 80, SNI on 443)
    /// disagrees with `issued_for`, the name DNS minted this connection's
    /// address for.
    WrongName {
        requested: String,
        issued_for: String,
    },
    /// `host` is on the list, but not for `method`.
    VerbDenied { method: String, host: String },
    /// The rate budget for `host` is spent.
    TooMany { host: String },
    /// `host`'s response exceeded the policy's `limit`-byte cap.
    TooBig { host: String, limit: u64 },
    /// The outbound fetch to the real `host` failed — its certificate did
    /// not verify against this computer's trust store, or the connection
    /// itself broke before a complete answer arrived. Either way nothing it
    /// sent reaches the guest.
    NotTrusted { host: String },
}

impl Refusal {
    /// The plain-English sentence: the card the user sees, and the text the
    /// audit ledger records for this refusal.
    #[must_use]
    pub fn sentence(&self) -> String {
        match self {
            Self::RawAddress { addr } => format!(
                "the assistant tried to reach {addr} directly, skipping the site name your IT \
                 department's list is written against, so that connection was refused"
            ),
            Self::NotOnList { host } => exarch::net_policy::refusal(host),
            Self::WrongName {
                requested,
                issued_for,
            } => format!(
                "the assistant reached '{issued_for}' and then asked for '{requested}', which \
                 is not the same site, so that connection was refused"
            ),
            Self::VerbDenied { method, host } => exarch::net_policy::method_refusal(method, host),
            Self::TooMany { host } => format!(
                "the assistant sent too many requests to '{host}' too quickly — ask your IT \
                 department to raise the limit if this assistant needs to send more"
            ),
            Self::TooBig { host, limit } => format!(
                "'{host}' sent back more than the {limit}-byte limit your IT department set — \
                 ask your IT department to raise it if this assistant needs to read more"
            ),
            Self::NotTrusted { host } => format!(
                "'{host}' did not answer with something that could be used — the connection \
                 failed, or the site could not be verified as genuine — so nothing it sent \
                 back was passed on"
            ),
        }
    }

    /// The 403 response the guest's own client reads, or `None` if this
    /// refusal has no session to deliver one into (see the module doc's
    /// asymmetry).
    #[must_use]
    pub fn page(&self) -> Option<Vec<u8>> {
        let host = match self {
            Self::NotOnList { host }
            | Self::VerbDenied { host, .. }
            | Self::TooMany { host }
            | Self::TooBig { host, .. }
            | Self::NotTrusted { host } => host,
            Self::RawAddress { .. } | Self::WrongName { .. } => return None,
        };
        let body = self.sentence();
        Some(
            format!(
                "HTTP/1.1 403 Forbidden\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\
                 X-Synod-Blocked: {host}\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n\
                 {body}",
                body.len()
            )
            .into_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain-English register every card sentence is checked against.
    /// It lives beside the text it guards, so a card cannot acquire our
    /// vocabulary without the build saying so.
    const REFUSAL_JARGON: &[&str] = &[
        "VM", "sandbox", "capability", "manifest", "card", "desk", "transport",
    ];

    /// Trap: this is a lowercase *substring* test, and `"card"` is a
    /// substring of both "standard" and "discard" — a sentence that uses
    /// either word trips this guard for the wrong reason. Keep refusal
    /// prose clear of both.
    fn assert_plain_english(message: &str) {
        for jargon in REFUSAL_JARGON {
            assert!(
                !message.to_lowercase().contains(&jargon.to_lowercase()),
                "refusal must not say '{jargon}', got: {message}"
            );
        }
    }

    #[test]
    fn every_refusal_reads_as_a_plain_english_card() {
        for refusal in [
            Refusal::RawAddress {
                addr: "203.0.113.1".parse().unwrap(),
            },
            Refusal::NotOnList {
                host: "example.com".into(),
            },
            Refusal::WrongName {
                requested: "a.example".into(),
                issued_for: "b.example".into(),
            },
            Refusal::VerbDenied {
                method: "POST".into(),
                host: "example.com".into(),
            },
            Refusal::TooMany {
                host: "example.com".into(),
            },
            Refusal::TooBig {
                host: "example.com".into(),
                limit: 1024,
            },
            Refusal::NotTrusted {
                host: "example.com".into(),
            },
        ] {
            // Exhaustive, no wildcard: a variant added to `Refusal` without
            // a line added to this array fails the build right here, not
            // merely a review comment on some other test.
            match &refusal {
                Refusal::RawAddress { .. }
                | Refusal::NotOnList { .. }
                | Refusal::WrongName { .. }
                | Refusal::VerbDenied { .. }
                | Refusal::TooMany { .. }
                | Refusal::TooBig { .. }
                | Refusal::NotTrusted { .. } => {}
            }
            assert_plain_english(&refusal.sentence());
        }
    }

    #[test]
    fn not_on_list_and_verb_denied_speak_with_net_policy_s_one_voice() {
        assert_eq!(
            Refusal::NotOnList {
                host: "example.com".into()
            }
            .sentence(),
            exarch::net_policy::refusal("example.com")
        );
        assert_eq!(
            Refusal::VerbDenied {
                method: "POST".into(),
                host: "example.com".into()
            }
            .sentence(),
            exarch::net_policy::method_refusal("POST", "example.com")
        );
    }

    #[test]
    fn raw_address_and_wrong_name_have_no_page_to_deliver() {
        assert!(
            Refusal::RawAddress {
                addr: "203.0.113.1".parse().unwrap()
            }
            .page()
            .is_none()
        );
        assert!(
            Refusal::WrongName {
                requested: "a.example".into(),
                issued_for: "b.example".into(),
            }
            .page()
            .is_none()
        );
    }

    #[test]
    fn a_deliverable_refusal_s_page_names_the_host_in_the_header() {
        let page = Refusal::NotOnList {
            host: "example.com".into(),
        }
        .page()
        .expect("NotOnList is delivered inside the intercepted session");
        let page = String::from_utf8(page).expect("the 403 page is ASCII");
        assert!(page.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(page.contains("X-Synod-Blocked: example.com\r\n"));
        assert!(page.contains("not on the list"));
    }
}
