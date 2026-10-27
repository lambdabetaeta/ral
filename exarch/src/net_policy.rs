//! The network policy: which public DNS names a guest's CONNECT
//! tunnel may reach, on port 443. `netgate` enforces it; this module only
//! says what it is.
//!
//! A destination allowlist, not an information-flow policy: an admitted host
//! receives whatever the guest sends once the tunnel is open. Read from a
//! fixed system path, never a per-user one, and evaluated under the same
//! no-authority grant as [`crate::config::load_declared`].

use ral_core::egress::{Host, NetPolicy};
use ral_core::own::UserFile;
use ral_core::shell::runtime::Value;
use ral_core::source::{SourceFile, SourceOrigin};

/// A system place, deliberately: the sandboxed guest writes only cwd and scratch.
const POLICY_DIR: &str = "/etc/exarch";
const POLICY_FILE: &str = "net-policy.ral";

const DEFAULT_POLICY_RAL: &str = include_str!("net_policy/default-policy.ral");

/// Error-message prefix, handed to `crate::config`'s shared load helpers.
const LABEL: &str = "network policy";

/// Load the policy: the file [`POLICY_FILE`] in [`POLICY_DIR`] if present, else
/// the embedded default.
///
/// # Errors
/// Returns `Err` if the file is present but unreadable, if evaluating it
/// raises, or if its terminal map fails to decode.
pub fn load() -> Result<NetPolicy, String> {
    let file = UserFile::fixed(POLICY_DIR, POLICY_FILE);
    let (source, origin) = match crate::config::read_optional_file(&file, LABEL)? {
        Some(text) => (
            text,
            SourceOrigin::File(SourceFile::new(file.path()).map_err(|e| format!("{LABEL} {e}"))?),
        ),
        None => (
            DEFAULT_POLICY_RAL.to_string(),
            SourceOrigin::named("<built-in:net-policy>"),
        ),
    };
    let source = ral_core::source::normalize_source_text(source);
    let cwd = ral_core::host::cwd()
        .ok_or_else(|| format!("{LABEL} cannot load: the working directory is unavailable"))?;
    let mut shell = ral_core::HostSurface::default()
        .shell(ral_core::io::terminal::TerminalState::default(), cwd)
        .map_err(|e| format!("{LABEL} cannot load: {e}"))?;
    decode(
        crate::config::evaluate_no_authority(&mut shell, &source, &origin, LABEL)?,
        &origin.to_string(),
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
        match key {
            "hosts" | "search" => {}
            "read" | "write" => {
                return Err(format!(
                    "network policy {display}: '{key}' is retired; name every admitted host \
                     in 'hosts'; this gate no longer judges HTTP methods"
                ));
            }
            "max-bytes" => {
                return Err(format!(
                    "network policy {display}: 'max-bytes' is retired; this gate no longer \
                     bounds response size"
                ));
            }
            "rate-per-minute" => {
                return Err(format!(
                    "network policy {display}: 'rate-per-minute' is retired; this gate no \
                     longer meters requests"
                ));
            }
            _ => {
                return Err(format!(
                    "network policy {display}: unknown key '{key}'; expected hosts, search"
                ));
            }
        }
    }
    let hosts_field = map.get("hosts");
    let raw_hosts = match hosts_field.as_deref() {
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
        let v = v.into_owned();
        let Value::String(raw) = v else {
            return Err(format!(
                "network policy {display}: 'hosts' entries must be strings, got {}",
                v.type_name()
            ));
        };
        let host = Host::parse(&raw).map_err(|e| format!("network policy {display}: {e}"))?;
        if !hosts.insert(host) {
            return Err(format!(
                "network policy {display}: '{raw}' is named twice in 'hosts' after lowercasing"
            ));
        }
    }
    let search = match map.get("search").as_deref() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluate and decode a policy the way [`load`] does; `config.rs` mirrors this.
    fn parse(source: &str) -> Result<NetPolicy, String> {
        let mut shell = ral_core::test_helper::core_shell();
        let source = ral_core::source::normalize_source_text(source.to_string());
        decode(
            crate::config::evaluate_no_authority(
                &mut shell,
                &source,
                &SourceOrigin::named("<test:net-policy>"),
                LABEL,
            )?,
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
    fn case_normalization_makes_pypi_org_and_pypi_org_uppercase_the_same_host() {
        let policy = parse(&hosts_policy(&["PyPI.ORG"])).expect("well-formed policy");
        assert!(policy.admits(&Host::parse("pypi.org").unwrap()));
    }

    #[test]
    fn a_duplicate_after_lowercasing_is_a_hard_error() {
        let err = parse(&hosts_policy(&["pypi.org", "PyPI.org"])).unwrap_err();
        assert!(err.contains("named twice"), "got: {err}");
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
