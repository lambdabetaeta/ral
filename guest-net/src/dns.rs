//! The stub DNS server the guest's `resolv.conf` points at.
//!
//! This is the only parser in the crate that reads bytes an adversary
//! chose — everything else on the wire (the frame codec, the TCP/IP stack
//! itself) is either shared, well-tested infrastructure or delegates to
//! `smoltcp`. [`Query::parse`] is hand-rolled and must never panic on any
//! input, which is why [`Query::parse`] alone (no [`Names`], no
//! [`RateLimiter`]) is the crate's `cargo-fuzz` target — see
//! `guest-net/fuzz/fuzz_targets/dns.rs` and the corpus test at the bottom
//! of this file.
//!
//! Every answer echoes the query's `ID` and `RD` bit, and the question
//! section verbatim, byte for byte — including whatever case a resolver
//! chose for [0x20 randomisation][rfc], since the only way to prove a
//! reply belongs to a query is to hand the question back exactly as asked.
//! Policy is matched on a lowercased copy; the wire bytes are never
//! rewritten.
//!
//! [rfc]: https://datatracker.ietf.org/doc/html/draft-vixie-dnsext-dns0x20
//!
//! An off-list name gets `NXDOMAIN` for any query type. An on-list name
//! gets a real answer only for `A` (one record, 60s TTL); `AAAA`, `HTTPS`,
//! and everything else get `NOERROR` with zero answers instead. That
//! distinction is load-bearing, not an oversight: `NXDOMAIN` on an `AAAA`
//! tells a stub resolver the *name itself* does not exist, which most
//! resolvers cache negatively for the whole name — including the `A`
//! lookup the same client makes moments later. `NOERROR`/zero-answers says
//! only "this record type is empty here", which is both true (we mint no
//! `AAAA`) and leaves the `A` answer alone.

use std::net::Ipv4Addr;

use exarch::egress::RateLimiter;

use crate::names::Names;

const TYPE_A: u16 = 1;
const TYPE_SOA: u16 = 6;
const TYPE_OPT: u16 = 41;
const CLASS_IN: u16 = 1;

const RCODE_NOERROR: u16 = 0;
const RCODE_NXDOMAIN: u16 = 3;

/// A compression pointer to offset 12 — the first byte after the header,
/// where the question's own name always starts. Every record this module
/// emits names either that question or a handful of fixed literal labels,
/// so this one pointer is all the name compression it ever needs.
const NAME_POINTER: [u8; 2] = [0xC0, 0x0C];

/// A parsed query: the pieces needed to answer it, plus the question's raw
/// wire bytes for verbatim echo.
pub struct Query {
    id: u16,
    /// The query's own flags word, kept whole so the response can echo
    /// `RD` and the opcode without re-deriving them bit by bit.
    flags: u16,
    /// Bytes `12..` of the message, the question section exactly as sent.
    question: Vec<u8>,
    /// The question's name, lowercased and dot-joined, for policy matching
    /// only — never written back to the wire.
    name: String,
    qtype: u16,
    /// Whether the query carried a bare `OPT` pseudo-record signalling
    /// EDNS(0) support, in the one shape a stub resolver actually sends:
    /// no answer or authority records, one additional record, right after
    /// the question.
    edns: bool,
}

