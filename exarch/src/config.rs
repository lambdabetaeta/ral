//! The unusual-provider config (`$XDG_CONFIG_HOME/exarch/config.ral`), plus one
//! environment knob the binary reads at boot.
//!
//! Built-in services need no declaration; a self-hosted or non-built-in
//! endpoint is the one thing exarch cannot know, so it is declared here: where
//! it lives, how it speaks, and whether it checks a key. Never the key itself,
//! which comes from the environment or the vault
//! ([`crate::provider::credential`]). The file is source, not data: it runs
//! through the same
//! [`ral_core::load::evaluate_source`] core as any other `.ral` load,
//! and its terminal map decodes into [`Service`]s.
//!
//! A redirected `endpoint` aims the agent's own traffic at an arbitrary server, so
//! the file is an exfiltration channel to whoever can write it. Hence two defences:
//! it lives at the XDG config home, never the working tree the sandboxed agent can
//! write, and it evaluates under [`Capabilities::deny_all`] — with `exec` denied
//! there is no route to the network, so the in-process guard suffices.

use crate::provider::identity::{adapter_for_protocol, key_var, protocol_for_adapter, protocols};
use crate::provider::{Auth, Service, ServiceName, built_in};
use ral_core::Shell;
use ral_core::capability::Capabilities;
use ral_core::types::{Break, Escape, Mooring, Value};

const CONFIG_FILE: &str = "config.ral";

/// Error-message prefix; `net_policy` passes its own to the shared helpers below.
pub(crate) const LABEL: &str = "exarch config";

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

/// Where exarch's declarations live.
pub fn path() -> std::path::PathBuf {
    crate::app::EXARCH
        .xdg_dir(ral_core::host::XdgKind::Config)
        .join(CONFIG_FILE)
}

/// The service declarations in `path`, an absent file being the empty list.
///
/// It evaluates in a throwaway shell, never the session shell: a value to
/// compute, not bindings to leak into the agent's scope — as
/// `crate::policy::base` does for the built-in capability profiles. `label`
/// opens every complaint, so the file says what it is ("exarch config",
/// "provider settings") before it says what is wrong.
///
/// # Errors
/// A present file that is unreadable, that raises, or that fails to decode is
/// a hard error: a mistyped endpoint is a misdirected agent, not a recoverable
/// default.
pub fn load_declared(path: &std::path::Path, label: &str) -> Result<Vec<Service>, String> {
    let Some(source) = read_optional_file(path, label)? else {
        return Ok(Vec::new());
    };
    let source = ral_core::source::normalize_source_text(source);
    let display = path.to_string_lossy().into_owned();
    let mut shell =
        ral_core::HostSurface::default().shell(ral_core::terminal::TerminalState::default());
    decode(
        evaluate_no_authority(&mut shell, &source, &display, label)?,
        &display,
        label,
    )
}

/// Write `services` back to `path` as the very source [`load_declared`]
/// reads — a file a program wrote and a person can still edit.
///
/// A `key` field is written only for a server that checks none; this file
/// carries no secret and needs no special permissions.
///
/// # Errors
/// Returns a plain sentence if a name or endpoint could not be written as a
/// quoted string without lying about its contents, or if the file could not
/// be created.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:config-write] writes the provider declarations back to the app's own config directory; configuration, not turn-time model data I/O"
)]
pub fn save_declared(
    path: &std::path::Path,
    services: &[Service],
    label: &str,
) -> Result<(), String> {
    let declarations: String = services
        .iter()
        .map(|service| -> Result<String, String> {
            let name = quoted(service.name.as_str(), "name", label)?;
            let endpoint = service.endpoint.as_deref().ok_or_else(|| {
                format!(
                    "{label}: '{}' names no address to write; a declared endpoint \
                     always has one; is this really a declared service?",
                    service.name
                )
            })?;
            let endpoint = quoted(endpoint, "address", label)?;
            let key = match service.auth {
                Auth::Keyless => ", key: `none",
                Auth::Key(_) | Auth::OAuth => "",
            };
            let protocol = protocol_for_adapter(service.adapter).ok_or_else(|| {
                format!(
                    "{label}: '{}' speaks a protocol this file has no name for; \
                     it can only hold one of {}.",
                    service.name,
                    protocols().join(", ")
                )
            })?;
            Ok(format!(
                "  {name}: [endpoint: {endpoint}{key}, protocol: '{protocol}'],\n"
            ))
        })
        .collect::<Result<_, _>>()?;
    let source = format!("{PREAMBLE}return [\n{declarations}]\n");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not make {}: {e}", parent.display()))?;
    }
    std::fs::write(path, source).map_err(|e| format!("could not write {}: {e}", path.display()))
}

const PREAMBLE: &str = "\
# Written by the accounts screen, and safe to edit by hand.
# Each entry is one service to talk to: where it lives, and how it speaks.
# Never a key: one is read from NAME_API_KEY, else the saved keys.
# key: `none marks a server that checks none.
";

