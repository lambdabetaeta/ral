//! Operator-set exarch configuration: the unusual-provider file
//! (`$XDG_CONFIG_HOME/exarch/config.ral`) and a handful of standalone
//! environment-variable knobs the binary reads once at startup.
//!
//! ## Unusual providers
//!
//! Famous providers auto-populate from the environment and carry no config
//! ([`crate::credential`]). A *custom* endpoint — self-hosted or non-famous —
//! is the one thing exarch cannot know: it is declared here, with its base
//! URL, its key env var, and its wire protocol. Slice 3 of the
//! provider-config ADR.
//!
//! The config is source-code, so it is evaluated, not parsed: a hand-written
//! `config.ral` whose terminal expression is a map
//! `[name: [endpoint: ..., key: ..., protocol: ...], ...]` runs through the
//! shared [`ral_core::builtins::modules::evaluate_source`] path — the same
//! core every other `.ral` load uses — and its terminal value is decoded into
//! [`CustomProvider`]s.
//!
//! ### Authority
//!
//! A redirected `endpoint` points the agent's own traffic at an arbitrary
//! server, so the config is an exfiltration channel if an attacker can write
//! it. Two structural defences:
//!
//! - it lives at the trusted XDG config home, **never** the working tree, so
//!   the sandboxed agent (which writes only cwd and scratch) cannot reach it;
//! - it is evaluated under a **no-authority grant** ([`Capabilities::deny_all`]
//!   — `exec`, `net`, and `fs` all denied), so the config can only compute a
//!   value, never cause an effect. With `exec` denied there is no route to the
//!   network, so the in-process gate suffices.
//!
//! ## Disk-warn ceiling
//!
//! [`disk_warn_bytes`] is a much smaller, unrelated knob: an optional
//! operator-set byte ceiling for the session log dir + scratch, read from an
//! environment variable rather than the `.ral` file (there is no value to
//! *compute* here, just a number to read once at boot). Absent means no
//! walks and no warnings, ever
//! (`decisions/260705_leases-and-budgets`, "Disk: report and warn only").

use crate::provider::CustomProvider;
use genai::adapter::AdapterKind;
use ral_core::Shell;
use ral_core::types::{Break, Capabilities, Value};

/// The config file name under `$XDG_CONFIG_HOME/exarch/`.
const CONFIG_FILE: &str = "config.ral";

/// The operator-set disk-warn ceiling, in bytes, or `None` if unset or
/// unparseable — absent means [`crate::agent::Agent::check_disk_warn`] never
/// walks the log/scratch dirs at all, no cost paid ever
/// (`decisions/260705_leases-and-budgets`, "Disk: report and warn only").
/// Read once at startup, like every other launch setting; there is
/// deliberately no live-reload.
pub fn disk_warn_bytes() -> Option<u64> {
    std::env::var("EXARCH_DISK_WARN_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Load the custom providers declared in `$XDG_CONFIG_HOME/exarch/config.ral`.
///
/// An absent file is a no-op: the empty list, so famous providers work with no
/// config at all. A present file is evaluated through the shared
/// `evaluate_source` path under a no-authority grant and its terminal map is
/// decoded into [`CustomProvider`]s. A malformed config is a hard error — a
/// custom endpoint mistyped is a misdirected agent, not a recoverable default,
/// so the user must see and fix it.
///
/// The config evaluates in a throwaway shell, never the session shell: the
/// config is a value to compute, not bindings to leak into the agent's scope.
/// This mirrors [`crate::policy::base`]'s built-in-profile loader, which
/// likewise runs a capability `.ral` script in a fresh `Shell::new`.
pub fn load() -> Result<Vec<CustomProvider>, String> {
    let path =
        crate::bootstrap::xdg_app_dir(ral_core::path::basedir::XdgKind::Config).join(CONFIG_FILE);
    let Some(source) = read_config(&path)? else {
        return Ok(Vec::new());
    };
    let source = ral_core::source::normalize_source_text(source);
    let display = path.to_string_lossy().into_owned();
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    decode(
        evaluate_no_authority(&mut shell, &source, &display)?,
        &display,
    )
}

/// Read the config file: `Ok(None)` when it is absent (a no-op), `Ok(Some)`
/// with its text when present, and a hard error on a genuine IO failure
/// (permission denied, say) — a present-but-unreadable config is never
/// silently treated as absent.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:provider-config] reads the unusual-provider config from the trusted XDG config home to configure transport; configuration loading at setup, not turn-time model data I/O."
)]
fn read_config(path: &std::path::Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("exarch config {}: {e}", path.display())),
    }
}

