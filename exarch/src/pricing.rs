//! Per-token pricing, fetched once per process from OpenRouter's
//! `GET /api/v1/models` and cached.
//!
//! The catalog backs every provider, not only the OpenRouter wire:
//! native Anthropic / OpenAI / DeepSeek launches reuse the same rates
//! (OR republishes the upstream cards verbatim, including the
//! Anthropic cache_write/cache_read multipliers), so one source of
//! truth replaces a hand-maintained per-model match table that went
//! stale on every model release.  Bare suffix lookups (`mercury-2`)
//! resolve to their prefixed catalog entry via [`add_bare_aliases`].

use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::OnceCell;

const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

#[derive(Clone, Copy, Default, Debug)]
pub struct ModelPricing {
    /// Dollars per token, base input rate.
    pub input: f64,
    /// Dollars per token, output rate.
    pub output: f64,
    /// Dollars per token charged on cache hits.  `0.0` when the model
    /// has no separate cache-read rate — callers fall back to `input`.
    pub cache_read: f64,
    /// Dollars per token charged when writing to the prompt cache.
    /// `0.0` when the model has no separate cache-write rate — callers
    /// fall back to `input`.
    pub cache_write: f64,
}

impl ModelPricing {
    /// Total cost in dollars for one turn given the four token counts
    /// genai surfaces.  `cache_creation` and `cache_read` are stripped
    /// from `input` before billing uncached tokens at the base rate —
    /// genai's Anthropic adapter reports `prompt_tokens` as the *sum*
    /// of all three so the same split is correct for every provider
    /// (OpenAI / DeepSeek pass `cache_creation = 0`, leaving the
    /// uncached term unchanged).  When the catalog publishes no
    /// separate cache rate, those tokens fall back to the base input
    /// rate — matching OR's own accounting for models without
    /// dedicated cache pricing.
    pub fn dollars(&self, input: u64, output: u64, cache_creation: u64, cache_read: u64) -> f64 {
        let uncached_input = input
            .saturating_sub(cache_creation)
            .saturating_sub(cache_read);
        let cw = if self.cache_write > 0.0 {
            self.cache_write
        } else {
            self.input
        };
        let cr = if self.cache_read > 0.0 {
            self.cache_read
        } else {
            self.input
        };
        uncached_input as f64 * self.input
            + cache_creation as f64 * cw
            + cache_read as f64 * cr
            + output as f64 * self.output
    }
}

/// Model capability snapshot pulled from OpenRouter's
/// `/api/v1/models` response.  `None` / empty fields mean the catalog
/// entry omitted them (or the catalog has not been fetched).
///
/// Fields we *don't* keep:
/// - `pricing` legs other than prompt/completion/cache live in
///   [`ModelPricing`].
/// - `architecture.input_modalities`, `top_provider.is_moderated`,
///   `per_request_limits`, `canonical_slug`/`name`/`description`,
///   `created` — none are wired to a current code path; add when a
///   concrete consumer exists.
// `tokenizer`, `supported_parameters` and `supports_tools` are
// scraped from the catalog but have no consumer yet — they exist so
// future code (a client-side token estimator; a pre-flight check
// before sending `tools`) doesn't need a second pass over the schema.
#[allow(dead_code)]
#[derive(Clone, Default, Debug)]
pub struct ModelCaps {
    /// Total context window in tokens (`context_length`).
    pub context_window: Option<u64>,
    /// Per-turn output cap as published by the top provider entry
    /// (`top_provider.max_completion_tokens`).
    pub max_output_tokens: Option<u32>,
    /// `architecture.tokenizer` (e.g. `"Claude"`, `"GPT"`,
    /// `"Cohere"`).  Useful for any future client-side token estimate.
    pub tokenizer: Option<String>,
    /// `supported_parameters` — list of names the model accepts on a
    /// request (e.g. `tools`, `reasoning`, `response_format`).  Empty
    /// when the catalog didn't surface one or the model is unlisted.
    /// Lets the caller fail fast on a model that doesn't admit
    /// `tools` rather than hitting a 4xx mid-stream.
    pub supported_parameters: Vec<String>,
    /// `canonical_slug` from the catalog — the provider-prefixed
    /// identifier (e.g. `anthropic/claude-opus-4-7`), often shorter
    /// than the user-supplied alias.  Useful for banner display.
    pub canonical_slug: Option<String>,
}