/// `text` as a single-quoted ral string — refused outright if it holds a
/// quote, a backslash, or a control character.  Escaping them would be easy
/// and wrong: these are names and addresses a person typed into a window, and
/// a service whose name contains a quote is a typo to ask about, not a string
/// to encode.
fn quoted(text: &str, field: &str, label: &str) -> Result<String, String> {
    if text.is_empty() {
        return Err(format!("{label}: the {field} is empty; what should it be?"));
    }
    if text
        .bytes()
        .any(|b| b == b'\'' || b == b'\\' || b < 0x20 || b == 0x7f)
    {
        return Err(format!(
            "{label}: the {field} '{text}' contains a quote, a backslash, or a control \
             character: is that really part of it?"
        ));
    }
    Ok(format!("'{text}'"))
}

/// Read a trusted `.ral` config or policy file, `Ok(None)` when it is absent. A
/// present-but-unreadable file is an error, never silently treated as absent.
/// Shared with [`crate::net_policy::load`].
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:config-load] reads a trusted configuration or policy file (the unusual-provider config, or the network policy) from a fixed path to set up transport/egress rules; configuration loading at setup, not turn-time model data I/O."
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
            ral_core::load::evaluate_source(&mooring, sh, source, display, None)
        })
        .map_err(|e| match e {
            Break::Error(err) => format!("{label} {display}: {}", err.message),
            Break::Escape(Escape::Exit(code)) => format!(
                "{label} {display}: the config called exit {code}, but a config file is \
                 read for its declarations, not run to completion: is that really what it \
                 should do?"
            ),
        })
}

/// Decode the config's terminal map of `name → declaration` into [`Service`]s.
fn decode(value: Value, display: &str, label: &str) -> Result<Vec<Service>, String> {
    let Value::Map(map) = value else {
        return Err(format!(
            "{label} {display}: expected a map of provider declarations \
             (`[name: [endpoint: ..., protocol: ...]]`), got {}",
            value.type_name()
        ));
    };
    let mut services = Vec::with_capacity(map.len());
    for (name, decl) in &map {
        services.push(decode_one(name, &decl, display, label)?);
    }
    Ok(services)
}

fn decode_one(name: &str, decl: &Value, display: &str, label: &str) -> Result<Service, String> {
    let where_ = format!("{label} {display}: provider '{name}'");
    let Value::Map(fields) = decl else {
        return Err(format!(
            "{where_}: expected a map [endpoint: ..., protocol: ...], got {}",
            decl.type_name()
        ));
    };
    for key in fields.keys() {
        if !matches!(key, "endpoint" | "key" | "protocol") {
            return Err(format!(
                "{where_}: unknown key '{key}'; expected endpoint, key, protocol"
            ));
        }
    }
    let endpoint = string_field(fields.get("endpoint").as_deref(), "endpoint", &where_)?;
    let protocol = string_field(fields.get("protocol").as_deref(), "protocol", &where_)?;

    let name = ServiceName::declared(name).map_err(|e| format!("{label} {display}: {e}"))?;
    if built_in(&name).is_some() {
        return Err(format!(
            "{where_}: '{name}' is already a built-in service; a declared \
             provider cannot reuse its name."
        ));
    }
    let auth = match fields.get("key").as_deref() {
        None => Auth::Key(key_var(&name)),
        Some(Value::Variant {
            label,
            payload: None,
        }) if &**label == "none" => Auth::Keyless,
        // Never echoed: what was written may be the key itself.
        Some(_) => {
            return Err(format!(
                "{where_}: 'key' can only be `none, for a server that checks no key. \
                 A key is read from {}, else from the saved keys, and never from \
                 this file: drop the field?",
                key_var(&name)
            ));
        }
    };

    Ok(Service::declared(
        name,
        endpoint,
        adapter_for_protocol(&protocol, &where_)?,
        auth,
    ))
}