impl Query {
    /// Parse a DNS message off the wire. Returns `None` for anything this
    /// module cannot make sense of — a truncated message, more than one
    /// question, a non-`IN` class, a compressed question name (never valid
    /// in a query) — in which case the caller drops the packet rather than
    /// answer a peer that is not speaking DNS.
    ///
    /// Never panics: every access is bounds-checked, since this is the
    /// crate's one parser of bytes an adversary chose.
    #[must_use]
    pub fn parse(msg: &[u8]) -> Option<Self> {
        if msg.len() < 12 {
            return None;
        }
        let id = u16_at(msg, 0)?;
        let flags = u16_at(msg, 2)?;
        let qdcount = u16_at(msg, 4)?;
        let ancount = u16_at(msg, 6)?;
        let nscount = u16_at(msg, 8)?;
        let arcount = u16_at(msg, 10)?;
        if qdcount != 1 {
            return None; // a resolver's query names exactly one question
        }

        let (name, mut pos) = read_name(msg, 12)?;
        let qtype = u16_at(msg, pos)?;
        pos += 2;
        let qclass = u16_at(msg, pos)?;
        pos += 2;
        if qclass != CLASS_IN {
            return None;
        }
        let question = msg.get(12..pos)?.to_vec();

        // Only the shape a stub resolver actually sends is honoured: a bare
        // OPT record, right after the question, with nothing ahead of it.
        // Anything stranger falls back to "no EDNS" rather than risk
        // misreading a crafted packet as one.
        let edns = ancount == 0
            && nscount == 0
            && arcount >= 1
            && msg.get(pos) == Some(&0)
            && u16_at(msg, pos + 1) == Some(TYPE_OPT);

        Some(Self {
            id,
            flags,
            question,
            name,
            qtype,
            edns,
        })
    }

