//! The IT-owned network policy: a root-owned script naming which hosts this
//! machine's exarch/synod sessions may reach, which HTTP methods each host
//! admits, and the size/rate ceilings on doing so.
//!
//! Read from a fixed system path (`/etc/exarch/net-policy.ral` — no XDG
//! home, no per-user override), evaluated through the shared
//! [`ral_core::builtins::modules::evaluate_source`] path under a
//! [`ral_core::types::Capabilities::deny_all`] grant, exactly as [`crate::config::load`]
//! reads the user's own unusual-provider config — the same vetted shape,
//! applied to a policy that is IT's rather than the user's. Absent, it
//! falls back to a small built-in default (mirrors
//! `crate::policy::base`'s embedded capability scripts).
//!
//! One file for every front-end on this machine: exarch and synod alike
//! ([`crate::fleet::desk::ExarchDesk`]) answer host enquiries off the same
//! allowlist, so it is not keyed by which one is running.

use ral_core::Shell;
use ral_core::types::Value;

/// The one fixed path every front-end reads — no XDG lookup, no
/// per-invocation override: this is IT's ruleset, not the user's.
const POLICY_PATH: &str = "/etc/exarch/net-policy.ral";

const DEFAULT_POLICY_RAL: &str = include_str!("net_policy/default-policy.ral");

/// This module's error-message prefix, passed to `crate::config`'s shared
/// `read_optional_file`/`evaluate_no_authority` helpers.
const LABEL: &str = "network policy";

/// The HTTP methods a policy may name, uppercase only — hostnames are
/// case-insensitive, HTTP methods are not, so a lowercase spelling is
/// refused rather than silently folded.
const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// One allowlist entry: a host (exact hostname or `*.suffix` wildcard — see
/// [`host_matches`]) and the HTTP methods it admits.
#[derive(Debug, Clone)]
pub struct Rule {
    pub host: String,
    pub methods: Vec<String>,
}

/// What this machine's sessions are allowed to reach, and how much of it.
#[derive(Debug)]
pub struct NetPolicy {
    pub allow: Vec<Rule>,
    /// The largest response body one request may read.
    pub max_response_bytes: u64,
    /// How many requests this process's whole tree may make per minute.
    pub rate_per_minute: u32,
    /// Whether this machine's agents may use the model provider's built-in,
    /// server-side web search. A separate verdict from `allow` because that
    /// allowlist cannot constrain a search the provider runs on our behalf —
    /// we never see the URLs it visits, so there is nothing here to check
    /// them against.
    pub search: bool,
}

impl NetPolicy {
    /// Whether `host` is admitted by any rule, method-blind.
    ///
    /// This is the DNS/accept gate: a name resolves, or a connection is
    /// accepted, before any method line exists to consult, so this
    /// predicate and [`Self::allows`] cannot be collapsed into one without
    /// leaving that earlier gate with nothing to check.
    #[must_use]
    pub fn host_allowed(&self, host: &str) -> bool {
        self.allow.iter().any(|r| host_matches(host, &r.host))
    }

    /// Whether `host` admits `method` — the proxy gate, checked once a
    /// request line is in the clear.
    #[must_use]
    pub fn allows(&self, method: &str, host: &str) -> bool {
        self.allow
            .iter()
            .any(|r| host_matches(host, &r.host) && r.methods.iter().any(|m| m == method))
    }
}

/// Load the policy: the file at [`POLICY_PATH`] if present, else the
/// embedded default.
///
/// # Errors
/// Returns `Err` if the file is present but unreadable, if evaluating it
/// raises an error, or if its terminal map fails to decode into a
/// [`NetPolicy`].
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

/// Require a present `Int`-valued field, naming the field on a miss.
fn int_field(value: Option<&Value>, field: &str, display: &str) -> Result<i64, String> {
    match value {
        Some(Value::Int(n)) => Ok(*n),
        Some(other) => Err(format!(
            "network policy {display}: '{field}' must be an Int, got {}",
            other.type_name()
        )),
        None => Err(format!("network policy {display}: missing '{field}'")),
    }
}