fn string_field(value: Option<&Value>, field: &str, where_: &str) -> Result<String, String> {
    match value {
        Some(Value::String(s)) => Ok(s.to_string()),
        Some(other) => Err(format!(
            "{where_}: '{field}' must be a string, got {}",
            other.type_name()
        )),
        None => Err(format!("{where_}: missing '{field}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::adapter::AdapterKind;

    /// Evaluate and decode a config source the way [`load_declared`] does.
    fn parse(source: &str) -> Result<Vec<Service>, String> {
        let mut shell = ral_core::test_helper::core_shell();
        let source = ral_core::source::normalize_source_text(source.to_string());
        decode(
            evaluate_no_authority(&mut shell, &source, "<test:config>", LABEL)?,
            "<test:config>",
            LABEL,
        )
    }

    /// What a window writes, the decoder reads back — the same three fields,
    /// the same protocol, and a keyless entry still keyless.
    #[test]
    fn declarations_round_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "exarch-declared-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("providers.ral");

        let written = vec![
            Service::declared(
                ServiceName::declared("house-llm").unwrap(),
                "https://llm.house.example/v1/".into(),
                AdapterKind::OpenAIResp,
                Auth::Key("HOUSE_LLM_API_KEY".into()),
            ),
            Service::declared(
                ServiceName::declared("ollama").unwrap(),
                "http://localhost:11434/v1/".into(),
                AdapterKind::OpenAI,
                Auth::Keyless,
            ),
        ];
        save_declared(&path, &written, LABEL).expect("write");
        let read = load_declared(&path, LABEL).expect("read back");
        assert_eq!(read, written);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An absent file is no providers, not a failure — the state every
    /// computer starts in.
    #[test]
    fn an_absent_file_declares_nothing() {
        let path = std::env::temp_dir().join("exarch-declared-absent-none.ral");
        std::fs::remove_file(&path).ok();
        assert_eq!(load_declared(&path, LABEL), Ok(Vec::new()));
    }

    /// A name with a quote in it is asked about, not escaped into the file.
    #[test]
    fn a_quote_in_a_name_is_refused_with_a_question() {
        let path = std::env::temp_dir().join("exarch-declared-quote.ral");
        let err = save_declared(
            &path,
            &[Service::declared(
                ServiceName::declared("it's-llm").unwrap(),
                "https://x.example/v1/".into(),
                AdapterKind::OpenAI,
                Auth::Keyless,
            )],
            LABEL,
        )
        .unwrap_err();
        assert!(err.contains("is that really part of it?"), "got: {err}");
    }

    /// Map iteration is sorted by key, so the providers come back in name order.
    #[test]
    fn three_protocols_map_to_adapters() {
        let services = parse(
            "return [
               a-anthropic: [endpoint: 'https://a.example/', protocol: 'anthropic'],
               b-completions: [endpoint: 'https://b.example/v1/', protocol: 'completions'],
               c-responses: [endpoint: 'https://c.example/v1/', protocol: 'responses'],
             ]",
        )
        .expect("valid config should parse");
        assert_eq!(services.len(), 3);
        assert_eq!(services[0].name.as_str(), "a-anthropic");
        assert_eq!(services[0].adapter, AdapterKind::Anthropic);
        assert_eq!(services[0].auth, Auth::Key("A_ANTHROPIC_API_KEY".into()));
        assert_eq!(services[0].endpoint.as_deref(), Some("https://a.example/"));
        assert_eq!(services[1].adapter, AdapterKind::OpenAI);
        assert_eq!(services[2].adapter, AdapterKind::OpenAIResp);
    }

    #[test]
    fn unknown_protocol_errors() {
        let err = parse("return [weird: [endpoint: 'https://w/', protocol: 'grpc']]").unwrap_err();
        assert!(err.contains("unknown protocol 'grpc'"), "got: {err}");
        assert!(err.contains("weird"), "should name the provider: {err}");
    }

    #[test]
    fn missing_field_errors() {
        let err = parse("return [x: [protocol: 'completions']]").unwrap_err();
        assert!(err.contains("missing 'endpoint'"), "got: {err}");
    }

    #[test]
    fn key_none_declares_a_server_that_checks_no_key() {
        let services = parse(
            "return [ollama: [endpoint: 'http://localhost:11434/v1/', protocol: 'completions', key: `none]]",
        )
        .expect("a keyless declared service should decode");
        assert_eq!(services[0].auth, Auth::Keyless);
    }

    /// The file never holds a key, nor names where one is: what was written
    /// in its place may be the key itself, so it is refused without an echo.
    #[test]
    fn a_key_written_into_the_file_is_refused_unechoed() {
        for key in ["'sk-secret'", "7", "`some 'sk-secret'"] {
            let err = parse(&format!(
                "return [x: [endpoint: 'https://x/', key: {key}, protocol: 'completions']]"
            ))
            .unwrap_err();
            assert!(err.contains("X_API_KEY"), "got: {err}");
            assert!(!err.contains("sk-secret"), "got: {err}");
        }
    }

    /// A declared name cannot shadow a built-in: once an account id is a
    /// service name, a shadow would be an identity collision.
    #[test]
    fn a_declared_name_colliding_with_a_built_in_is_refused() {
        let err =
            parse("return [anthropic: [endpoint: 'https://x.example/', protocol: 'anthropic']]")
                .unwrap_err();
        assert!(err.contains("anthropic"), "{err}");
        assert!(err.contains("built-in"), "{err}");
    }

    /// Rejected, rather than silently dropped.
    #[test]
    fn unknown_field_errors() {
        let err =
            parse("return [x: [endpoint: 'https://x/', protocol: 'completions', model: 'm']]")
                .unwrap_err();
        assert!(err.contains("unknown key 'model'"), "got: {err}");
    }

    #[test]
    fn non_map_terminal_errors() {
        let err = parse("return 42").unwrap_err();
        assert!(err.contains("expected a map"), "got: {err}");
    }

    /// A bare command statement is ral's surface for an external invocation, so
    /// this is the whole exfiltration route: stopped at the in-process exec guard.
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