    /// The question's name, lowercased — what [`Names::resolve`] is asked
    /// about.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Build the answer to this query, minting or looking up the name's
    /// address through `names`, and auditing the decision there.
    #[must_use]
    pub fn answer(&self, names: &mut Names) -> Vec<u8> {
        let addr = names.resolve(&self.name);
        let (rcode, a_record, soa) = match addr {
            None => (RCODE_NXDOMAIN, None, true),
            Some(ip) if self.qtype == TYPE_A => (RCODE_NOERROR, Some(ip), false),
            Some(_) => (RCODE_NOERROR, None, true),
        };

        let opcode = (self.flags >> 11) & 0xF;
        let rd = self.flags & 0x0100;
        let resp_flags = 0x8000 // QR: this is a response
            | (opcode << 11)
            | 0x0400 // AA: authoritative — every answer here is minted by us, not fetched from elsewhere
            | rd
            | rcode;

        let mut out = Vec::with_capacity(64 + self.question.len());
        out.extend_from_slice(&self.id.to_be_bytes());
        out.extend_from_slice(&resp_flags.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        out.extend_from_slice(&u16::from(a_record.is_some()).to_be_bytes());
        out.extend_from_slice(&u16::from(soa).to_be_bytes());
        out.extend_from_slice(&u16::from(self.edns).to_be_bytes());
        out.extend_from_slice(&self.question);

        if let Some(ip) = a_record {
            write_a(&mut out, ip);
        }
        if soa {
            write_soa(&mut out);
        }
        if self.edns {
            write_opt(&mut out);
        }
        out
    }
}

fn u16_at(msg: &[u8], pos: usize) -> Option<u16> {
    msg.get(pos..pos + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

/// Read one (uncompressed) domain name starting at `pos`, returning it
/// dot-joined and the offset just past its terminating zero length.
/// Compression pointers are refused — never valid in a question, and this
/// module has no need to follow one.
fn read_name(msg: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    loop {
        let len = *msg.get(pos)?;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None;
        }
        let len = len as usize;
        pos += 1;
        let label = msg.get(pos..pos + len)?;
        pos += len;
        labels.push(String::from_utf8_lossy(label).into_owned());
        if labels.len() > 127 {
            return None; // no real name nests this deep
        }
    }
    Some((labels.join("."), pos))
}

fn write_a(out: &mut Vec<u8>, addr: Ipv4Addr) {
    out.extend_from_slice(&NAME_POINTER);
    out.extend_from_slice(&TYPE_A.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out.extend_from_slice(&60u32.to_be_bytes()); // TTL
    out.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH
    out.extend_from_slice(&addr.octets());
}

/// The negative-answer record: present whenever there is no `A` to give,
/// whether the name is off-list (`NXDOMAIN`) or on-list but not an `A`
/// query (`NOERROR`/zero answers). Its `MINIMUM` field is what a
/// resolver's negative cache actually keys its TTL on, so it is kept equal
/// to the `A` record's own TTL rather than invented separately.
fn write_soa(out: &mut Vec<u8>) {
    out.extend_from_slice(&NAME_POINTER);
    out.extend_from_slice(&TYPE_SOA.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out.extend_from_slice(&60u32.to_be_bytes()); // TTL
    let rdata = soa_rdata();
    out.extend_from_slice(&u16::try_from(rdata.len()).expect("fixed-size rdata").to_be_bytes());
    out.extend_from_slice(&rdata);
}

fn soa_rdata() -> Vec<u8> {
    let mut rdata = encode_name(&["ns", "synod"]);
    rdata.extend(encode_name(&["hostmaster", "synod"]));
    rdata.extend_from_slice(&1u32.to_be_bytes()); // SERIAL
    rdata.extend_from_slice(&3600u32.to_be_bytes()); // REFRESH
    rdata.extend_from_slice(&600u32.to_be_bytes()); // RETRY
    rdata.extend_from_slice(&86_400u32.to_be_bytes()); // EXPIRE
    rdata.extend_from_slice(&60u32.to_be_bytes()); // MINIMUM: the negative-cache TTL
    rdata
}

fn encode_name(labels: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for label in labels {
        out.push(u8::try_from(label.len()).expect("a fixed internal label"));
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// The EDNS(0) pseudo-record: bare, a 1232-byte UDP payload size (the
/// widely-used safe default under common path MTUs), the DO bit cleared
/// (we sign nothing, so claiming DNSSEC support would be a lie), and no
/// options echoed back — this resolver answers everything itself and has
/// no options handshake to offer.
fn write_opt(out: &mut Vec<u8>) {
    out.push(0); // root name
    out.extend_from_slice(&TYPE_OPT.to_be_bytes());
    out.extend_from_slice(&1232u16.to_be_bytes()); // requestor's UDP payload size, repurposing CLASS
    out.extend_from_slice(&0u32.to_be_bytes()); // extended RCODE / VERSION / flags (DO cleared), repurposing TTL
    out.extend_from_slice(&0u16.to_be_bytes()); // RDLENGTH: no options
}

/// Parse and answer one query, or drop it.
///
/// `limiter` is a second [`RateLimiter`] — DNS's own, distinct from the
/// HTTP budget — and a query over cap is dropped silently rather than
/// answered with a refusal: a dropped query just retries, but an answered
/// flood is an amplifier a stranger could point at someone else's address.
pub fn handle(query: &[u8], names: &mut Names, limiter: &RateLimiter) -> Option<Vec<u8>> {
    if !limiter.try_take() {
        return None;
    }
    Query::parse(query).map(|q| q.answer(names))
}

#[cfg(test)]
mod tests {
    use super::*;
    use exarch::net_policy::{NetPolicy, Rule};
    use std::sync::Arc;

    fn wire_name(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for label in name.split('.') {
            out.push(u8::try_from(label.len()).unwrap());
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }

    /// Build a minimal query message: `id`, `RD` set iff `rd`, one question
    /// for `name`/`qtype`, and a bare `OPT` additional record iff `edns`.
    fn build_query(id: u16, rd: bool, name: &str, qtype: u16, edns: bool) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(&id.to_be_bytes());
        let flags: u16 = if rd { 0x0100 } else { 0 };
        msg.extend_from_slice(&flags.to_be_bytes());
        msg.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        msg.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
        msg.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
        msg.extend_from_slice(&u16::from(edns).to_be_bytes()); // ARCOUNT
        msg.extend(wire_name(name));
        msg.extend_from_slice(&qtype.to_be_bytes());
        msg.extend_from_slice(&CLASS_IN.to_be_bytes());
        if edns {
            msg.push(0); // root
            msg.extend_from_slice(&TYPE_OPT.to_be_bytes());
            msg.extend_from_slice(&4096u16.to_be_bytes());
            msg.extend_from_slice(&0u32.to_be_bytes());
            msg.extend_from_slice(&0u16.to_be_bytes());
        }
        msg
    }

    fn names(allow: &[&str]) -> Names {
        let policy = Arc::new(NetPolicy {
            allow: allow
                .iter()
                .map(|h| Rule {
                    host: (*h).to_string(),
                    methods: vec!["GET".to_string()],
                })
                .collect(),
            max_response_bytes: 1,
            rate_per_minute: 1,
            search: false,
        });
        let path = std::env::temp_dir().join(format!(
            "guest-net-dns-test-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Names::new(
            policy,
            exarch::egress::AuditLog::for_test(&path),
            Ipv4Addr::new(100, 64, 0, 1),
        )
    }

    fn header_flags(resp: &[u8]) -> u16 {
        u16_at(resp, 2).unwrap()
    }

    #[test]
    fn id_and_rd_are_echoed() {
        let mut n = names(&["a.example"]);
        let q = build_query(0x1234, true, "a.example", TYPE_A, false);
        let resp = Query::parse(&q).unwrap().answer(&mut n);
        assert_eq!(u16_at(&resp, 0), Some(0x1234));
        assert_eq!(header_flags(&resp) & 0x0100, 0x0100);

        let q2 = build_query(0x5678, false, "a.example", TYPE_A, false);
        let resp2 = Query::parse(&q2).unwrap().answer(&mut n);
        assert_eq!(u16_at(&resp2, 0), Some(0x5678));
        assert_eq!(header_flags(&resp2) & 0x0100, 0);
    }

    /// 0x20 case randomisation: whatever case the question arrived in must
    /// come back byte-identical, even though policy matching lowercases
    /// its own copy.
    #[test]
    fn the_question_is_echoed_verbatim_case_and_all() {
        let mut n = names(&["a.example"]);
        let q = build_query(1, true, "A.eXaMpLe", TYPE_A, false);
        let resp = Query::parse(&q).unwrap().answer(&mut n);
        // The question section starts right after the 12-byte header.
        let question = &q[12..];
        assert_eq!(&resp[12..12 + question.len()], question);
    }

    #[test]
    fn an_off_list_name_gets_nxdomain_and_an_soa() {
        let mut n = names(&["a.example"]);
        let q = build_query(1, true, "off.example", TYPE_A, false);
        let resp = Query::parse(&q).unwrap().answer(&mut n);
        assert_eq!(header_flags(&resp) & 0xF, 3); // NXDOMAIN
        assert_eq!(u16_at(&resp, 6), Some(0)); // ANCOUNT
        assert_eq!(u16_at(&resp, 8), Some(1)); // NSCOUNT: one SOA
    }

    #[test]
    fn an_on_list_a_query_gets_one_answer_with_a_60s_ttl() {
        let mut n = names(&["a.example"]);
        let q = build_query(1, true, "a.example", TYPE_A, false);
        let resp = Query::parse(&q).unwrap().answer(&mut n);
        assert_eq!(header_flags(&resp) & 0xF, 0); // NOERROR
        assert_eq!(u16_at(&resp, 6), Some(1)); // ANCOUNT
        let question_len = q.len() - 12;
        let rr = 12 + question_len;
        // NAME(2) TYPE(2) CLASS(2) TTL(4) RDLENGTH(2) RDATA(4)
        let ttl = u32::from_be_bytes(resp[rr + 6..rr + 10].try_into().unwrap());
        assert_eq!(ttl, 60);
        assert_eq!(u16_at(&resp, rr + 10), Some(4));
    }

    /// `AAAA` on an on-list name must be `NOERROR`/zero answers, never
    /// `NXDOMAIN` — an `NXDOMAIN` here would poison a stub resolver's
    /// negative cache for the whole name, including the following `A`.
    #[test]
    fn an_on_list_aaaa_query_is_noerror_with_zero_answers() {
        let mut n = names(&["a.example"]);
        let q = build_query(1, true, "a.example", 28, false); // AAAA
        let resp = Query::parse(&q).unwrap().answer(&mut n);
        assert_eq!(header_flags(&resp) & 0xF, 0); // NOERROR, not NXDOMAIN
        assert_eq!(u16_at(&resp, 6), Some(0)); // ANCOUNT
        assert_eq!(u16_at(&resp, 8), Some(1)); // NSCOUNT: the SOA
    }

    /// Same for `HTTPS` (type 65) — everything but `A` gets the same
    /// empty-but-not-negative treatment.
    #[test]
    fn an_on_list_https_query_is_noerror_with_zero_answers() {
        let mut n = names(&["a.example"]);
        let q = build_query(1, true, "a.example", 65, false);
        let resp = Query::parse(&q).unwrap().answer(&mut n);
        assert_eq!(header_flags(&resp) & 0xF, 0);
        assert_eq!(u16_at(&resp, 6), Some(0));
    }

    #[test]
    fn edns_is_echoed_as_a_bare_opt_with_do_cleared() {
        let mut n = names(&["a.example"]);
        let q = build_query(1, true, "a.example", TYPE_A, true);
        let resp = Query::parse(&q).unwrap().answer(&mut n);
        assert_eq!(u16_at(&resp, 10), Some(1)); // ARCOUNT
        // The OPT record trails everything else; find it from the end.
        let opt = &resp[resp.len() - 11..];
        assert_eq!(opt[0], 0); // root name
        assert_eq!(u16::from_be_bytes([opt[1], opt[2]]), TYPE_OPT);
        assert_eq!(u16::from_be_bytes([opt[3], opt[4]]), 1232);
        assert_eq!(u32::from_be_bytes(opt[5..9].try_into().unwrap()), 0); // DO cleared, extended RCODE 0
        assert_eq!(u16::from_be_bytes([opt[9], opt[10]]), 0); // no options
    }

    #[test]
    fn no_edns_in_the_query_means_no_opt_in_the_answer() {
        let mut n = names(&["a.example"]);
        let q = build_query(1, true, "a.example", TYPE_A, false);
        let resp = Query::parse(&q).unwrap().answer(&mut n);
        assert_eq!(u16_at(&resp, 10), Some(0)); // ARCOUNT
    }

    #[test]
    fn a_query_over_the_rate_cap_is_dropped_silently() {
        let mut n = names(&["a.example"]);
        let limiter = RateLimiter::with_window(1, std::time::Duration::from_mins(1));
        let q = build_query(1, true, "a.example", TYPE_A, false);
        assert!(handle(&q, &mut n, &limiter).is_some());
        assert!(handle(&q, &mut n, &limiter).is_none());
    }

    #[test]
    fn a_message_too_short_for_a_header_is_refused() {
        assert!(Query::parse(&[0u8; 4]).is_none());
    }

    #[test]
    fn more_than_one_question_is_refused() {
        let mut msg = build_query(1, true, "a.example", TYPE_A, false);
        msg[4..6].copy_from_slice(&2u16.to_be_bytes()); // claim QDCOUNT=2
        assert!(Query::parse(&msg).is_none());
    }

    #[test]
    fn a_compression_pointer_in_the_question_is_refused() {
        let mut msg = build_query(1, true, "a.example", TYPE_A, false);
        msg[12] = 0xC0; // the first label length byte, turned into a pointer tag
        assert!(Query::parse(&msg).is_none());
    }

    #[test]
    fn a_name_claiming_a_label_past_the_buffer_is_refused_not_panicked() {
        let mut msg = build_query(1, true, "a.example", TYPE_A, false);
        msg[12] = 0x3F; // a 63-byte label with nowhere near that much buffer left
        assert!(Query::parse(&msg).is_none());
    }

    /// The seed corpus `cargo-fuzz` would run this target against, replayed
    /// here so the same inputs are exercised by the normal test suite —
    /// this is the crate's one untrusted-input parser, so proving it never
    /// panics is not optional.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "a test-only replay of the crate's own checked-in fuzz corpus, not turn-time model I/O"
    )]
    fn the_fuzz_corpus_never_panics_the_parser() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/dns");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return; // no corpus checked out here yet — nothing to replay
        };
        for entry in entries.flatten() {
            let bytes = std::fs::read(entry.path()).expect("readable corpus file");
            let _ = Query::parse(&bytes);
        }
    }
}
