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
            "a built-in service name must satisfy what `declared` refuses: \
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
    pub auth: Auth,
    /// The sole authority on whether this service's turns cost money.
    pub billing: Billing,
    /// Whether this service takes `vendor/model` slugs and serving-endpoint
    /// pins — true for `OpenRouter` alone, and the reason no code below compares
    /// a service name against the string "openrouter".
    pub routes: bool,
    pub meter: Meter,
}

/// Where this service publishes what is left of its allowance, or that it
/// does not. Every row names one: an absence is an answer a row gives, never
/// a default it forgot.
///
/// Plain data, so a declared service carries one exactly as a built-in row
/// does, and the one `match` that turns it into a request lives in
/// `allowance`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Meter {
    /// The Codex backend's rate-limit readout: a 5-hour and a weekly window,
    /// each a used-percentage.
    Codex,
    /// `GET /api/v1/key`: credits spent against the key's cap, in dollars.
    OpenRouterCredits,
    /// Every key-bearing vendor that simply bills what you use, and a
    /// subscription with no readout (`opencode-go`).
    Unpublished,
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

impl Service {
    /// An endpoint the user declared: metered, unrouted, and unmetered by any
    /// readout.
    pub fn declared(name: ServiceName, endpoint: String, adapter: AdapterKind, auth: Auth) -> Self {
        Self {
            name,
            endpoint: Some(endpoint),
            adapter,
            auth,
            billing: Billing::Metered,
            routes: false,
            meter: Meter::Unpublished,
        }
    }
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

    /// A `ChatGPT` login, identified by its token's `issued` stamp.
    pub fn chatgpt(issued: &str, handle: impl Into<String>) -> Self {
        let service = chatgpt_service();
        Self {
            id: AccountId::of_login(&service.name, issued),
            handle: handle.into(),
            service,
        }
    }
}

#[cfg(test)]
impl Account {
    /// A built-in service's sole account.
    pub(crate) fn built_in(name: &str) -> Self {
        Self::of_service(built_in(&ServiceName::declared(name).unwrap()).unwrap())
    }