#[allow(dead_code)]
impl ModelCaps {
    /// Does the model advertise `tools` as a supported parameter?
    /// Returns `true` when the list is empty (unknown / catalog miss):
    /// callers shouldn't refuse tool calls just because the catalog
    /// didn't list the field — the data is informative, not a gate.
    pub fn supports_tools(&self) -> bool {
        self.supported_parameters.is_empty()
            || self.supported_parameters.iter().any(|p| p == "tools")
    }
}

/// Pricing + caps share the same `/api/v1/models` payload, so a single
/// `OnceCell` holds both keyed maps and the public `lookup`/`caps`
/// helpers read out of it.  Two separate cells would duplicate the
/// HTTP call (and tear if only one were populated).
static CATALOG: OnceCell<Snapshot> = OnceCell::const_new();

/// Populate the OpenRouter pricing and capability caches from
/// `/api/v1/models` if they haven't already been populated.  Safe to
/// call concurrently; only the first caller does the fetch.  On
/// failure (network down, OpenRouter changed the response shape, etc.)
/// both caches initialise empty so [`lookup`] / [`caps`] return `None`
/// and the renderer falls back to `—` exactly as it did before.
pub async fn ensure_loaded() {
    CATALOG
        .get_or_init(|| async { fetch().await.unwrap_or_default() })
        .await;
}

/// Return the per-token pricing for `model` if `ensure_loaded` has been
/// awaited and `model` is in the catalog.  Returns `None` otherwise.
pub fn lookup(model: &str) -> Option<ModelPricing> {
    CATALOG.get()?.prices.get(model).copied()
}

/// Return a cloned capability snapshot for `model` if the OpenRouter
/// catalog has been fetched.  `None` for native-provider models —
/// OpenRouter is the only catalog exarch pulls.  `ModelCaps` holds
/// owned `String`/`Vec` fields so this clones; the caller usually
/// pulls one or two fields and drops the rest.
pub fn caps(model: &str) -> Option<ModelCaps> {
    CATALOG.get()?.caps.get(model).cloned()
}

/// The total context window in tokens for `model`, when the OpenRouter
/// catalog has been fetched and lists it.  A targeted accessor that skips
/// the `ModelCaps` clone [`caps`] makes — the compaction trigger reads
/// only this one field, at every turn boundary.  `None` for a native
/// provider or before the catalog loads, so the caller falls back to the
/// byte heuristic; it self-heals once [`ensure_loaded`] completes.
pub fn context_window(model: &str) -> Option<u64> {
    CATALOG.get()?.caps.get(model)?.context_window
}

#[derive(Default)]
struct Snapshot {
    prices: HashMap<String, ModelPricing>,
    caps: HashMap<String, ModelCaps>,
}

async fn fetch() -> Result<Snapshot, reqwest::Error> {
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::config())
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp: ModelsResponse = client.get(MODELS_URL).send().await?.json().await?;
    Ok(build_snapshot(resp.data))
}

fn build_snapshot(data: Vec<ModelEntry>) -> Snapshot {
    let mut prices = HashMap::with_capacity(data.len());
    let mut caps = HashMap::with_capacity(data.len());
    for entry in data {
        let p = ModelPricing {
            input: parse_price(&entry.pricing.prompt),
            output: parse_price(&entry.pricing.completion),
            cache_read: entry
                .pricing
                .input_cache_read
                .as_deref()
                .map(parse_price)
                .unwrap_or(0.0),
            cache_write: entry
                .pricing
                .input_cache_write
                .as_deref()
                .map(parse_price)
                .unwrap_or(0.0),
        };
        // Skip entries whose base rates failed to parse — they'd just
        // bill at $0 and produce misleading display.  Falling through
        // to `lookup -> None` keeps the renderer honest with `—`.
        if p.input > 0.0 || p.output > 0.0 {
            prices.insert(entry.id.clone(), p);
        }
        // Caps are independent of pricing: even an entry that failed
        // pricing parse can carry usable context-window info.
        let context_window = entry.context_length;
        let max_output_tokens = entry
            .top_provider
            .as_ref()
            .and_then(|tp| tp.max_completion_tokens);
        let tokenizer = entry
            .architecture
            .as_ref()
            .and_then(|a| a.tokenizer.clone());
        let supported_parameters = entry.supported_parameters.clone();
        let canonical_slug = entry.canonical_slug.clone();
        let any = context_window.is_some()
            || max_output_tokens.is_some()
            || tokenizer.is_some()
            || !supported_parameters.is_empty()
            || canonical_slug.is_some();
        if any {
            caps.insert(
                entry.id,
                ModelCaps {
                    context_window,
                    max_output_tokens,
                    tokenizer,
                    supported_parameters,
                    canonical_slug,
                },
            );
        }
    }
    add_bare_aliases(&mut prices);
    add_bare_aliases(&mut caps);
    Snapshot { prices, caps }
}

