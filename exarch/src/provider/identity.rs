//! Services and accounts: where bytes go, and who is asking.
//!
//! A service is an endpoint, a wire adapter, a billing flavour and a model
//! list. An account is an identity, a credential, and a name for itself. One
//! service may own many accounts — a `ChatGPT` login email carries a personal
//! account and one per workspace, each with its own issued id — so the two are
//! kept apart rather than flattened into a single provider identity whose only
//! distinguishing field would be its name.

use genai::adapter::AdapterKind;

/// A service's identity, and what a human types after `--provider`.
///
/// Two doors, because the two sources differ in trust: the built-in table is
/// known good, a declaration is input. A colon is refused because it separates
/// the halves of an [`AccountId`] below, and a name that could contain one
/// would make that rendering ambiguous.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceName(String);

impl ServiceName {
    /// For the built-in table only.
    pub(crate) fn built_in(name: &'static str) -> Self {
        debug_assert!(
            Self::declared(name).is_ok(),
            "a built-in service name must satisfy what `declared` refuses — \
             the colon rule is what keeps AccountId renderings injective"
        );
        Self(name.to_string())
    }

    /// # Errors
    /// Empty, colon-bearing, or control-bearing names are refused, each with
    /// its own sentence — this is the message a mistyped `config.ral` gets.
    pub fn declared(name: &str) -> Result<Self, String> {
        if name.is_empty() {
            return Err("A provider name cannot be empty. What is this service called?".into());
        }
        if name.contains(':') {
            return Err(format!(
                "A provider name cannot contain a colon, and `{name}` does. \
                 A colon separates a service from one of its accounts, so a name \
                 carrying one could not be told from a pair."
            ));
        }
        if name.contains(char::is_control) {
            return Err(format!(
                "A provider name cannot contain control characters, and `{}` does. \
                 Did a newline or a tab slip into the declaration?",
                name.escape_debug()
            ));
        }
        Ok(Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ServiceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An account's identity, unique across every service and every product.
///
/// A key-bearing service renders as its own name; a login as
/// `"{service}:{issued}"`, where `issued` is the id the issuer gave it.
/// Service names carry no colon, so the first colon separates the halves and
/// the rendering is injective.
///
/// **Nothing ever parses one.** Every use — `state.json`, the model cache, the
/// record log, synod's wire, `--provider` — compares a rendering against the
/// renderings of the accounts actually present. There is no `from_str`, and
/// adding one would reintroduce the ambiguity this type exists to prevent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(String);

impl AccountId {
    pub fn of_service(name: &ServiceName) -> Self {
        Self(name.0.clone())
    }

    pub fn of_login(service: &ServiceName, issued: &str) -> Self {
        Self(format!("{}:{issued}", service.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where requests go and how they are spoken. Plain data: a built-in service is
/// a row of a static table, a declared one is the same struct parsed from a
/// declarations file. Provenance is not a type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    pub name: ServiceName,
    pub endpoint: Option<String>,
    pub adapter: AdapterKind,
    /// `None` for chatgpt and for declared endpoints, which name no model of
    /// their own; the selection then has to come from `--model` or the catalog.
    pub default_model: Option<String>,
    pub auth: Auth,
    /// The sole authority on whether this service's turns cost money.
    pub billing: Billing,
    /// Whether this service takes `vendor/model` slugs and serving-endpoint
    /// pins — true for `OpenRouter` alone, and the reason no code below compares
    /// a service name against the string "openrouter".
    pub routes: bool,
}

/// What a *declaration* knows about a request's bearer token. Not where the
/// secret is kept: that is the one difference between exarch and synod, and it
/// must not reach this type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    /// The environment variable naming it.
    Env(String),
    /// A signed-in login. `ChatGPT`'s flow, named for its shape.
    OAuth,
    /// The declaration names no source. Whatever the embedding product's vault
    /// supplies for this account, else the inert `NO_AUTH_PLACEHOLDER` for a
    /// local server that wants no `Authorization` at all — one arm serving
    /// both, because no declaration file has ever recorded which it is.
    Unnamed,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Billing {
    Metered,
    /// A subscription: turns report tokens but never a cost. chatgpt and
    /// opencode-go.
    FlatRate,
}

/// Who is asking. Several accounts may belong to one [`Service`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: AccountId,
    pub service: Service,
    /// What this account calls itself, from its own credential alone: a login
    /// email (qualified by workspace or plan when the token says so), or the
    /// service's name for a key. A local fact — never unique, never a display
    /// string on its own, and therefore never needing reconciliation when a
    /// sibling account arrives or leaves.
    pub handle: String,
}

impl Account {
    /// The sole account of a key-bearing service, whose id and handle are both
    /// the service's name.
    pub fn of_service(service: Service) -> Self {
        Self {
            id: AccountId::of_service(&service.name),
            handle: service.name.as_str().to_string(),
            service,
        }
    }
}

/// Separates a service from the handle of one of its accounts.
const HANDLE_SEPARATOR: &str = " · ";

/// Name one account among the accounts present.
///
/// The service alone when its handle is the service's name — every key-bearing
/// service. Otherwise the service and the handle both, so a `ChatGPT` login
/// never reads as bare "chatgpt" with another beside it, and a lone `ChatGPT`
/// login never loses its email. Two accounts left indistinguishable by their
/// handles are separated by their ids, which are unique by construction — and
/// an account whose handle merely *reads* as another's id-qualified form is
/// qualified too, so a handle cannot impersonate a sibling's label. Nothing is
/// decorated: how an account bills is [`Billing`]'s business, not its name's.
///
/// This is the one place anything in either product names an account, and it
/// takes the set because the answer depends on it. That is precisely why the
/// answer is not a field on [`Account`].
///
/// ```text
/// anthropic
/// opencode-go
/// chatgpt · alex@bristol.ac.uk
/// chatgpt · alex@work (Acme Ltd)
/// ```
pub fn label(account: &Account, among: &[Account]) -> String {
    let named = unqualified(account);
    let collides = among.iter().any(|other| {
        other.id != account.id && (unqualified(other) == named || qualified(other) == named)
    });
    if collides { qualified(account) } else { named }
}

/// Every account's label, comma-joined — the roster an error that must name
/// the choices prints.
pub fn roster(among: &[Account]) -> String {
    among
        .iter()
        .map(|account| label(account, among))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The tiebreak form, with the id appended.
fn qualified(account: &Account) -> String {
    format!("{}{HANDLE_SEPARATOR}{}", unqualified(account), account.id)
}

/// The label before the id is called on to separate a tie.
fn unqualified(account: &Account) -> String {
    let service = account.service.name.as_str();
    let mut named = service.to_string();
    if account.handle != service {
        named.push_str(HANDLE_SEPARATOR);
        named.push_str(&account.handle);
    }
    named
}

/// The built-in table: nine key-bearing services plus chatgpt.
pub fn built_in_services() -> Vec<Service> {
    let keyed =
        |name, endpoint: Option<&str>, adapter, default_model: &str, env, billing| Service {
            name: ServiceName::built_in(name),
            endpoint: endpoint.map(str::to_string),
            adapter,
            default_model: Some(default_model.to_string()),
            auth: Auth::Env(String::from(env)),
            billing,
            routes: false,
        };
    vec![
        keyed(
            "anthropic",
            None,
            AdapterKind::Anthropic,
            "claude-opus-4",
            "ANTHROPIC_API_KEY",
            Billing::Metered,
        ),
        keyed(
            "openai",
            None,
            AdapterKind::OpenAIResp,
            "gpt-5.5",
            "OPENAI_API_KEY",
            Billing::Metered,
        ),
        Service {
            routes: true,
            ..keyed(
                "openrouter",
                Some("https://openrouter.ai/api/v1/"),
                AdapterKind::OpenAI,
                "anthropic/claude-opus-4",
                "OPENROUTER_API_KEY",
                Billing::Metered,
            )
        },
        keyed(
            "deepseek",
            None,
            AdapterKind::DeepSeek,
            "deepseek-chat",
            "DEEPSEEK_API_KEY",
            Billing::Metered,
        ),
        keyed(
            "gemini",
            None,
            AdapterKind::Gemini,
            "gemini-2.5-pro",
            "GEMINI_API_KEY",
            Billing::Metered,
        ),
        // opencode issues one key per account; the endpoint alone tells Zen from Go.
        keyed(
            "opencode-zen",
            Some("https://opencode.ai/zen/v1/"),
            AdapterKind::OpenAI,
            "glm-5.1",
            "OPENCODE_API_KEY",
            Billing::Metered,
        ),
        keyed(
            "opencode-go",
            Some("https://opencode.ai/zen/go/v1/"),
            AdapterKind::OpenAI,
            "glm-5.2",
            "OPENCODE_API_KEY",
            Billing::FlatRate,
        ),
        keyed(
            "xai",
            None,
            AdapterKind::Xai,
            "grok-4.3",
            "XAI_API_KEY",
            Billing::Metered,
        ),
        keyed(
            "qwen",
            Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1/"),
            AdapterKind::OpenAI,
            "qwen3.6-plus",
            "DASHSCOPE_API_KEY",
            Billing::Metered,
        ),
        chatgpt_service(),
    ]
}

pub fn built_in(name: &ServiceName) -> Option<Service> {
    built_in_services().into_iter().find(|s| &s.name == name)
}

/// The chatgpt row by name, since `oauth` mints accounts against it.
///
/// It names no endpoint: the Codex backend a login talks to is reached by a
/// per-request URL override carrying the bearer token, not by a base URL.
pub fn chatgpt_service() -> Service {
    Service {
        name: ServiceName::built_in("chatgpt"),
        endpoint: None,
        adapter: AdapterKind::OpenAIResp,
        default_model: None,
        auth: Auth::OAuth,
        billing: Billing::FlatRate,
        routes: false,
    }
}

/// The service a scripted test backend answers to. Not a row of the table: it
/// must never appear in a picker.
pub fn scripted_service() -> Service {
    Service {
        name: ServiceName::built_in("scripted"),
        endpoint: None,
        adapter: AdapterKind::OpenAIResp,
        default_model: None,
        auth: Auth::Unnamed,
        billing: Billing::Metered,
        routes: false,
    }
}

/// The wire adapter for a specific `model` under `service`.
///
/// Only `OpenAI` splits by model: some of its models still speak the classic
/// Chat Completions API rather than Responses. `AdapterKind::from_model`
/// name-sniffs across every vendor, so a verdict outside the two `OpenAI`
/// adapters is a coincidental match on another vendor's convention, not
/// `OpenAI`'s own split, and is discarded.
///
/// This is the one place a service is compared against a name string, because
/// that split is a fact about one vendor's models rather than a property any
/// declaration could carry.
pub(super) fn adapter_for_model(service: &Service, model: &str) -> AdapterKind {
    if service.name.as_str() != "openai" {
        return service.adapter;
    }
    match AdapterKind::from_model(model).unwrap_or(AdapterKind::OpenAIResp) {
        adapter @ (AdapterKind::OpenAI | AdapterKind::OpenAIResp) => adapter,
        _ => AdapterKind::OpenAIResp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(name: &str) -> Service {
        built_in(&ServiceName::declared(name).unwrap()).unwrap()
    }

    fn login(handle: &str, issued: &str) -> Account {
        let service = chatgpt_service();
        Account {
            id: AccountId::of_login(&service.name, issued),
            service,
            handle: handle.to_string(),
        }
    }

    #[test]
    fn openai_service_keeps_openai_adapter_split() {
        let openai = service("openai");
        assert_eq!(adapter_for_model(&openai, "gpt-4.1"), AdapterKind::OpenAI);
        assert_eq!(
            adapter_for_model(&openai, "gpt-5.5"),
            AdapterKind::OpenAIResp
        );
        assert_eq!(
            adapter_for_model(&service("deepseek"), "gpt-5.5"),
            AdapterKind::DeepSeek
        );
    }

    #[test]
    fn a_declared_name_refuses_a_colon() {
        let refusal = ServiceName::declared("chatgpt:abc").unwrap_err();
        assert!(refusal.contains("colon"), "{refusal}");
        assert!(ServiceName::declared("").is_err());
        assert!(ServiceName::declared("local\nllama").is_err());
        assert_eq!(
            ServiceName::declared("local-llama").unwrap().as_str(),
            "local-llama"
        );
    }

    #[test]
    fn a_key_bearing_account_is_named_by_its_service_alone() {
        let anthropic = Account::of_service(service("anthropic"));
        let go = Account::of_service(service("opencode-go"));
        let among = [anthropic.clone(), go.clone()];
        assert_eq!(label(&anthropic, &among), "anthropic");
        assert_eq!(label(&go, &among), "opencode-go");
        assert_eq!(anthropic.id.as_str(), "anthropic");
    }

    #[test]
    fn a_lone_chatgpt_account_keeps_its_handle() {
        let one = login("alex@bristol.ac.uk", "acct-1");
        assert_eq!(
            label(&one, std::slice::from_ref(&one)),
            "chatgpt · alex@bristol.ac.uk"
        );
    }

    #[test]
    fn two_accounts_on_one_email_draw_two_distinguishable_labels() {
        let personal = login("alex@bristol.ac.uk", "acct-1");
        let work = login("alex@bristol.ac.uk (Acme Ltd)", "acct-2");
        let among = [personal.clone(), work.clone()];
        assert_ne!(label(&personal, &among), label(&work, &among));

        // Handles that stayed identical fall back to the ids, which cannot collide.
        let twin = login("alex@bristol.ac.uk", "acct-2");
        let among = [personal.clone(), twin.clone()];
        assert_ne!(label(&personal, &among), label(&twin, &among));
        assert!(
            label(&twin, &among).ends_with("chatgpt:acct-2"),
            "{}",
            label(&twin, &among)
        );
    }

    /// The claims a handle is built from are issuer- and workspace-supplied,
    /// so one can embed the separator and even a sibling's whole qualified
    /// rendering; the labels must still read apart.
    #[test]
    fn a_handle_embedding_anothers_qualified_rendering_still_reads_apart() {
        let plain = login("alex@work", "acct-1");
        // The twin forces `plain` onto its id-qualified form...
        let twin = login("alex@work", "acct-2");
        // ...which is exactly what this handle spells out.
        let imposter = login("alex@work · chatgpt:acct-1", "acct-3");
        let among = [plain.clone(), twin.clone(), imposter.clone()];
        let labels = [
            label(&plain, &among),
            label(&twin, &among),
            label(&imposter, &among),
        ];
        let distinct: std::collections::BTreeSet<&String> = labels.iter().collect();
        assert_eq!(distinct.len(), 3, "{labels:?}");
    }

    #[test]
    fn an_account_id_renders_injectively() {
        let chatgpt = chatgpt_service().name;
        assert_eq!(AccountId::of_service(&chatgpt).as_str(), "chatgpt");
        assert_eq!(AccountId::of_login(&chatgpt, "abc").as_str(), "chatgpt:abc");
        // No declared service can render as `chatgpt:abc`, because no service
        // name may carry the colon that separates the halves.
        assert!(ServiceName::declared("chatgpt:abc").is_err());
    }
}
