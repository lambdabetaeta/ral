//! The IT-owned network policy: which public DNS names a guest's CONNECT
//! tunnel may reach, on port 443. `guest-net` enforces it; this module only
//! says what it is.
//!
//! A destination allowlist, not an information-flow policy: an admitted host
//! receives whatever the guest sends once the tunnel is open. Read from a
//! fixed system path, never a per-user one, and evaluated under the same
//! no-authority grant as [`crate::config::load`].

use ral_core::Shell;
use ral_core::types::Value;

/// A system path, deliberately: the sandboxed guest writes only cwd and scratch.
const POLICY_PATH: &str = "/etc/exarch/net-policy.ral";

const DEFAULT_POLICY_RAL: &str = include_str!("net_policy/default-policy.ral");

/// Error-message prefix, handed to `crate::config`'s shared load helpers.
const LABEL: &str = "network policy";

/// One exact, lowercase ASCII DNS name — the only shape this policy admits.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Host(String);

impl Host {
    /// The only constructor, hence the lowercase invariant. `guest-net` runs
    /// the CONNECT target through here, so `raw` is bytes an adversary chose;
    /// wildcards and IP literals are refused rather than matched.
    ///
    /// # Errors
    /// Returns `Err` naming what was wrong with `raw`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("a host name must not be empty".to_string());
        }
        if !raw.is_ascii() {
            return Err(format!("'{raw}' is not ASCII"));
        }
        if raw.starts_with('[') || raw.parse::<std::net::IpAddr>().is_ok() {
            return Err(format!("'{raw}' is an IP literal, not a host name"));
        }
        if raw.contains([':', '/', '@']) || raw.contains(char::is_whitespace) {
            return Err(format!(
                "'{raw}' contains a character that has no place in a bare host name"
            ));
        }
        if raw.bytes().any(|b| b.is_ascii_control()) {
            return Err(format!("'{raw}' contains a control byte"));
        }
        if raw.contains('*') {
            return Err(format!("'{raw}' is a wildcard, which this policy refuses"));
        }
        if raw.ends_with('.') {
            return Err(format!(
                "'{raw}' has a trailing dot, which this policy refuses"
            ));
        }
        let lower = raw.to_ascii_lowercase();
        if lower.len() > 253 {
            return Err(format!("'{raw}' is over 253 bytes long"));
        }
        for label in lower.split('.') {
            if label.is_empty() {
                return Err(format!("'{raw}' has an empty label"));
            }
            if label.len() > 63 {
                return Err(format!("'{raw}' has a label over 63 bytes long"));
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(format!("'{raw}' has a label starting or ending with '-'"));
            }
            if label.contains('_') {
                return Err(format!("'{raw}' has a label with an underscore"));
            }
            if !label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(format!(
                    "'{raw}' has a label with a character outside [a-z0-9-]"
                ));
            }
        }
        Ok(Self(lower))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What this machine's sessions may open a CONNECT tunnel to.
#[derive(Debug)]
pub struct NetPolicy {
    pub hosts: std::collections::HashSet<Host>,
    /// The provider's own server-side web search, a separate verdict: we
    /// never see the URLs it visits, so `hosts` cannot constrain it.
    pub search: bool,
}

impl NetPolicy {
    #[must_use]
    pub fn admits(&self, host: &Host) -> bool {
        self.hosts.contains(host)
    }
}

/// Load the policy: the file at [`POLICY_PATH`] if present, else the
/// embedded default.
///
/// # Errors
/// Returns `Err` if the file is present but unreadable, if evaluating it
/// raises, or if its terminal map fails to decode.
pub fn load() -> Result<NetPolicy, String> {
    let path = std::path::Path::new(POLICY_PATH);
    let (source, display) = match crate::config::read_optional_file(path, LABEL)? {
        Some(text) => (text, path.to_string_lossy().into_owned()),
        None => (
            DEFAULT_POLICY_RAL.to_string(),
            "<built-in:net-policy>".to_string(),
        ),
    };
    let source = ral_core::source::normalize_source_text(source);
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    decode(
        crate::config::evaluate_no_authority(&mut shell, &source, &display, LABEL)?,
        &display,
    )
}

