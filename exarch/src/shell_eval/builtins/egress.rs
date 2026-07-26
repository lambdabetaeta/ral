//! `fetch-url`: the model's one action onto the outbound-network enquiry
//! class [`crate::fleet::desk::ExarchDesk::fetch_url`] answers. A sibling of
//! `harness.rs`, kept separate since `fetch-url` is neither agent- nor
//! schedule-family — it crosses the desk seam exactly once, with no launch
//! spine and no grant to hold.

use ral_core::builtins::util::check_arity;
use ral_core::serial::FOValue;
use ral_core::typecheck::builtins::{BuiltinTypeRule, fun, mk_scheme as scheme, pure, thunk};
use ral_core::typecheck::{Scheme, Ty, Unifier};
use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled, sig};
use ral_core::{Shell, Value};
use std::borrow::Cow;

/// `fetch-url <url>` — enquire `` `fetch-url `` with the URL and decode the
/// answer: `Bytes` on success, a didactic error on any other shape (the
/// desk never answers anything else, so this is a shape-check, not a
/// dispatch). The refusal itself — a blocked domain, a rate cap, an
/// over-size response — is the desk's, and reaches here as an ordinary
/// call error.
fn builtin_fetch_url(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "fetch-url")?;
    let url = args[0].to_string();
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "fetch-url".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String { value: url }],
            })),
        },
    )?;
    match answer {
        FOValue::Bytes { value } => Ok(Value::Bytes(value)),
        other => Err(sig(format!(
            "fetch-url: host answered an unexpected shape for its response, got {other:?}"
        ))),
    }
}

/// `fetch-url :: Str → F Bytes`
fn scheme_fetch_url(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::String, pure(Ty::Bytes))))
}

pub static EGRESS_BUILTINS: &[BuiltinEntry] = &[BuiltinEntry {
    name: Cow::Borrowed("fetch-url"),
    type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_fetch_url),
    doc: "fetch-url <url>  — fetch the URL with an HTTP(S) GET and return its body as Bytes; interpolate it as text, or pipe to-string $bytes | from-json/from-csv/… for structured formats. Only sites your IT department has approved may be reached, and one response is capped in size — a fetch to a site not on that list, one that answers too much, or one made too soon after too many others, is refused, naming the site and pointing you to your IT department.",
    body: BuiltinBody::Static(builtin_fetch_url),
}];

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    /// Builtin-table hygiene: `fetch-url` is registered, mirroring
    /// `harness.rs`'s own registration pin for the agent/schedule family.
    #[test]
    fn fetch_url_is_registered_in_egress_builtins() {
        assert!(
            EGRESS_BUILTINS
                .iter()
                .any(|e| e.name.as_ref() == "fetch-url"),
            "fetch-url must be registered in EGRESS_BUILTINS"
        );
    }

    /// The full stack, end to end: a real session's `` fetch-url `` call
    /// reaches [`crate::fleet::desk::ExarchDesk::fetch_url`] through this
    /// builtin's own `enquire`, and a host absent from the (default,
    /// test-only) allowlist comes back as an ordinary call error naming the
    /// host and pointing at IT — proving the builtin decodes the desk's
    /// refusal rather than mishandling it.
    #[test]
    fn fetch_url_full_stack_propagates_the_desks_refusal() {
        let dir =
            std::env::temp_dir().join(format!("exarch-fetch-url-builtin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "fetch-url 'http://127.0.0.1:1/'",
            5,
            &emit,
        );
        assert!(
            result.content.contains("127.0.0.1"),
            "must name the blocked host, got: {}",
            result.content
        );
        assert!(
            result.content.contains("IT department"),
            "must point at IT, got: {}",
            result.content
        );
    }
}
