//! The unusual-provider config (`$XDG_CONFIG_HOME/exarch/config.ral`), plus one
//! environment knob the binary reads at boot.
//!
//! Famous providers auto-populate from the environment
//! ([`crate::provider::credential`]); a self-hosted or non-famous endpoint is the
//! one thing exarch cannot know, so it is declared here. The file is source, not
//! data: it runs through the same
//! [`ral_core::builtins::modules::evaluate_source`] core as any other `.ral` load,
//! and its terminal map decodes into [`CustomProvider`]s.
//!
//! A redirected `endpoint` aims the agent's own traffic at an arbitrary server, so
//! the file is an exfiltration channel to whoever can write it. Hence two defences:
//! it lives at the XDG config home, never the working tree the sandboxed agent can
//! write, and it evaluates under [`Capabilities::deny_all`] — with `exec` denied
//! there is no route to the network, so the in-process gate suffices.

use crate::provider::CustomProvider;
use genai::adapter::AdapterKind;
use ral_core::Shell;
use ral_core::types::{Break, Capabilities, Mooring, Value};

const CONFIG_FILE: &str = "config.ral";

/// Error-message prefix; `net_policy` passes its own to the shared helpers below.
const LABEL: &str = "exarch config";

/// The operator-set disk-warn ceiling in bytes, read once at boot — there is
/// deliberately no live reload. Absent means the log and scratch dirs are never
/// walked at all.
///
/// # Errors
/// A present but unparseable value is a hard error, not a silent disable.
pub fn disk_warn_bytes() -> Result<Option<u64>, String> {
    match std::env::var("EXARCH_DISK_WARN_BYTES") {
        Ok(s) => s
            .parse()
            .map(Some)
            .map_err(|e| format!("EXARCH_DISK_WARN_BYTES: '{s}' is not a byte count: {e}")),
        Err(_) => Ok(None),
    }
}

/// Load the custom providers declared in `$XDG_CONFIG_HOME/exarch/config.ral`;
/// an absent file is the empty list, so famous providers need no config at all.
///
/// It evaluates in a throwaway shell, never the session shell: a value to compute,
/// not bindings to leak into the agent's scope — as `crate::policy::base` does for
/// the built-in capability profiles.
///
/// # Errors
/// A present file that is unreadable, that raises, or that fails to decode is a
/// hard error: a mistyped endpoint is a misdirected agent, not a recoverable
/// default.
pub fn load() -> Result<Vec<CustomProvider>, String> {
    let path = crate::bootstrap::EXARCH
        .xdg_dir(ral_core::path::basedir::XdgKind::Config)
        .join(CONFIG_FILE);
    let Some(source) = read_optional_file(&path, LABEL)? else {
        return Ok(Vec::new());
    };
    let source = ral_core::source::normalize_source_text(source);
    let display = path.to_string_lossy().into_owned();
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    decode(
        evaluate_no_authority(&mut shell, &source, &display, LABEL)?,
        &display,
    )
}

/// Read a trusted `.ral` config or policy file, `Ok(None)` when it is absent. A
/// present-but-unreadable file is an error, never silently treated as absent.
/// Shared with [`crate::net_policy::load`].
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:config-load] reads a trusted configuration or policy file (the unusual-provider config, or the IT-owned network policy) from a fixed path to set up transport/egress rules; configuration loading at setup, not turn-time model data I/O."
)]
pub(crate) fn read_optional_file(
    path: &std::path::Path,
    label: &str,
) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{label} {}: {e}", path.display())),
    }
}

/// Evaluate `source` under a [`Capabilities::deny_all`] frame, so the config can
/// compute a value but reach no effect. Shared with [`crate::net_policy::load`].
pub(crate) fn evaluate_no_authority(
    shell: &mut Shell,
    source: &str,
    display: &str,
    label: &str,
) -> Result<Value, String> {
    // No run is in hand — a throwaway shell computing a value — so nothing is
    // installed: no surface, no desk, no nursery.
    let mooring = Mooring::adrift();
    shell
        .with_capabilities(Capabilities::deny_all(), |sh| {
            ral_core::builtins::modules::evaluate_source(&mooring, sh, source, display)
        })
        .map_err(|e| match e {
            Break::Error(err) => format!("{label} {display}: {}", err.message),
            other @ Break::Escape(_) => format!("{label} {display}: {other:?}"),
        })
}

/// Decode the config's terminal map of `name → declaration` into
/// [`CustomProvider`]s.
fn decode(value: Value, display: &str) -> Result<Vec<CustomProvider>, String> {
    let Value::Map(map) = value else {
        return Err(format!(
            "exarch config {display}: expected a map of provider declarations \
             (`[name: [endpoint: ..., key: ..., protocol: ...]]`), got {}",
            value.type_name()
        ));
    };
    let mut providers = Vec::with_capacity(map.len());
    for (name, decl) in &map {
        providers.push(decode_one(name, decl, display)?);
    }
    Ok(providers)
}

