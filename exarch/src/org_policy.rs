//! The IT-owned `fetch-url` policy: a root-owned script naming which
//! domains this machine's exarch/synod sessions may fetch from, and the
//! size/rate ceilings on doing so.
//!
//! Read from a fixed system path (`/etc/exarch/fetch-policy.ral` — no XDG
//! home, no per-user override), evaluated through the shared
//! [`ral_core::builtins::modules::evaluate_source`] path under a
//! [`ral_core::types::Capabilities::deny_all`] grant, exactly as [`crate::config::load`]
//! reads the user's own unusual-provider config — the same vetted shape,
//! applied to a policy that is IT's rather than the user's. Absent, it
//! falls back to a small built-in default (mirrors
//! `crate::policy::base`'s embedded capability scripts).
//!
//! One file for every front-end on this machine: `` `fetch-url ``'s host
//! answering function is shared by exarch and synod alike
//! ([`crate::fleet::desk::ExarchDesk`]), so the allowlist is not keyed by
//! which one is running.

use ral_core::Shell;
use ral_core::types::Value;

/// The one fixed path every front-end reads — no XDG lookup, no
/// per-invocation override: this is IT's ruleset, not the user's.
const POLICY_PATH: &str = "/etc/exarch/fetch-policy.ral";

const DEFAULT_POLICY_RAL: &str = include_str!("org_policy/default-policy.ral");

/// This module's error-message prefix, passed to `crate::config`'s shared
/// `read_optional_file`/`evaluate_no_authority` helpers.
const LABEL: &str = "fetch-url policy";

/// What `fetch-url` is allowed to reach, and how much of it.
#[derive(Debug)]
pub struct OrgPolicy {
    /// Hostnames or `*.suffix` wildcards `fetch-url` may reach —
    /// see [`domain_allowed`].
    pub allow: Vec<String>,
    /// The largest response body one `fetch-url` call may read.
    pub max_response_bytes: u64,
    /// How many `fetch-url` calls this process's whole tree may make per
    /// minute.
    pub rate_per_minute: u32,
    /// Whether this machine's agents may use the model provider's built-in,
    /// server-side web search. A separate verdict from `allow` because that
    /// allowlist cannot constrain a search the provider runs on our behalf —
    /// we never see the URLs it visits, so there is nothing here to check
    /// them against.
    pub search: bool,
}

/// Load the policy: the file at [`POLICY_PATH`] if present, else the
/// embedded default.
///
/// # Errors
/// Returns `Err` if the file is present but unreadable, if evaluating it
/// raises an error, or if its terminal map fails to decode into an
/// [`OrgPolicy`].
pub fn load() -> Result<OrgPolicy, String> {
    let path = std::path::Path::new(POLICY_PATH);
    let (source, display) = match crate::config::read_optional_file(path, LABEL)? {
        Some(text) => (text, path.to_string_lossy().into_owned()),
        None => (
            DEFAULT_POLICY_RAL.to_string(),
            "<built-in:fetch-policy>".to_string(),
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
            "fetch-url policy {display}: '{field}' must be an Int, got {}",
            other.type_name()
        )),
        None => Err(format!("fetch-url policy {display}: missing '{field}'")),
    }
}

/// Decode the policy's terminal value — a map
/// `[allow: [...], max-bytes: ..., rate-per-minute: ..., search: ...]` —
/// into an [`OrgPolicy`]. Strict on shape and on keys: a malformed policy is
/// a hard error naming the offending field.
fn decode(value: Value, display: &str) -> Result<OrgPolicy, String> {
    let Value::Map(map) = value else {
        return Err(format!(
            "fetch-url policy {display}: expected a map \
             [allow: [...], max-bytes: ..., rate-per-minute: ..., search: ...], got {}",
            value.type_name()
        ));
    };
    for key in map.keys() {
        if !matches!(
            key.as_str(),
            "allow" | "max-bytes" | "rate-per-minute" | "search"
        ) {
            return Err(format!(
                "fetch-url policy {display}: unknown key '{key}' — expected \
                 allow, max-bytes, rate-per-minute, search"
            ));
        }
    }
    let allow = match map.get("allow") {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(s.clone()),
                other => Err(format!(
                    "fetch-url policy {display}: 'allow' entries must be strings, got {}",
                    other.type_name()
                )),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(other) => {
            return Err(format!(
                "fetch-url policy {display}: 'allow' must be a list of strings, got {}",
                other.type_name()
            ));
        }
        None => return Err(format!("fetch-url policy {display}: missing 'allow'")),
    };
    let max_bytes = int_field(map.get("max-bytes"), "max-bytes", display)?;
    let max_response_bytes = u64::try_from(max_bytes).map_err(|_| {
        format!(
            "fetch-url policy {display}: 'max-bytes' must be a non-negative Int, got {max_bytes}"
        )
    })?;
    let rate = int_field(map.get("rate-per-minute"), "rate-per-minute", display)?;
    let rate_per_minute = u32::try_from(rate).map_err(|_| {
        format!(
            "fetch-url policy {display}: 'rate-per-minute' must be a non-negative Int that fits \
             in 32 bits, got {rate}"
        )
    })?;
    let search = match map.get("search") {
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            return Err(format!(
                "fetch-url policy {display}: 'search' must be a Bool, got {}",
                other.type_name()
            ));
        }
        None => return Err(format!("fetch-url policy {display}: missing 'search'")),
    };
    Ok(OrgPolicy {
        allow,
        max_response_bytes,
        rate_per_minute,
        search,
    })
}