/// Decode the policy's terminal map into a [`NetPolicy`]. An unrecognised key
/// is a hard error naming itself, never ignored — a policy that half-parses
/// binds nobody.
fn decode(value: Value, display: &str) -> Result<NetPolicy, String> {
    let Value::Map(map) = value else {
        return Err(format!(
            "network policy {display}: expected a map [hosts: [...], search: ...], got {}",
            value.type_name()
        ));
    };
    for key in map.keys() {
        match key.as_str() {
            "hosts" | "search" => {}
            "read" | "write" => {
                return Err(format!(
                    "network policy {display}: '{key}' is retired — name every admitted host \
                     in 'hosts'; this gate no longer judges HTTP methods"
                ));
            }
            "max-bytes" => {
                return Err(format!(
                    "network policy {display}: 'max-bytes' is retired — this gate no longer \
                     bounds response size"
                ));
            }
            "rate-per-minute" => {
                return Err(format!(
                    "network policy {display}: 'rate-per-minute' is retired — this gate no \
                     longer meters requests"
                ));
            }
            _ => {
                return Err(format!(
                    "network policy {display}: unknown key '{key}' — expected hosts, search"
                ));
            }
        }
    }
    let raw_hosts = match map.get("hosts") {
        Some(Value::List(items)) => items,
        Some(other) => {
            return Err(format!(
                "network policy {display}: 'hosts' must be a list of strings, got {}",
                other.type_name()
            ));
        }
        None => return Err(format!("network policy {display}: missing 'hosts'")),
    };
    let mut hosts = std::collections::HashSet::with_capacity(raw_hosts.len());
    for v in raw_hosts {
        let Value::String(raw) = v else {
            return Err(format!(
                "network policy {display}: 'hosts' entries must be strings, got {}",
                v.type_name()
            ));
        };
        let host = Host::parse(raw).map_err(|e| format!("network policy {display}: {e}"))?;
        if !hosts.insert(host) {
            return Err(format!(
                "network policy {display}: '{raw}' is named twice in 'hosts' after lowercasing"
            ));
        }
    }
    let search = match map.get("search") {
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            return Err(format!(
                "network policy {display}: 'search' must be a Bool, got {}",
                other.type_name()
            ));
        }
        None => return Err(format!("network policy {display}: missing 'search'")),
    };
    Ok(NetPolicy { hosts, search })
}