fn decode_one(name: &str, decl: &Value, display: &str) -> Result<CustomProvider, String> {
    let where_ = format!("exarch config {display}: provider '{name}'");
    let Value::Map(fields) = decl else {
        return Err(format!(
            "{where_}: expected a map [endpoint: ..., key: ..., protocol: ...], got {}",
            decl.type_name()
        ));
    };
    for key in fields.keys() {
        if !matches!(key.as_str(), "endpoint" | "key" | "protocol") {
            return Err(format!(
                "{where_}: unknown key '{key}' — expected endpoint, key, protocol"
            ));
        }
    }
    let endpoint = string_field(fields.get("endpoint"), "endpoint", &where_)?;
    // Omitting `key` declares a no-auth local endpoint (Ollama et al.), which
    // `provider::credential` resolves to an inert placeholder bearer.
    let key_env = optional_string_field(fields.get("key"), "key", &where_)?;
    let protocol = string_field(fields.get("protocol"), "protocol", &where_)?;
    Ok(CustomProvider {
        label: name.to_string(),
        key_env,
        endpoint,
        adapter: adapter_for_protocol(&protocol, &where_)?,
    })
}

fn string_field(value: Option<&Value>, field: &str, where_: &str) -> Result<String, String> {
    match value {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!(
            "{where_}: '{field}' must be a string, got {}",
            other.type_name()
        )),
        None => Err(format!("{where_}: missing '{field}'")),
    }
}

/// A present-but-non-string value is still an error: an omission is deliberate,
/// a wrong type is a mistake to surface.
fn optional_string_field(
    value: Option<&Value>,
    field: &str,
    where_: &str,
) -> Result<Option<String>, String> {
    match value {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!(
            "{where_}: '{field}' must be a string, got {}",
            other.type_name()
        )),
    }
}

/// The three wire protocols the config admits: `completions` is `OpenAI` v1,
/// `responses` is `OpenAI` v2, `anthropic` the Anthropic native protocol.
fn adapter_for_protocol(protocol: &str, where_: &str) -> Result<AdapterKind, String> {
    match protocol {
        "completions" => Ok(AdapterKind::OpenAI),
        "responses" => Ok(AdapterKind::OpenAIResp),
        "anthropic" => Ok(AdapterKind::Anthropic),
        other => Err(format!(
            "{where_}: unknown protocol '{other}' — expected completions, responses, or anthropic"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluate and decode a config source the way [`load`] does.
    fn parse(source: &str) -> Result<Vec<CustomProvider>, String> {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let source = ral_core::source::normalize_source_text(source.to_string());
        decode(
            evaluate_no_authority(&mut shell, &source, "<test:config>", LABEL)?,
            "<test:config>",
        )
    }

    /// Map iteration is sorted by key, so the providers come back in label order.
    #[test]
    fn three_protocols_map_to_adapters() {
        let providers = parse(
            "return [
               a-anthropic: [endpoint: 'https://a.example/', key: 'A_KEY', protocol: 'anthropic'],
               b-completions: [endpoint: 'https://b.example/v1/', key: 'B_KEY', protocol: 'completions'],
               c-responses: [endpoint: 'https://c.example/v1/', key: 'C_KEY', protocol: 'responses'],
             ]",
        )
        .expect("valid config should parse");
        assert_eq!(providers.len(), 3);
        assert_eq!(providers[0].label, "a-anthropic");
        assert_eq!(providers[0].adapter, AdapterKind::Anthropic);
        assert_eq!(providers[0].key_env, Some("A_KEY".to_string()));
        assert_eq!(providers[0].endpoint, "https://a.example/");
        assert_eq!(providers[1].adapter, AdapterKind::OpenAI);
        assert_eq!(providers[2].adapter, AdapterKind::OpenAIResp);
    }

    #[test]
    fn unknown_protocol_errors() {
        let err = parse("return [weird: [endpoint: 'https://w/', key: 'W', protocol: 'grpc']]")
            .unwrap_err();
        assert!(err.contains("unknown protocol 'grpc'"), "got: {err}");
        assert!(err.contains("weird"), "should name the provider: {err}");
    }

    #[test]
    fn missing_field_errors() {
        let err = parse("return [x: [key: 'X_KEY', protocol: 'completions']]").unwrap_err();
        assert!(err.contains("missing 'endpoint'"), "got: {err}");
    }

    /// `endpoint` and `protocol` stay required when `key` is dropped.
    #[test]
    fn missing_key_is_a_no_auth_provider() {
        let providers = parse(
            "return [ollama: [endpoint: 'http://localhost:11434/v1/', protocol: 'completions']]",
        )
        .expect("a keyless custom provider should decode");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].label, "ollama");
        assert_eq!(providers[0].key_env, None);
        assert_eq!(providers[0].endpoint, "http://localhost:11434/v1/");
        assert_eq!(providers[0].adapter, AdapterKind::OpenAI);
    }

    #[test]
    fn non_string_key_errors() {
        let err = parse("return [x: [endpoint: 'https://x/', key: 7, protocol: 'completions']]")
            .unwrap_err();
        assert!(err.contains("'key' must be a string"), "got: {err}");
    }

    /// Rejected, rather than silently dropped.
    #[test]
    fn unknown_field_errors() {
        let err = parse(
            "return [x: [endpoint: 'https://x/', key: 'K', protocol: 'completions', model: 'm']]",
        )
        .unwrap_err();
        assert!(err.contains("unknown key 'model'"), "got: {err}");
    }

    #[test]
    fn non_map_terminal_errors() {
        let err = parse("return 42").unwrap_err();
        assert!(err.contains("expected a map"), "got: {err}");
    }

    /// A bare command statement is ral's surface for an external invocation, so
    /// this is the whole exfiltration route: stopped at the in-process exec gate.
    #[cfg(unix)]
    #[test]
    fn no_authority_grant_denies_exec() {
        let err = parse("/bin/echo pwned\nreturn []").unwrap_err();
        assert!(
            err.contains("denied by active grant"),
            "exec must be denied under the no-authority grant; got: {err}"
        );
    }
}
