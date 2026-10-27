//! Pure data and judgments for a guest's CONNECT destination policy.

/// One exact, lowercase ASCII DNS name — the only shape this policy admits.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Host(String);

impl Host {
    /// The only constructor, so every Host is lowercase; refuses wildcards
    /// and IP literals (CONNECT targets are adversarial).
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

    /// A policy admitting exactly `hosts`, with provider search enabled.
    ///
    /// # Panics
    /// Panics if a fixture host is invalid.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(hosts: &[&str]) -> Self {
        Self {
            hosts: hosts
                .iter()
                .map(|host| Host::parse(host).expect("valid fixture host"))
                .collect(),
            search: true,
        }
    }
}

/// The refusal a `host` outside the allowlist gets.
#[must_use]
pub fn refusal(host: &str) -> String {
    format!(
        "'{host}' is not on the list of sites this assistant is allowed to reach: \
         ask whoever administers this computer to add it if you need this site"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_host_admits_itself_and_nothing_else() {
        let policy = NetPolicy::for_test(&["example.com"]);
        assert!(policy.admits(&Host::parse("example.com").unwrap()));
        assert!(!policy.admits(&Host::parse("evil.example.com").unwrap()));
    }

    #[test]
    fn case_normalization_makes_pypi_org_and_pypi_org_uppercase_the_same_host() {
        assert_eq!(
            Host::parse("PyPI.ORG").unwrap(),
            Host::parse("pypi.org").unwrap()
        );
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
}