/// The refusal a `host` outside the allowlist gets.
#[must_use]
pub fn refusal(host: &str) -> String {
    format!(
        "'{host}' is not on the list of sites your IT department has approved for this \
         assistant to reach — ask your IT department to add it if you need this site"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluate and decode a policy the way [`load`] does; `config.rs` mirrors this.
    fn parse(source: &str) -> Result<NetPolicy, String> {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let source = ral_core::source::normalize_source_text(source.to_string());
        decode(
            crate::config::evaluate_no_authority(&mut shell, &source, "<test:net-policy>", LABEL)?,
            "<test:net-policy>",
        )
    }

    fn hosts_policy(hosts: &[&str]) -> String {
        let hosts = hosts
            .iter()
            .map(|h| format!("'{h}'"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("return [hosts: [{hosts}], search: true]")
    }

    #[test]
    fn an_exact_host_admits_itself_and_nothing_else() {
        let policy = parse(&hosts_policy(&["example.com"])).expect("well-formed policy");
        assert!(policy.admits(&Host::parse("example.com").unwrap()));
        assert!(!policy.admits(&Host::parse("evil.example.com").unwrap()));
    }

    #[test]
    fn admits_is_false_for_an_unlisted_host() {
        let policy = parse(&hosts_policy(&["example.com"])).expect("well-formed policy");
        assert!(!policy.admits(&Host::parse("other.com").unwrap()));
    }

    #[test]
    fn case_normalization_makes_pypi_org_and_pypi_org_uppercase_the_same_host() {
        let policy = parse(&hosts_policy(&["PyPI.ORG"])).expect("well-formed policy");
        assert!(policy.admits(&Host::parse("pypi.org").unwrap()));
        assert_eq!(
            Host::parse("PyPI.ORG").unwrap(),
            Host::parse("pypi.org").unwrap()
        );
    }

    #[test]
    fn a_duplicate_after_lowercasing_is_a_hard_error() {
        let err = parse(&hosts_policy(&["pypi.org", "PyPI.org"])).unwrap_err();
        assert!(err.contains("named twice"), "got: {err}");
    }

    #[test]
    fn a_wildcard_is_refused() {
        let err = Host::parse("*.example.com").unwrap_err();
        assert!(err.contains("wildcard"), "got: {err}");
    }

    #[test]
    fn an_ipv4_literal_is_refused() {
        let err = Host::parse("192.0.2.1").unwrap_err();
        assert!(err.contains("IP literal"), "got: {err}");
    }

    #[test]
    fn a_bracketed_ipv6_literal_is_refused() {
        let err = Host::parse("[::1]").unwrap_err();
        assert!(err.contains("IP literal"), "got: {err}");
    }

    #[test]
    fn a_trailing_dot_is_refused() {
        let err = Host::parse("example.com.").unwrap_err();
        assert!(err.contains("trailing dot"), "got: {err}");
    }

    #[test]
    fn an_underscore_is_refused() {
        let err = Host::parse("_dmarc.example.com").unwrap_err();
        assert!(err.contains("underscore"), "got: {err}");
    }

    #[test]
    fn an_empty_label_is_refused() {
        let err = Host::parse("example..com").unwrap_err();
        assert!(err.contains("empty label"), "got: {err}");
    }

    #[test]
    fn a_non_ascii_host_is_refused() {
        let err = Host::parse("café.com").unwrap_err();
        assert!(err.contains("ASCII"), "got: {err}");
    }

    #[test]
    fn the_read_key_names_hosts_as_its_replacement() {
        let err = parse("return [read: [], write: [], search: true]").unwrap_err();
        assert!(
            err.contains("'read' is retired") && err.contains("hosts"),
            "got: {err}"
        );
    }

    #[test]
    fn the_write_key_names_hosts_as_its_replacement() {
        let err = parse("return [hosts: [], write: [], search: true]").unwrap_err();
        assert!(
            err.contains("'write' is retired") && err.contains("hosts"),
            "got: {err}"
        );
    }

    #[test]
    fn the_max_bytes_key_names_its_own_retirement() {
        let err = parse("return [hosts: [], max-bytes: 1, search: true]").unwrap_err();
        assert!(err.contains("'max-bytes' is retired"), "got: {err}");
    }

    #[test]
    fn the_rate_per_minute_key_names_its_own_retirement() {
        let err = parse("return [hosts: [], rate-per-minute: 1, search: true]").unwrap_err();
        assert!(err.contains("'rate-per-minute' is retired"), "got: {err}");
    }

    #[test]
    fn an_unknown_key_is_a_hard_error_naming_it() {
        let err = parse("return [hosts: [], search: true, extra: 'x']").unwrap_err();
        assert!(err.contains("unknown key 'extra'"), "got: {err}");
    }

    #[test]
    fn the_built_in_policy_parses_and_admits_ports_ubuntu_com() {
        let policy = parse(DEFAULT_POLICY_RAL).expect("the built-in default must parse");
        assert!(policy.admits(&Host::parse("ports.ubuntu.com").unwrap()));
        assert!(policy.search);
    }

    #[test]
    fn search_true_round_trips() {
        let policy = parse("return [hosts: ['a.example'], search: true]")
            .expect("a well-formed policy with search: true must parse");
        assert!(policy.search);
    }

    #[test]
    fn search_false_round_trips() {
        let policy = parse("return [hosts: ['a.example'], search: false]")
            .expect("a well-formed policy with search: false must parse");
        assert!(!policy.search);
    }

    #[test]
    fn a_missing_hosts_key_is_a_hard_error() {
        let err = parse("return [search: true]").unwrap_err();
        assert!(err.contains("missing 'hosts'"), "got: {err}");
    }
}