/// Evaluate `source` through the shared `evaluate_source` core under a
/// [`Capabilities::deny_all`] frame, so the config can compute a value but
/// reach no effect. Errors are surfaced with the file path for provenance.
fn evaluate_no_authority(shell: &mut Shell, source: &str, display: &str) -> Result<Value, String> {
    shell
        .with_capabilities(Capabilities::deny_all(), |sh| {
            ral_core::builtins::modules::evaluate_source(sh, source, display)
        })
        .map_err(|e| match e {
            Break::Error(err) => format!("exarch config {display}: {}", err.message),
            other @ Break::Escape(_) => format!("exarch config {display}: {other:?}"),
        })
}

/// Decode the config's terminal value — a map of `name → declaration` — into
/// [`CustomProvider`]s. Strict on shape and on per-entry keys: a malformed
/// declaration is a hard error naming the offending provider.
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

/// Decode one `name → [endpoint: ..., key: ..., protocol: ...]` declaration.
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
    let key_env = string_field(fields.get("key"), "key", &where_)?;
    let protocol = string_field(fields.get("protocol"), "protocol", &where_)?;
    Ok(CustomProvider {
        label: name.to_string(),
        key_env,
        endpoint,
        adapter: adapter_for_protocol(&protocol, &where_)?,
    })
}

/// Require a present `String`-valued field, naming the field on a miss.
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

/// Map a wire-protocol name onto the genai [`AdapterKind`] it is. The three
/// the config admits map 1:1 — `completions` is `OpenAI` v1, `responses` is
/// `OpenAI` v2, `anthropic` is the Anthropic native protocol.
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

    /// Evaluate a config `source` string the way [`load`] does — through the
    /// shared core under the no-authority grant, in a fresh throwaway shell —
    /// and decode it.
    fn parse(source: &str) -> Result<Vec<CustomProvider>, String> {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let source = ral_core::source::normalize_source_text(source.to_string());
        decode(
            evaluate_no_authority(&mut shell, &source, "<test:config>")?,
            "<test:config>",
        )
    }

    /// One custom provider of each of the three protocols decodes into the
    /// right `AdapterKind`. Map iteration is sorted by key, so the result is
    /// in label order.
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
        assert_eq!(providers[0].key_env, "A_KEY");
        assert_eq!(providers[0].endpoint, "https://a.example/");
        assert_eq!(providers[1].adapter, AdapterKind::OpenAI);
        assert_eq!(providers[2].adapter, AdapterKind::OpenAIResp);
    }

    /// An unknown protocol name is a hard error naming the provider.
    #[test]
    fn unknown_protocol_errors() {
        let err = parse("return [weird: [endpoint: 'https://w/', key: 'W', protocol: 'grpc']]")
            .unwrap_err();
        assert!(err.contains("unknown protocol 'grpc'"), "got: {err}");
        assert!(err.contains("weird"), "should name the provider: {err}");
    }

    /// A missing required field is a hard error.
    #[test]
    fn missing_field_errors() {
        let err =
            parse("return [x: [endpoint: 'https://x/', protocol: 'completions']]").unwrap_err();
        assert!(err.contains("missing 'key'"), "got: {err}");
    }

    /// An unknown declaration key is rejected rather than silently dropped.
    #[test]
    fn unknown_field_errors() {
        let err = parse(
            "return [x: [endpoint: 'https://x/', key: 'K', protocol: 'completions', model: 'm']]",
        )
        .unwrap_err();
        assert!(err.contains("unknown key 'model'"), "got: {err}");
    }

    /// A non-map terminal value is a hard error.
    #[test]
    fn non_map_terminal_errors() {
        let err = parse("return 42").unwrap_err();
        assert!(err.contains("expected a map"), "got: {err}");
    }

    /// The no-authority grant denies `exec`: a config that tries to run a
    /// command is rejected rather than spawning it. A bare command statement
    /// (`/bin/echo …`) is the ral surface for an external invocation; under
    /// [`Capabilities::deny_all`] it hits the in-process exec gate and fails
    /// with a grant-denial, so the config never reaches the network either
    /// (with `exec` denied there is no route to it).
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