/// Decode one `write` row — `[host: Str, methods: [Str]]` — into a [`Rule`],
/// naming the offending row on any shape mismatch.
fn decode_write_row(row: &Value, display: &str) -> Result<Rule, String> {
    let Value::Map(row) = row else {
        return Err(format!(
            "network policy {display}: 'write' rows must be maps \
             [host: Str, methods: [Str]], got {}",
            row.type_name()
        ));
    };
    for key in row.keys() {
        if !matches!(key.as_str(), "host" | "methods") {
            return Err(format!(
                "network policy {display}: unknown key '{key}' in a 'write' row — expected \
                 host, methods"
            ));
        }
    }
    let host = match row.get("host") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(format!(
                "network policy {display}: a 'write' row's 'host' must be a Str, got {}",
                other.type_name()
            ));
        }
        None => {
            return Err(format!(
                "network policy {display}: a 'write' row is missing 'host'"
            ));
        }
    };
    let raw_methods = match row.get("methods") {
        Some(Value::List(items)) => items,
        Some(other) => {
            return Err(format!(
                "network policy {display}: '{host}' in 'write' has a 'methods' field that must \
                 be a list of strings, got {}",
                other.type_name()
            ));
        }
        None => {
            return Err(format!(
                "network policy {display}: '{host}' in 'write' is missing 'methods'"
            ));
        }
    };
    if raw_methods.is_empty() {
        return Err(format!(
            "network policy {display}: '{host}' in 'write' has an empty 'methods' list — name \
             at least one method, or drop the row"
        ));
    }
    let methods = raw_methods
        .iter()
        .map(|m| {
            let Value::String(m) = m else {
                return Err(format!(
                    "network policy {display}: '{host}' in 'write' names a method that must be \
                     a Str, got {}",
                    m.type_name()
                ));
            };
            if METHODS.contains(&m.as_str()) {
                Ok(m.clone())
            } else if METHODS.contains(&m.to_ascii_uppercase().as_str()) {
                Err(format!(
                    "network policy {display}: '{host}' names method '{m}' in lowercase — HTTP \
                     methods are case-sensitive and must be written uppercase, unlike hostnames"
                ))
            } else {
                Err(format!(
                    "network policy {display}: '{host}' names unknown method '{m}' — expected \
                     one of GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"
                ))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Rule { host, methods })
}

/// Decode the policy's terminal value — a map
/// `[read: [...], write: [...], max-bytes: ..., rate-per-minute: ..., search: ...]`
/// — into a [`NetPolicy`]. Strict on shape and on keys: a malformed policy
/// is a hard error naming the offending field.
fn decode(value: Value, display: &str) -> Result<NetPolicy, String> {
    let Value::Map(map) = value else {
        return Err(format!(
            "network policy {display}: expected a map \
             [read: [...], write: [...], max-bytes: ..., rate-per-minute: ..., search: ...], \
             got {}",
            value.type_name()
        ));
    };
    for key in map.keys() {
        if !matches!(
            key.as_str(),
            "read" | "write" | "max-bytes" | "rate-per-minute" | "search"
        ) {
            return Err(format!(
                "network policy {display}: unknown key '{key}' — expected \
                 read, write, max-bytes, rate-per-minute, search"
            ));
        }
    }
    let read = match map.get("read") {
        Some(Value::List(items)) => items,
        Some(other) => {
            return Err(format!(
                "network policy {display}: 'read' must be a list of strings, got {}",
                other.type_name()
            ));
        }
        None => return Err(format!("network policy {display}: missing 'read'")),
    };
    let write = match map.get("write") {
        Some(Value::List(items)) => items,
        Some(other) => {
            return Err(format!(
                "network policy {display}: 'write' must be a list of \
                 [host: Str, methods: [Str]] rows, got {}",
                other.type_name()
            ));
        }
        None => return Err(format!("network policy {display}: missing 'write'")),
    };
    let mut allow = Vec::with_capacity(read.len() + write.len());
    let mut seen = std::collections::HashSet::new();
    for v in read {
        let host = match v {
            Value::String(s) => s.clone(),
            other => {
                return Err(format!(
                    "network policy {display}: 'read' entries must be strings, got {}",
                    other.type_name()
                ));
            }
        };
        if !seen.insert(host.to_ascii_lowercase()) {
            return Err(format!(
                "network policy {display}: '{host}' is named twice across 'read' and 'write'"
            ));
        }
        allow.push(Rule {
            host,
            methods: vec!["GET".to_string(), "HEAD".to_string()],
        });
    }
    for row in write {
        let rule = decode_write_row(row, display)?;
        if !seen.insert(rule.host.to_ascii_lowercase()) {
            return Err(format!(
                "network policy {display}: '{}' is named twice across 'read' and 'write'",
                rule.host
            ));
        }
        allow.push(rule);
    }
    let max_bytes = int_field(map.get("max-bytes"), "max-bytes", display)?;
    let max_response_bytes = u64::try_from(max_bytes).map_err(|_| {
        format!("network policy {display}: 'max-bytes' must be a non-negative Int, got {max_bytes}")
    })?;
    let rate = int_field(map.get("rate-per-minute"), "rate-per-minute", display)?;
    let rate_per_minute = u32::try_from(rate).map_err(|_| {
        format!(
            "network policy {display}: 'rate-per-minute' must be a non-negative Int that fits \
             in 32 bits, got {rate}"
        )
    })?;
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
    Ok(NetPolicy {
        allow,
        max_response_bytes,
        rate_per_minute,
        search,
    })
}

/// Whether `host` matches allowlist `pattern` — an exact hostname, or a
/// `*.suffix` wildcard matching `host` itself or any of its subdomains.
/// Nothing richer (no regex): the allowlist stays enumerable and
/// reviewable, the same rationale `synod::grant`'s exec map is kept a flat
/// enumeration rather than a pattern language. Case-insensitive, since
/// hostnames are.
fn host_matches(host: &str, pattern: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    match pattern.strip_prefix("*.") {
        Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
        None => host == pattern,
    }
}

/// The refusal a `host` that admits no rule at all gets — the DNS/accept
/// gate's message.
#[must_use]
pub fn refusal(host: &str) -> String {
    format!(
        "'{host}' is not on the list of sites your IT department has approved for this \
         assistant to reach — ask your IT department to add it if you need this site"
    )
}

/// The refusal a `host` that is on the list, but not for `method`, gets —
/// the proxy gate's message.
#[must_use]
pub fn method_refusal(method: &str, host: &str) -> String {
    format!(
        "'{host}' is on the list your IT department has approved, but not for {method} — ask \
         your IT department to widen it if this assistant needs to send that there"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluate a policy `source` string the way [`load`] does — through the
    /// shared core under the no-authority grant, in a fresh throwaway shell —
    /// and decode it. Mirrors `config.rs`'s own test-only `parse` helper.
    fn parse(source: &str) -> Result<NetPolicy, String> {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let source = ral_core::source::normalize_source_text(source.to_string());
        decode(
            crate::config::evaluate_no_authority(&mut shell, &source, "<test:net-policy>", LABEL)?,
            "<test:net-policy>",
        )
    }

    /// A minimal well-formed policy, `read` set to `hosts` and `write` empty
    /// — the shape most of the migrated tests only need one axis of.
    fn read_only(hosts: &[&str]) -> String {
        let hosts = hosts
            .iter()
            .map(|h| format!("'{h}'"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "return [read: [{hosts}], write: [], max-bytes: 1, rate-per-minute: 1, search: false]"
        )
    }

    #[test]
    fn a_read_entry_matches_an_exact_hostname() {
        let policy = parse(&read_only(&["example.com"])).expect("well-formed policy");
        assert!(policy.host_allowed("example.com"));
        assert!(!policy.host_allowed("evil.example.com"));
        assert!(!policy.host_allowed("other.com"));
    }

    #[test]
    fn a_read_entry_matches_a_wildcard_suffix() {
        let policy = parse(&read_only(&["*.bristol.ac.uk"])).expect("well-formed policy");
        assert!(policy.host_allowed("bristol.ac.uk"));
        assert!(policy.host_allowed("cs.bristol.ac.uk"));
        assert!(!policy.host_allowed("bristol.ac.uk.evil.com"));
        assert!(!policy.host_allowed("otherbristol.ac.uk"));
    }

    #[test]
    fn an_empty_read_list_admits_nothing() {
        let policy = parse(&read_only(&[])).expect("well-formed policy");
        assert!(!policy.host_allowed("example.com"));
    }

    #[test]
    fn a_read_entry_is_case_insensitive() {
        let policy = parse(&read_only(&["*.Bristol.AC.UK"])).expect("well-formed policy");
        assert!(policy.host_allowed("CS.BRISTOL.AC.UK"));
    }

    #[test]
    fn the_embedded_default_policy_parses() {
        let policy = parse(DEFAULT_POLICY_RAL).expect("the built-in default must parse");
        assert!(
            policy.host_allowed("archive.ubuntu.com"),
            "an unmanaged install must be useful on day one"
        );
        assert!(policy.max_response_bytes > 0);
        assert!(policy.rate_per_minute > 0);
    }

    /// `load()` itself, exercised end to end: with no real
    /// `/etc/exarch/net-policy.ral` on the test machine, it falls back to
    /// the same embedded default the test above parses directly.
    #[test]
    fn load_falls_back_to_the_embedded_default_when_no_file_is_installed() {
        assert!(
            !std::path::Path::new(POLICY_PATH).exists(),
            "this test assumes no real IT policy file is installed on the test machine"
        );
        let policy = load().expect("load() must fall back to the embedded default");
        assert!(!policy.allow.is_empty());
        assert_eq!(policy.max_response_bytes, 268_435_456);
    }

    #[test]
    fn a_malformed_policy_is_a_hard_error_naming_the_offending_field() {
        let err = parse("return [read: [], write: [], max-bytes: 1]").unwrap_err();
        assert!(err.contains("missing 'rate-per-minute'"), "got: {err}");
    }

    #[test]
    fn an_unknown_key_is_a_hard_error_naming_it() {
        let err = parse(
            "return [read: [], write: [], max-bytes: 1, rate-per-minute: 1, search: true, \
             extra: 'x']",
        )
        .unwrap_err();
        assert!(err.contains("unknown key 'extra'"), "got: {err}");
    }

    #[test]
    fn a_negative_max_bytes_is_a_hard_error() {
        let err =
            parse("return [read: [], write: [], max-bytes: -1, rate-per-minute: 1, search: true]")
                .unwrap_err();
        assert!(
            err.contains("'max-bytes' must be a non-negative Int"),
            "got: {err}"
        );
    }

    #[test]
    fn a_non_string_read_entry_is_a_hard_error() {
        let err =
            parse("return [read: [7], write: [], max-bytes: 1, rate-per-minute: 1, search: true]")
                .unwrap_err();
        assert!(err.contains("'read' entries must be strings"), "got: {err}");
    }

    #[test]
    fn search_true_round_trips() {
        let policy = parse(
            "return [read: ['a'], write: [], max-bytes: 1, rate-per-minute: 1, search: true]",
        )
        .expect("a well-formed policy with search: true must parse");
        assert!(policy.search);
    }

    #[test]
    fn search_false_round_trips() {
        let policy = parse(
            "return [read: ['a'], write: [], max-bytes: 1, rate-per-minute: 1, search: false]",
        )
        .expect("a well-formed policy with search: false must parse");
        assert!(!policy.search);
    }

    // -- one test per grammar rule --------------------------------------

    #[test]
    fn a_non_list_read_is_a_hard_error() {
        let err = parse(
            "return [read: 'not-a-list', write: [], max-bytes: 1, rate-per-minute: 1, \
             search: true]",
        )
        .unwrap_err();
        assert!(
            err.contains("'read' must be a list of strings"),
            "got: {err}"
        );
    }

    #[test]
    fn a_malformed_write_row_is_a_hard_error() {
        let err = parse(
            "return [read: [], write: ['not-a-row'], max-bytes: 1, rate-per-minute: 1, \
             search: true]",
        )
        .unwrap_err();
        assert!(err.contains("'write' rows must be maps"), "got: {err}");
    }

    #[test]
    fn an_unknown_method_is_a_hard_error_naming_it() {
        let err = parse(
            "return [read: [], write: [[host: 'a.example', methods: ['TRACE']]], max-bytes: 1, \
             rate-per-minute: 1, search: true]",
        )
        .unwrap_err();
        assert!(err.contains("unknown method 'TRACE'"), "got: {err}");
    }

    #[test]
    fn a_lowercase_method_is_a_hard_error_naming_it() {
        let err = parse(
            "return [read: [], write: [[host: 'a.example', methods: ['post']]], max-bytes: 1, \
             rate-per-minute: 1, search: true]",
        )
        .unwrap_err();
        assert!(err.contains("in lowercase"), "got: {err}");
    }

    #[test]
    fn an_empty_methods_list_is_a_hard_error() {
        let err = parse(
            "return [read: [], write: [[host: 'a.example', methods: []]], max-bytes: 1, \
             rate-per-minute: 1, search: true]",
        )
        .unwrap_err();
        assert!(err.contains("empty 'methods' list"), "got: {err}");
    }

    #[test]
    fn a_host_named_twice_across_read_and_write_is_a_hard_error() {
        let err = parse(
            "return [read: ['a.example'], write: [[host: 'a.example', methods: ['POST']]], \
             max-bytes: 1, rate-per-minute: 1, search: true]",
        )
        .unwrap_err();
        assert!(err.contains("named twice"), "got: {err}");
    }

    #[test]
    fn write_grants_exactly_the_named_methods() {
        let policy = parse(
            "return [read: [], write: [[host: 'a.example', methods: ['POST', 'PUT']]], \
             max-bytes: 1, rate-per-minute: 1, search: true]",
        )
        .expect("well-formed policy");
        assert!(policy.allows("POST", "a.example"));
        assert!(policy.allows("PUT", "a.example"));
        assert!(
            !policy.allows("GET", "a.example"),
            "write grants only what it names"
        );
        assert!(!policy.allows("DELETE", "a.example"));
    }

    #[test]
    fn nothing_on_the_shipped_default_accepts_a_write() {
        let policy = parse(DEFAULT_POLICY_RAL).expect("the built-in default must parse");
        assert!(
            policy.allow.iter().all(|r| r.methods == ["GET", "HEAD"]),
            "the shipped default is read-only: every rule must be a read entry"
        );
    }
}
