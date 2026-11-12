//! Services and accounts: where bytes go, and who is asking.
//!
//! A service is an endpoint, a wire adapter, a billing flavour and a model
//! list. An account is an identity, a credential, and a name for itself. One
//! service may own many accounts, each keyed by the identity its sign-in
//! returns rather than by its human-readable handle.

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
    pub model_protocol: ModelProtocol,
    pub hosted_search: HostedSearch,
    pub auth: Auth,
    /// The sole authority on whether this service's turns cost money.
    pub billing: Billing,
    /// Whether this service takes `vendor/model` slugs and admits a
    /// [`Via`](super::Via) — true for `OpenRouter` alone, and the reason no code
    /// below compares a service name against the string "openrouter".
    pub routes: bool,
    pub meter: Meter,
}

/// How a service selects its wire encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelProtocol {
    Fixed(AdapterKind),
    OpenAI,
}

impl ModelProtocol {
    /// Only a fixed protocol can round-trip through a declaration.
    pub fn declaration_protocol(self) -> Option<&'static str> {
        match self {
            Self::Fixed(adapter) => PROTOCOL_ADAPTERS
                .iter()
                .find(|(_, known)| *known == adapter)
                .map(|(name, _)| *name),
            Self::OpenAI => None,
        }
    }

    /// The adapter used by default and for model listings.
    pub fn default_adapter(self) -> AdapterKind {
        match self {
            Self::Fixed(adapter) => adapter,
            Self::OpenAI => AdapterKind::OpenAIResp,
        }
    }

    /// Vendor-name guesses outside `OpenAI`'s two encodings are discarded.
    pub fn adapter_for_model(self, model: &str) -> AdapterKind {
        match self {
            Self::Fixed(adapter) => adapter,
            Self::OpenAI => match AdapterKind::from_model(model) {
                Ok(adapter @ (AdapterKind::OpenAI | AdapterKind::OpenAIResp)) => adapter,
                _ => AdapterKind::OpenAIResp,
            },
        }
    }
}

/// Hosted search capability, independent of wire encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedSearch {
    None,
    Native,
    ExternalAccess,
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
    /// `GET /api/v1/key`: the effective spending budget, in dollars.
    OpenRouterBudget,
    /// Codex's server-defined usage windows and reset times.
    Codex,
    /// A service that publishes no allowance readout.
    Unpublished,
}

/// How a service's requests authenticate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    /// A key: this environment variable's at launch, else the vault's.
    Key(String),
    /// An account authenticated through this sign-in route.
    OAuth(Login),
    /// A server that checks no key, sent the inert `NO_AUTH_PLACEHOLDER`.
    Keyless,
}

/// Explicit sign-in routes, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Login {
    Chatgpt,
    Codex,
    Grok,
}

impl Login {
    pub const ALL: [Self; 3] = [Self::Chatgpt, Self::Codex, Self::Grok];

    pub fn service(self) -> Service {
        let (name, adapter, meter, hosted_search) = match self {
            Self::Chatgpt => (
                "openai-chatgpt",
                AdapterKind::OpenAIResp,
                Meter::Unpublished,
                HostedSearch::Native,
            ),
            Self::Codex => (
                "openai-codex",
                AdapterKind::OpenAIResp,
                Meter::Codex,
                HostedSearch::ExternalAccess,
            ),
            Self::Grok => (
                "xai-grok",
                AdapterKind::Xai,
                Meter::Unpublished,
                HostedSearch::None,
            ),
        };
        Service {
            name: ServiceName::built_in(name),
            endpoint: None,
            model_protocol: ModelProtocol::Fixed(adapter),
            hosted_search,
            auth: Auth::OAuth(self),
            billing: Billing::FlatRate,
            routes: false,
            meter,
        }
    }

    /// # Errors
    /// Unknown names are refused without aliases.
    pub fn named(name: &str) -> Result<Self, String> {
        match name {
            "openai-chatgpt" => Ok(Self::Chatgpt),
            "openai-codex" => Ok(Self::Codex),
            "xai-grok" => Ok(Self::Grok),
            _ => Err(format!(
                "unknown service '{name}'; sign in to one of: openai-chatgpt, openai-codex, xai-grok"
            )),
        }
    }
}