/// OpenRouter accepts both the prefixed form (`inception/mercury-2`) and
/// the bare suffix (`mercury-2`) on the wire — its alias router resolves
/// the latter to the former.  The local catalog is keyed by the full id,
/// so a user who passes the bare name on the command line gets a lookup
/// miss and `—` for cost.  Mirror OR's behaviour by indexing each entry
/// under its bare suffix as well, but *only* when that suffix is unique
/// across the catalog: an ambiguous bare alias would silently bind to
/// whichever vendor sorted first, which is worse than a miss.
///
/// OpenRouter additionally separates a model's version with a dot
/// (`anthropic/claude-opus-4.8`), whereas the native Anthropic provider
/// names the very same model with a dash (`claude-opus-4-8`) — exarch
/// passes the native id through, so the dotted catalog key never matches.
/// Bridge the two by *also* indexing each qualifying entry under the
/// dash-normalized form of its bare suffix (every `.` replaced by `-`),
/// so `anthropic/claude-opus-4.8` is reachable as both `claude-opus-4.8`
/// and `claude-opus-4-8`.  The dash form inherits the same source-suffix
/// uniqueness guard, is generated only when the bare suffix actually
/// carries a `.`, never overwrites a literal catalog key, and is dropped
/// when two distinct dotted suffixes would collapse onto one dash key —
/// each guard for the same reason the bare alias has it: an alias that
/// could bind to the wrong rate is worse than a miss.
fn add_bare_aliases<V: Clone>(map: &mut HashMap<String, V>) {
    let mut suffix_count: HashMap<&str, usize> = HashMap::new();
    for key in map.keys() {
        if let Some((_, suffix)) = key.split_once('/') {
            *suffix_count.entry(suffix).or_default() += 1;
        }
    }
    let unique_suffix = |suffix: &str| suffix_count.get(suffix).copied() == Some(1);

    let mut dash_count: HashMap<String, usize> = HashMap::new();
    for key in map.keys() {
        if let Some((_, suffix)) = key.split_once('/') {
            if unique_suffix(suffix) && suffix.contains('.') {
                *dash_count.entry(suffix.replace('.', "-")).or_default() += 1;
            }
        }
    }

    let aliases: Vec<(String, V)> = map
        .iter()
        .flat_map(|(key, value)| {
            let suffix = match key.split_once('/') {
                Some((_, suffix)) if unique_suffix(suffix) => suffix,
                _ => return Vec::new(),
            };
            let mut out = Vec::new();
            if !map.contains_key(suffix) {
                out.push((suffix.to_string(), value.clone()));
            }
            if suffix.contains('.') {
                let dash = suffix.replace('.', "-");
                if dash_count.get(&dash).copied() == Some(1) && !map.contains_key(&dash) {
                    out.push((dash, value.clone()));
                }
            }
            out
        })
        .collect();
    map.extend(aliases);
}

/// OpenRouter posts prices as strings (in $/token) so they can carry
/// more precision than an f32 round-trip would preserve.  Anything we
/// can't parse becomes `0.0`, which is filtered out by the caller for
/// base rates and treated as "no separate rate" for cache rates.
fn parse_price(s: &str) -> f64 {
    s.parse().unwrap_or(0.0)
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    pricing: Pricing,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    top_provider: Option<TopProvider>,
    #[serde(default)]
    architecture: Option<Architecture>,
    #[serde(default)]
    supported_parameters: Vec<String>,
    #[serde(default)]
    canonical_slug: Option<String>,
}

#[derive(Deserialize)]
struct Pricing {
    prompt: String,
    completion: String,
    #[serde(default)]
    input_cache_read: Option<String>,
    #[serde(default)]
    input_cache_write: Option<String>,
}