    /// A declared endpoint's sole account, keyed by `{NAME}_KEY`.
    pub(crate) fn declared(name: &str) -> Self {
        Self::of_service(Service::declared(
            ServiceName::declared(name).unwrap(),
            format!("https://{name}.example/v1/"),
            AdapterKind::OpenAI,
            Auth::Env(format!("{}_KEY", name.to_uppercase())),
        ))
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
    let keyed = |name, endpoint: Option<&str>, adapter, env, billing, meter| Service {
        name: ServiceName::built_in(name),
        endpoint: endpoint.map(str::to_string),
        adapter,
        auth: Auth::Env(String::from(env)),
        billing,
        routes: false,
        meter,
    };
    vec![
        keyed(
            "anthropic",
            None,
            AdapterKind::Anthropic,
            "ANTHROPIC_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
        ),
        keyed(
            "openai",
            None,
            AdapterKind::OpenAIResp,
            "OPENAI_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
        ),
        Service {
            routes: true,
            ..keyed(
                "openrouter",
                Some("https://openrouter.ai/api/v1/"),
                AdapterKind::OpenAI,
                "OPENROUTER_API_KEY",
                Billing::Metered,
                Meter::OpenRouterCredits,
            )
        },
        keyed(
            "deepseek",
            None,
            AdapterKind::DeepSeek,
            "DEEPSEEK_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
        ),
        keyed(
            "gemini",
            None,
            AdapterKind::Gemini,
            "GEMINI_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
        ),
        // opencode issues one key per account; the endpoint alone tells Zen from Go.
        keyed(
            "opencode-zen",
            Some("https://opencode.ai/zen/v1/"),
            AdapterKind::OpenAI,
            "OPENCODE_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
        ),
        keyed(
            "opencode-go",
            Some("https://opencode.ai/zen/go/v1/"),
            AdapterKind::OpenAI,
            "OPENCODE_API_KEY",
            Billing::FlatRate,
            Meter::Unpublished,
        ),
        keyed(
            "xai",
            None,
            AdapterKind::Xai,
            "XAI_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
        ),
        keyed(
            "qwen",
            Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1/"),
            AdapterKind::OpenAI,
            "DASHSCOPE_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
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
        auth: Auth::OAuth,
        billing: Billing::FlatRate,
        routes: false,
        meter: Meter::Codex,
    }
}

/// The service a scripted test backend answers to. Not a row of the table: it
/// must never appear in a picker.
pub fn scripted_service() -> Service {
    Service {
        name: ServiceName::built_in("scripted"),
        endpoint: None,
        adapter: AdapterKind::OpenAIResp,
        auth: Auth::Unnamed,
        billing: Billing::Metered,
        routes: false,
        meter: Meter::Unpublished,
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

/// The wire protocols a declaration may name, and the adapter each means:
/// `completions` is `OpenAI` v1, `responses` is `OpenAI` v2, `anthropic` the
/// Anthropic native protocol.
///
/// One table read in both directions, so a protocol written out is the one
/// that was read in and neither direction can drift from the other. It is
/// also the list a window offers, in the order it should offer them — most
/// familiar first.
const PROTOCOL_ADAPTERS: &[(&str, AdapterKind)] = &[
    ("completions", AdapterKind::OpenAI),
    ("responses", AdapterKind::OpenAIResp),
    ("anthropic", AdapterKind::Anthropic),
];

/// The adapter `protocol` names.
///
/// # Errors
/// Returns a sentence naming the admissible protocols, so a window can hand a
/// typed-in one straight here rather than keeping a second list of its own.
pub fn adapter_for_protocol(protocol: &str, where_: &str) -> Result<AdapterKind, String> {
    PROTOCOL_ADAPTERS
        .iter()
        .find(|(name, _)| *name == protocol)
        .map(|(_, adapter)| *adapter)
        .ok_or_else(|| {
            format!(
                "{where_}: unknown protocol '{protocol}'; expected {}",
                protocols().join(", ")
            )
        })
}

/// The protocol keyword an adapter was decoded from.
///
/// `None` for an adapter no declaration could have named: [`crate::config::save_declared`]
/// refuses to write one rather than silently filing it under the wrong
/// protocol.
pub fn protocol_for_adapter(adapter: AdapterKind) -> Option<&'static str> {
    PROTOCOL_ADAPTERS
        .iter()
        .find(|(_, known)| *known == adapter)
        .map(|(name, _)| *name)
}

/// The protocol keywords a window offers, most familiar first — the one table
/// above, read as a list.
pub fn protocols() -> Vec<&'static str> {
    PROTOCOL_ADAPTERS.iter().map(|(name, _)| *name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the whole table; `opencode-go` documents that a subscription
    /// with no readout is a choice the row states.
    #[test]
    fn only_chatgpt_and_openrouter_meter_anything() {
        for service in built_in_services() {
            let expected = match service.name.as_str() {
                "chatgpt" => Meter::Codex,
                "openrouter" => Meter::OpenRouterCredits,
                _ => Meter::Unpublished,
            };
            assert_eq!(service.meter, expected, "{}", service.name);
        }
    }

    #[test]
    fn openai_service_keeps_openai_adapter_split() {
        let openai = Account::built_in("openai").service;
        assert_eq!(adapter_for_model(&openai, "gpt-4.1"), AdapterKind::OpenAI);
        assert_eq!(
            adapter_for_model(&openai, "gpt-5.5"),
            AdapterKind::OpenAIResp
        );
        assert_eq!(
            adapter_for_model(&Account::built_in("deepseek").service, "gpt-5.5"),
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
        let anthropic = Account::built_in("anthropic");
        let go = Account::built_in("opencode-go");
        let among = [anthropic.clone(), go.clone()];
        assert_eq!(label(&anthropic, &among), "anthropic");
        assert_eq!(label(&go, &among), "opencode-go");
        assert_eq!(anthropic.id.as_str(), "anthropic");
    }

    #[test]
    fn a_lone_chatgpt_account_keeps_its_handle() {
        let one = Account::chatgpt("acct-1", "alex@bristol.ac.uk");
        assert_eq!(
            label(&one, std::slice::from_ref(&one)),
            "chatgpt · alex@bristol.ac.uk"
        );
    }

    #[test]
    fn two_accounts_on_one_email_draw_two_distinguishable_labels() {
        let personal = Account::chatgpt("acct-1", "alex@bristol.ac.uk");
        let work = Account::chatgpt("acct-2", "alex@bristol.ac.uk (Acme Ltd)");
        let among = [personal.clone(), work.clone()];
        assert_ne!(label(&personal, &among), label(&work, &among));

        // Handles that stayed identical fall back to the ids, which cannot collide.
        let twin = Account::chatgpt("acct-2", "alex@bristol.ac.uk");
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
        let plain = Account::chatgpt("acct-1", "alex@work");
        // The twin forces `plain` onto its id-qualified form...
        let twin = Account::chatgpt("acct-2", "alex@work");
        // ...which is exactly what this handle spells out.
        let imposter = Account::chatgpt("acct-3", "alex@work · chatgpt:acct-1");
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