/// The variable a declared service's key is read from: `{NAME}_API_KEY`, as a
/// built-in's is, with what a shell cannot export folded to `_`.
pub fn key_var(name: &ServiceName) -> String {
    let stem: String = name
        .as_str()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{stem}_API_KEY")
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Billing {
    Metered,
    /// A subscription: turns report tokens but never a cost.
    FlatRate,
}

impl Service {
    /// Whether requests ride a `ChatGPT` plan over `OpenAI`'s token-sharing
    /// route, which refuses sampling controls and an output cap.
    pub fn shares_a_plan(&self) -> bool {
        matches!(self.auth, Auth::OAuth(Login::Chatgpt))
    }

    /// An endpoint the user declared: metered, unrouted, and unmetered by any
    /// readout.
    pub fn declared(name: ServiceName, endpoint: String, adapter: AdapterKind, auth: Auth) -> Self {
        Self {
            name,
            endpoint: Some(endpoint),
            model_protocol: ModelProtocol::Fixed(adapter),
            hosted_search: HostedSearch::None,
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
    /// email, or the service's name for a key. A local fact — never unique, never a display
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

    /// A login, keyed by its route and opaque issuer-local identity.
    pub fn login(login: Login, issued: &str, handle: impl Into<String>) -> Self {
        let service = login.service();
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

    /// A declared endpoint's sole account, keyed by `{NAME}_API_KEY`.
    pub(crate) fn declared(name: &str) -> Self {
        let name = ServiceName::declared(name).unwrap();
        Self::of_service(Service::declared(
            name.clone(),
            format!("https://{name}.example/v1/"),
            AdapterKind::OpenAI,
            Auth::Key(key_var(&name)),
        ))
    }
}

/// Separates a service from the handle of one of its accounts.
const HANDLE_SEPARATOR: &str = " · ";

/// Name one account among the accounts present.
///
/// The service alone when its handle is the service's name — every key-bearing
/// service. Otherwise the service and handle both, so a lone login keeps its
/// handle and never reads as a bare service name beside another login. Equal
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
/// openai-chatgpt · alex@bristol.ac.uk
/// openai-codex · alex@work · openai-codex:acct-2
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

/// The built-in table: ten key-bearing services followed by sign-in routes.
pub fn built_in_services() -> Vec<Service> {
    let keyed =
        |name, endpoint: Option<&str>, model_protocol, env, billing, meter, hosted_search| {
            Service {
                name: ServiceName::built_in(name),
                endpoint: endpoint.map(str::to_string),
                model_protocol,
                hosted_search,
                auth: Auth::Key(String::from(env)),
                billing,
                routes: false,
                meter,
            }
        };
    let mut services = vec![
        keyed(
            "anthropic",
            None,
            ModelProtocol::Fixed(AdapterKind::Anthropic),
            "ANTHROPIC_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
            HostedSearch::Native,
        ),
        keyed(
            "openai",
            None,
            ModelProtocol::OpenAI,
            "OPENAI_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
            HostedSearch::Native,
        ),
        Service {
            routes: true,
            ..keyed(
                "openrouter",
                Some("https://openrouter.ai/api/v1/"),
                ModelProtocol::Fixed(AdapterKind::OpenAI),
                "OPENROUTER_API_KEY",
                Billing::Metered,
                Meter::OpenRouterBudget,
                HostedSearch::None,
            )
        },
        keyed(
            "deepseek",
            None,
            ModelProtocol::Fixed(AdapterKind::DeepSeek),
            "DEEPSEEK_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
            HostedSearch::None,
        ),
        keyed(
            "gemini",
            None,
            ModelProtocol::Fixed(AdapterKind::Gemini),
            "GEMINI_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
            HostedSearch::Native,
        ),
        // opencode issues one key per account; the endpoint alone tells Zen from Go.
        keyed(
            "opencode-zen",
            Some("https://opencode.ai/zen/v1/"),
            ModelProtocol::Fixed(AdapterKind::OpenAI),
            "OPENCODE_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
            HostedSearch::None,
        ),
        keyed(
            "opencode-go",
            Some("https://opencode.ai/zen/go/v1/"),
            ModelProtocol::Fixed(AdapterKind::OpenAI),
            "OPENCODE_API_KEY",
            Billing::FlatRate,
            Meter::Unpublished,
            HostedSearch::None,
        ),
        keyed(
            "xai",
            None,
            ModelProtocol::Fixed(AdapterKind::Xai),
            "XAI_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
            HostedSearch::None,
        ),
        keyed(
            "qwen",
            Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1/"),
            ModelProtocol::Fixed(AdapterKind::OpenAI),
            "DASHSCOPE_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
            HostedSearch::None,
        ),
        keyed(
            "meta",
            Some("https://api.meta.ai/v1/"),
            ModelProtocol::Fixed(AdapterKind::OpenAIResp),
            "META_API_KEY",
            Billing::Metered,
            Meter::Unpublished,
            HostedSearch::None,
        ),
    ];
    services.extend(Login::ALL.map(Login::service));
    services
}

pub fn built_in(name: &ServiceName) -> Option<Service> {
    built_in_services().into_iter().find(|s| &s.name == name)
}

/// The service a scripted test backend answers to. Not a row of the table: it
/// must never appear in a picker.
pub fn scripted_service() -> Service {
    Service {
        name: ServiceName::built_in("scripted"),
        endpoint: None,
        model_protocol: ModelProtocol::Fixed(AdapterKind::OpenAIResp),
        hosted_search: HostedSearch::None,
        auth: Auth::Keyless,
        billing: Billing::Metered,
        routes: false,
        meter: Meter::Unpublished,
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

/// The protocol keywords a window offers, most familiar first — the one table
/// above, read as a list.
pub fn protocols() -> Vec<&'static str> {
    PROTOCOL_ADAPTERS.iter().map(|(name, _)| *name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_model_split_survives_service_renaming() {
        let mut service = Account::built_in("openai").service;
        service.name = ServiceName::declared("renamed").unwrap();
        let protocol = service.model_protocol;
        assert_eq!(protocol.default_adapter(), AdapterKind::OpenAIResp);
        assert_eq!(protocol.adapter_for_model("gpt-4.1"), AdapterKind::OpenAI);
        assert_eq!(
            protocol.adapter_for_model("gpt-5.5"),
            AdapterKind::OpenAIResp
        );
        for model in ["claude-opus-4", "gemini-2.5-pro", "unknown-model"] {
            assert_eq!(protocol.adapter_for_model(model), AdapterKind::OpenAIResp);
        }
        let mut fixed = Account::built_in("deepseek").service;
        fixed.name = ServiceName::declared("openai").unwrap();
        assert_eq!(
            fixed.model_protocol.adapter_for_model("gpt-5.5"),
            AdapterKind::DeepSeek
        );
    }

    #[test]
    fn meta_uses_fixed_responses_and_metered_key_policy() {
        let service = Account::built_in("meta").service;
        assert_eq!(service.endpoint.as_deref(), Some("https://api.meta.ai/v1/"));
        assert_eq!(service.auth, Auth::Key("META_API_KEY".into()));
        assert_eq!(
            service.model_protocol,
            ModelProtocol::Fixed(AdapterKind::OpenAIResp)
        );
        for model in ["gpt-4.1", "claude-opus-4", "unfamiliar-meta-model"] {
            assert_eq!(
                service.model_protocol.adapter_for_model(model),
                AdapterKind::OpenAIResp
            );
        }
        assert_eq!(service.billing, Billing::Metered);
        assert_eq!(service.meter, Meter::Unpublished);
        assert!(!service.routes);
        assert_eq!(service.hosted_search, HostedSearch::None);
    }

    #[test]
    fn declarations_name_fixed_protocols_only() {
        for (name, adapter) in PROTOCOL_ADAPTERS {
            assert_eq!(
                ModelProtocol::Fixed(*adapter).declaration_protocol(),
                Some(*name)
            );
        }
        assert_eq!(ModelProtocol::OpenAI.declaration_protocol(), None);
        assert_eq!(
            ModelProtocol::Fixed(AdapterKind::Gemini).declaration_protocol(),
            None
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
    fn two_accounts_on_one_email_draw_two_distinguishable_labels() {
        let personal = Account::login(Login::Chatgpt, "acct-1", "alex@bristol.ac.uk");
        let work = Account::login(Login::Chatgpt, "acct-2", "alex@bristol.ac.uk (Acme Ltd)");
        let among = [personal.clone(), work.clone()];
        assert_ne!(label(&personal, &among), label(&work, &among));

        // Handles that stayed identical fall back to the ids, which cannot collide.
        let twin = Account::login(Login::Chatgpt, "acct-2", "alex@bristol.ac.uk");
        let among = [personal.clone(), twin.clone()];
        assert_ne!(label(&personal, &among), label(&twin, &among));
        assert!(
            label(&twin, &among).ends_with("openai-chatgpt:acct-2"),
            "{}",
            label(&twin, &among)
        );
    }

    /// The claims a handle is built from are issuer- and workspace-supplied,
    /// so one can embed the separator and even a sibling's whole qualified
    /// rendering; the labels must still read apart.
    #[test]
    fn a_handle_embedding_anothers_qualified_rendering_still_reads_apart() {
        let plain = Account::login(Login::Chatgpt, "acct-1", "alex@work");
        // The twin forces `plain` onto its id-qualified form...
        let twin = Account::login(Login::Chatgpt, "acct-2", "alex@work");
        // ...which is exactly what this handle spells out.
        let imposter = Account::login(
            Login::Chatgpt,
            "acct-3",
            "alex@work · openai-chatgpt:acct-1",
        );
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
    fn a_declared_key_is_read_from_a_variable_a_shell_can_export() {
        let name = ServiceName::declared("house-llm.v2").unwrap();
        assert_eq!(key_var(&name), "HOUSE_LLM_V2_API_KEY");
    }
}