#[derive(Deserialize)]
struct TopProvider {
    #[serde(default)]
    max_completion_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct Architecture {
    #[serde(default)]
    tokenizer: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_price_accepts_dollar_strings() {
        assert!((parse_price("0.000003") - 3e-6).abs() < 1e-12);
        assert_eq!(parse_price("0"), 0.0);
        assert_eq!(parse_price(""), 0.0);
        assert_eq!(parse_price("nonsense"), 0.0);
    }

    /// Verify the response shape matches a realistic /models payload.
    /// Anchors the deserialise against the OpenRouter contract so a
    /// breaking change there fails this test rather than silently
    /// emptying the cache at runtime.
    #[test]
    fn deserialises_minimal_models_payload() {
        let raw = r#"{
            "data": [
                {
                    "id": "openai/gpt-5.2",
                    "pricing": {
                        "prompt": "0.0000005",
                        "completion": "0.0000015"
                    }
                },
                {
                    "id": "anthropic/claude-opus-4",
                    "pricing": {
                        "prompt": "0.000015",
                        "completion": "0.000075",
                        "input_cache_read": "0.0000015",
                        "input_cache_write": "0.00001875"
                    }
                }
            ]
        }"#;
        let resp: ModelsResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.data[0].id, "openai/gpt-5.2");
        assert!(resp.data[1].pricing.input_cache_read.is_some());
    }

    /// Capability fields (context_length, top_provider.max_completion_tokens,
    /// architecture.tokenizer, supported_parameters, canonical_slug) must
    /// all be `#[serde(default)]` so a missing field on any entry doesn't
    /// nuke the whole catalog parse.
    #[test]
    fn deserialises_full_caps_payload() {
        let raw = r#"{
            "data": [
                {
                    "id": "anthropic/claude-opus-4-7",
                    "canonical_slug": "anthropic/claude-opus-4-7",
                    "context_length": 200000,
                    "pricing": {
                        "prompt": "0.000015",
                        "completion": "0.000075"
                    },
                    "top_provider": { "max_completion_tokens": 32000 },
                    "architecture": { "tokenizer": "Claude" },
                    "supported_parameters": ["tools", "reasoning", "temperature"]
                }
            ]
        }"#;
        let resp: ModelsResponse = serde_json::from_str(raw).unwrap();
        let e = &resp.data[0];
        assert_eq!(e.context_length, Some(200_000));
        assert_eq!(
            e.top_provider
                .as_ref()
                .and_then(|t| t.max_completion_tokens),
            Some(32_000)
        );
        assert_eq!(
            e.architecture.as_ref().and_then(|a| a.tokenizer.clone()),
            Some("Claude".into())
        );
        assert!(e.supported_parameters.iter().any(|p| p == "tools"));
        assert_eq!(
            e.canonical_slug.as_deref(),
            Some("anthropic/claude-opus-4-7")
        );
    }

    /// Bare suffixes (`mercury-2`) must resolve when the catalog has a
    /// single prefixed entry (`inception/mercury-2`).  OR's router
    /// accepts both forms on the wire, so the user's `--model mercury-2`
    /// produces a successful LLM call but, without aliasing, a $0 cost.
    #[test]
    fn bare_alias_resolves_unique_suffix() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("inception/mercury-2".into(), 42);
        m.insert("anthropic/claude-opus-4".into(), 99);
        add_bare_aliases(&mut m);
        assert_eq!(m.get("mercury-2"), Some(&42));
        assert_eq!(m.get("claude-opus-4"), Some(&99));
        // The original prefixed keys are still present.
        assert_eq!(m.get("inception/mercury-2"), Some(&42));
    }

    /// When two vendors publish the same bare name, the alias is
    /// ambiguous and must NOT be inserted — silently picking one would
    /// charge the wrong price.  The user has to pass the prefixed form.
    #[test]
    fn bare_alias_skips_ambiguous_suffix() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("vendor-a/foo".into(), 1);
        m.insert("vendor-b/foo".into(), 2);
        add_bare_aliases(&mut m);
        assert!(
            !m.contains_key("foo"),
            "ambiguous bare alias must not be inserted"
        );
        assert_eq!(m.get("vendor-a/foo"), Some(&1));
        assert_eq!(m.get("vendor-b/foo"), Some(&2));
    }

    /// Anthropic claude-opus-4 rates as OR publishes them: input
    /// $15/M, output $75/M, cache_write 1.25× input, cache_read 0.10×
    /// input.  Bills uncached input + cache_creation × cache_write +
    /// cache_read × cache_read + output × output, with cache tokens
    /// stripped from `input` first (genai sums all three on
    /// `prompt_tokens` for the Anthropic adapter).
    #[test]
    fn dollars_anthropic_cache_split() {
        let p = ModelPricing {
            input: 15e-6,
            output: 75e-6,
            cache_read: 1.5e-6,
            cache_write: 18.75e-6,
        };
        // 1000 prompt = 200 uncached + 300 write + 500 read; 100 output.
        let d = p.dollars(1000, 100, 300, 500);
        let expected = 200.0 * 15e-6 + 300.0 * 18.75e-6 + 500.0 * 1.5e-6 + 100.0 * 75e-6;
        assert!((d - expected).abs() < 1e-12, "got {d}, expected {expected}");
    }

    /// Models without separate cache rates bill cache tokens at the
    /// base input rate.  The fallback must NOT silently bill cache
    /// tokens at $0 — that would systematically underreport cost on
    /// every OpenAI-family turn that hits the prompt cache.
    #[test]
    fn dollars_falls_back_to_input_when_cache_rates_absent() {
        let p = ModelPricing {
            input: 2e-6,
            output: 8e-6,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        let d = p.dollars(1000, 100, 0, 400);
        // 600 uncached × 2e-6 + 0 × 2e-6 + 400 × 2e-6 + 100 × 8e-6
        // = (600 + 400) × 2e-6 + 100 × 8e-6 = 2e-3 + 8e-4 = 2.8e-3
        let expected = 1000.0 * 2e-6 + 100.0 * 8e-6;
        assert!((d - expected).abs() < 1e-12, "got {d}, expected {expected}");
    }

    /// A bare key that already exists in the catalog (some vendor
    /// publishes `<name>` with no prefix) must not be overwritten by an
    /// alias from a prefixed entry — the literal entry wins.
    #[test]
    fn bare_alias_does_not_overwrite_existing_bare_entry() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("foo".into(), 10);
        m.insert("vendor/foo".into(), 20);
        add_bare_aliases(&mut m);
        assert_eq!(m.get("foo"), Some(&10));
    }

    /// OpenRouter separates a version with a dot
    /// (`anthropic/claude-opus-4.8`); the native Anthropic provider uses
    /// a dash (`claude-opus-4-8`) for the same model.  A unique dotted
    /// suffix must resolve under its dotted bare form, its dash form, AND
    /// the original prefixed key — otherwise every modern native-Anthropic
    /// launch bills at $0.
    #[test]
    fn dash_alias_resolves_unique_dotted_suffix() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("anthropic/claude-opus-4.8".into(), 7);
        add_bare_aliases(&mut m);
        assert_eq!(m.get("claude-opus-4.8"), Some(&7));
        assert_eq!(m.get("claude-opus-4-8"), Some(&7));
        assert_eq!(m.get("anthropic/claude-opus-4.8"), Some(&7));
    }

    /// An ambiguous dotted suffix is as unsafe as an ambiguous bare one:
    /// neither the dotted bare alias nor the dash alias may be inserted.
    #[test]
    fn dash_alias_skips_ambiguous_suffix() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("vendor-a/model-4.5".into(), 1);
        m.insert("vendor-b/model-4.5".into(), 2);
        add_bare_aliases(&mut m);
        assert!(!m.contains_key("model-4.5"));
        assert!(!m.contains_key("model-4-5"));
        assert_eq!(m.get("vendor-a/model-4.5"), Some(&1));
        assert_eq!(m.get("vendor-b/model-4.5"), Some(&2));
    }

    /// The dash alias must never clobber a literal catalog key already
    /// living at the dash form — the literal entry wins, exactly as the
    /// bare alias yields to a pre-existing bare entry.
    #[test]
    fn dash_alias_does_not_overwrite_existing_literal_key() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("claude-opus-4-8".into(), 10);
        m.insert("anthropic/claude-opus-4.8".into(), 20);
        add_bare_aliases(&mut m);
        assert_eq!(m.get("claude-opus-4-8"), Some(&10));
        // The dotted bare alias is still safe to add.
        assert_eq!(m.get("claude-opus-4.8"), Some(&20));
    }

    /// Two distinct dotted suffixes can normalize to the same dash key
    /// (`a-1.0` and `a.1-0` both → `a-1-0`).  That collision is ambiguous,
    /// so neither contributes a dash alias — though each keeps its own
    /// unique dotted bare alias.
    #[test]
    fn dash_alias_skips_colliding_normalized_key() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("vendor/model-1.0".into(), 1);
        m.insert("vendor/model.1-0".into(), 2);
        add_bare_aliases(&mut m);
        assert!(!m.contains_key("model-1-0"));
        assert_eq!(m.get("model-1.0"), Some(&1));
        assert_eq!(m.get("model.1-0"), Some(&2));
    }
}