/// Whether `host` is admitted by `patterns`.
///
/// An exact hostname match, or a `*.suffix` wildcard matching `host` itself
/// or any of its subdomains. Nothing richer (no regex): the allowlist stays
/// enumerable and reviewable, the same rationale `synod::grant`'s exec map
/// is kept a flat enumeration rather than a pattern language.
/// Case-insensitive, since hostnames are.
#[must_use]
pub fn domain_allowed(host: &str, patterns: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    patterns.iter().any(|p| {
        let p = p.to_ascii_lowercase();
        match p.strip_prefix("*.") {
            Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
            None => host == p,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluate a policy `source` string the way [`load`] does — through the
    /// shared core under the no-authority grant, in a fresh throwaway shell —
    /// and decode it. Mirrors `config.rs`'s own test-only `parse` helper.
    fn parse(source: &str) -> Result<OrgPolicy, String> {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let source = ral_core::source::normalize_source_text(source.to_string());
        decode(
            crate::config::evaluate_no_authority(
                &mut shell,
                &source,
                "<test:fetch-policy>",
                LABEL,
            )?,
            "<test:fetch-policy>",
        )
    }

    #[test]
    fn domain_allowed_matches_an_exact_hostname() {
        let patterns = vec!["example.com".to_string()];
        assert!(domain_allowed("example.com", &patterns));
        assert!(!domain_allowed("evil.example.com", &patterns));
        assert!(!domain_allowed("other.com", &patterns));
    }

    #[test]
    fn domain_allowed_matches_a_wildcard_suffix() {
        let patterns = vec!["*.bristol.ac.uk".to_string()];
        assert!(domain_allowed("bristol.ac.uk", &patterns));
        assert!(domain_allowed("cs.bristol.ac.uk", &patterns));
        assert!(!domain_allowed("bristol.ac.uk.evil.com", &patterns));
        assert!(!domain_allowed("otherbristol.ac.uk", &patterns));
    }

    #[test]
    fn domain_allowed_refuses_everything_with_an_empty_allowlist() {
        assert!(!domain_allowed("example.com", &[]));
    }

    #[test]
    fn domain_allowed_is_case_insensitive() {
        let patterns = vec!["*.Bristol.AC.UK".to_string()];
        assert!(domain_allowed("CS.BRISTOL.AC.UK", &patterns));
    }

    #[test]
    fn the_embedded_default_policy_parses() {
        let policy = parse(DEFAULT_POLICY_RAL).expect("the built-in default must parse");
        assert!(!policy.allow.is_empty());
        assert!(policy.max_response_bytes > 0);
        assert!(policy.rate_per_minute > 0);
    }

    /// `load()` itself, exercised end to end: with no real
    /// `/etc/exarch/fetch-policy.ral` on the test machine, it falls back to
    /// the same embedded default the test above parses directly.
    #[test]
    fn load_falls_back_to_the_embedded_default_when_no_file_is_installed() {
        assert!(
            !std::path::Path::new(POLICY_PATH).exists(),
            "this test assumes no real IT policy file is installed on the test machine"
        );
        let policy = load().expect("load() must fall back to the embedded default");
        assert!(!policy.allow.is_empty());
    }

    #[test]
    fn a_malformed_policy_is_a_hard_error_naming_the_offending_field() {
        let err = parse("return [allow: ['a'], max-bytes: 1]").unwrap_err();
        assert!(err.contains("missing 'rate-per-minute'"), "got: {err}");
    }

    #[test]
    fn an_unknown_key_is_a_hard_error_naming_it() {
        let err = parse("return [allow: ['a'], max-bytes: 1, rate-per-minute: 1, extra: 'x']")
            .unwrap_err();
        assert!(err.contains("unknown key 'extra'"), "got: {err}");
    }

    #[test]
    fn a_negative_max_bytes_is_a_hard_error() {
        let err = parse("return [allow: ['a'], max-bytes: -1, rate-per-minute: 1]").unwrap_err();
        assert!(
            err.contains("'max-bytes' must be a non-negative Int"),
            "got: {err}"
        );
    }

    #[test]
    fn a_non_string_allow_entry_is_a_hard_error() {
        let err = parse("return [allow: [7], max-bytes: 1, rate-per-minute: 1]").unwrap_err();
        assert!(
            err.contains("'allow' entries must be strings"),
            "got: {err}"
        );
    }

    #[test]
    fn search_true_round_trips() {
        let policy = parse("return [allow: ['a'], max-bytes: 1, rate-per-minute: 1, search: true]")
            .expect("a well-formed policy with search: true must parse");
        assert!(policy.search);
    }

    #[test]
    fn search_false_round_trips() {
        let policy =
            parse("return [allow: ['a'], max-bytes: 1, rate-per-minute: 1, search: false]")
                .expect("a well-formed policy with search: false must parse");
        assert!(!policy.search);
    }
}
