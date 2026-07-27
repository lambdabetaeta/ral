//! The ledger that makes the raw-IP argument hold: a `100.64.0.0/10`
//! address exists at all only because
//! [`exarch::net_policy::NetPolicy::host_allowed`] admitted the name DNS
//! minted it for.
//!
//! `stack`'s accept gate reads this ledger the other way — given a
//! destination address the guest dialled, is there a name behind it at
//! all? An address the guest fabricated out of thin air, or recalls from a
//! previous, unrelated session, was never minted here, so it maps to
//! nothing and is refused before a byte of the connection is read. That is
//! gate two; gate one is [`Names::resolve`] itself, which is the only
//! place an address is ever created.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use exarch::egress::{AuditLog, Record};
use exarch::net_policy::NetPolicy;

/// Base of the `100.64.0.0/10` shared-address-space block every minted name
/// lives in — carrier-grade NAT space, chosen because it is never a real
/// route on either the guest's or the host's own network, so a name's
/// address can never collide with something the guest meant literally.
const POOL_BASE: u32 = 0x6440_0000;

/// The `/10` block holds `2^22` host addresses.
const POOL_LEN: u32 = 1 << 22;

/// A minted address, bound to the lowercased name DNS answered it for.
///
/// One instance per guest session (owned by `stack`'s core thread, so no
/// locking): the mapping is never persisted and never shared with another
/// session, which is exactly why a stale address from a previous session
/// cannot mean anything here.
pub struct Names {
    policy: Arc<NetPolicy>,
    audit: AuditLog,
    /// The interface's own address, excluded from the mintable range so a
    /// name can never collide with the host stack's own identity.
    gateway: Ipv4Addr,
    by_name: HashMap<String, Ipv4Addr>,
    by_addr: HashMap<Ipv4Addr, String>,
    next: u32,
}

impl Names {
    #[must_use]
    pub fn new(policy: Arc<NetPolicy>, audit: AuditLog, gateway: Ipv4Addr) -> Self {
        Self {
            policy,
            audit,
            gateway,
            by_name: HashMap::new(),
            by_addr: HashMap::new(),
            next: 1, // offset 0 is the pool's network address, left unminted
        }
    }

    /// Mint (or recall) the address standing in for `name`, auditing the
    /// decision either way. Returns `None` for a name
    /// [`NetPolicy::host_allowed`] does not admit — DNS's `NXDOMAIN` case —
    /// and never mints an address for one, which is the whole of gate one.
    pub fn resolve(&mut self, name: &str) -> Option<Ipv4Addr> {
        let lower = name.to_ascii_lowercase();
        if !self.policy.host_allowed(&lower) {
            self.audit.record(Record::Name {
                name: &lower,
                allowed: false,
                addr: None,
            });
            return None;
        }
        let addr = if let Some(&addr) = self.by_name.get(&lower) {
            addr
        } else {
            let addr = Self::mint(&mut self.next, &self.by_addr, self.gateway);
            self.by_name.insert(lower.clone(), addr);
            self.by_addr.insert(addr, lower.clone());
            addr
        };
        self.audit.record(Record::Name {
            name: &lower,
            allowed: true,
            addr: Some(addr),
        });
        Some(addr)
    }

    /// The name a minted `addr` stands for, or `None` if DNS never minted
    /// it — gate two's entire check, read at TCP accept.
    #[must_use]
    pub fn name_of(&self, addr: Ipv4Addr) -> Option<&str> {
        self.by_addr.get(&addr).map(String::as_str)
    }

    /// Take the next free address in the pool.
    ///
    /// # Panics
    /// If the pool's four million addresses are ever exhausted. No guest
    /// session names four million distinct hosts in one boot; reaching this
    /// is a runaway process, not a capacity plan to build for.
    fn mint(next: &mut u32, by_addr: &HashMap<Ipv4Addr, String>, gateway: Ipv4Addr) -> Ipv4Addr {
        while *next < POOL_LEN {
            let addr = Ipv4Addr::from(POOL_BASE + *next);
            *next += 1;
            if addr != gateway && !by_addr.contains_key(&addr) {
                return addr;
            }
        }
        panic!("the 100.64.0.0/10 name pool is exhausted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(allow: &[&str]) -> Names {
        let policy = Arc::new(NetPolicy {
            allow: allow
                .iter()
                .map(|h| exarch::net_policy::Rule {
                    host: (*h).to_string(),
                    methods: vec!["GET".to_string()],
                })
                .collect(),
            max_response_bytes: 1,
            rate_per_minute: 1,
            search: false,
        });
        let path = std::env::temp_dir().join(format!(
            "guest-net-names-test-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Names::new(
            policy,
            AuditLog::for_test(&path),
            Ipv4Addr::new(100, 64, 0, 1),
        )
    }

    #[test]
    fn an_off_list_name_mints_nothing() {
        assert!(names(&["a.example"]).resolve("b.example").is_none());
    }

    #[test]
    fn an_on_list_name_mints_an_address_in_the_pool() {
        let mut n = names(&["a.example"]);
        let addr = n.resolve("a.example").expect("on-list name mints");
        assert_eq!(u32::from(addr) & 0xFFC0_0000, POOL_BASE);
    }

    #[test]
    fn resolving_the_same_name_twice_returns_the_same_address() {
        let mut n = names(&["a.example"]);
        let first = n.resolve("a.example").unwrap();
        let second = n.resolve("a.example").unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn distinct_names_mint_distinct_addresses() {
        let mut n = names(&["a.example", "b.example"]);
        let a = n.resolve("a.example").unwrap();
        let b = n.resolve("b.example").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn resolution_is_case_insensitive() {
        let mut n = names(&["a.example"]);
        let lower = n.resolve("a.example").unwrap();
        let upper = n.resolve("A.EXAMPLE").unwrap();
        assert_eq!(lower, upper);
    }

    #[test]
    fn an_address_dns_never_minted_has_no_name() {
        let mut n = names(&["a.example"]);
        let minted = n.resolve("a.example").unwrap();
        assert_eq!(n.name_of(minted), Some("a.example"));
        assert_eq!(n.name_of(Ipv4Addr::new(100, 64, 0, 99)), None);
    }

    /// `names()`'s gateway is `100.64.0.1`, the pool's first mintable
    /// offset — so this also proves [`Names::mint`] steps past it rather
    /// than handing it out.
    #[test]
    fn the_gateway_address_is_never_minted() {
        let mut n = names(&["a.example"]);
        let addr = n.resolve("a.example").unwrap();
        assert_ne!(addr, n.gateway);
    }
}
