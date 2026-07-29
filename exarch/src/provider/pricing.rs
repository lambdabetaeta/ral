//! Per-token pricing and model capabilities, fetched once per process from
//! `OpenRouter`'s `GET /api/v1/models` and cached.
//!
//! OR republishes the upstream cards verbatim, so the one catalog prices
//! Anthropic, `OpenAI` and the OR wire alike.  `DeepSeek` is the exception:
//! OR publishes its generic aliases at $0, so native `DeepSeek` traffic bills
//! off the hardcoded table in `deepseek_price`.

use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::OnceCell;

const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// Dollars per *token*, not the per-million figures the vendors publish.  A
/// `0.0` cache rate means the catalog carried none, and `dollars` bills those
/// tokens at `input` instead.
#[derive(Clone, Copy, Default, Debug)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl ModelPricing {
    /// Cost of one turn from the four counts genai surfaces.  genai reports
    /// `prompt_tokens` as the *sum* of uncached, cache-creation and cache-read
    /// tokens, so the two cache counts come off `input` before it bills at the
    /// base rate; a provider that never caches passes zeros and the uncached
    /// term is unchanged.
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
        #[allow(
            clippy::cast_precision_loss,
            reason = "token counts are bounded far below 2^52; f64 represents them exactly"
        )]
        let cost = (uncached_input as f64).mul_add(
            self.input,
            (cache_creation as f64).mul_add(
                cw,
                (cache_read as f64).mul_add(cr, output as f64 * self.output),
            ),
        );
        cost
    }
}

/// What the catalog says a model can do.  A `None` or empty field means the
/// entry omitted it, or the catalog has not been fetched.
#[derive(Clone, Default, Debug)]
pub struct ModelCaps {
    pub context_window: Option<u64>,
    /// Request knobs the model accepts: `tools`, `reasoning`, `temperature`…
    pub supported_parameters: Vec<String>,
}

impl ModelCaps {
    /// Does the model advertise `param`?  An empty list is a catalog miss, not
    /// a denial, so it answers `true` — the picker's knobs and synod's effort
    /// mask stay open on a model the catalog simply never listed.
    pub fn supports(&self, param: &str) -> bool {
        self.supported_parameters.is_empty() || self.supported_parameters.iter().any(|p| p == param)
    }
}

/// Prices and caps ride one payload, so one cell holds both maps: two cells
/// would fetch twice, and could tear with only one of them populated.
static CATALOG: OnceCell<Snapshot> = OnceCell::const_new();

/// Fetch the catalog, once per process; concurrent callers share the one
/// fetch.
///
/// A failed fetch caches an *empty* snapshot rather than retrying, so every
/// lookup stays `None` for the life of the process: `Usage::parts` prints `—`
/// for cost and the status line drops its ctx segment.
pub async fn ensure_loaded() {
    CATALOG
        .get_or_init(|| async { fetch().await.unwrap_or_default() })
        .await;
}

/// Catalog pricing for `model`, `None` before [`ensure_loaded`] finishes or on
/// a miss.  A caller that knows the wire adapter wants [`lookup_for`], which
/// routes `DeepSeek` away from the catalog.
pub(crate) fn lookup(model: &str) -> Option<ModelPricing> {
    CATALOG.get()?.prices.get(model).copied()
}

/// One side (regular or peak) of a `DeepSeek` rate card, in dollars per
/// 1M tokens.
#[derive(Clone, Copy)]
struct DeepSeekRates {
    input: f64,
    output: f64,
    cache_read: f64,
}

/// `DeepSeek`'s peak-pricing windows, 01:00-04:00 and 06:00-10:00, on `hour`
/// in 0..=23 UTC.  Both ends are half-open, as `DeepSeek` states them.
fn is_peak_hour(hour: i8) -> bool {
    (1..4).contains(&hour) || (6..10).contains(&hour)
}

/// `DeepSeek`'s own API rates, which double inside the peak windows.  OR
/// publishes many `DeepSeek` aliases at $0 and `build_snapshot` drops
/// zero-rate entries, so the catalog would price this traffic at nothing.
fn deepseek_price(model: &str) -> Option<ModelPricing> {
    // `deepseek-chat` and `deepseek-reasoner` are the non-thinking and
    // thinking faces of `deepseek-v4-flash`, priced alike.
    const FLASH: DeepSeekRates = DeepSeekRates {
        input: 0.14,
        output: 0.28,
        cache_read: 0.0028,
    };
    const FLASH_PEAK: DeepSeekRates = DeepSeekRates {
        input: 0.28,
        output: 0.56,
        cache_read: 0.0056,
    };
    const PRO: DeepSeekRates = DeepSeekRates {
        input: 0.435,
        output: 0.87,
        cache_read: 0.003_625,
    };
    const PRO_PEAK: DeepSeekRates = DeepSeekRates {
        input: 0.87,
        output: 1.74,
        cache_read: 0.00725,
    };

    let peak = is_peak_hour(
        jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::UTC)
            .hour(),
    );
    let r = match model {
        "deepseek-chat" | "deepseek-reasoner" | "deepseek-v4-flash" => {
            if peak {
                FLASH_PEAK
            } else {
                FLASH
            }
        }
        "deepseek-v4-pro" => {
            if peak {
                PRO_PEAK
            } else {
                PRO
            }
        }
        _ => return None,
    };
    Some(ModelPricing {
        input: r.input / 1_000_000.0,
        output: r.output / 1_000_000.0,
        cache_read: r.cache_read / 1_000_000.0,
        cache_write: 0.0,
    })
}

/// Pricing for `model` from whichever source is authoritative for `adapter`:
/// the native rates first for `DeepSeek`, the catalog for everyone else.
pub fn lookup_for(model: &str, adapter: genai::adapter::AdapterKind) -> Option<ModelPricing> {
    if adapter == genai::adapter::AdapterKind::DeepSeek {
        deepseek_price(model).or_else(|| lookup(model))
    } else {
        lookup(model)
    }
}

/// Capabilities for `model`, cloned out of the catalog.
pub fn caps(model: &str) -> Option<ModelCaps> {
    CATALOG.get()?.caps.get(model).cloned()
}

/// [`caps`], defaulting on a miss — an unlisted model, or a call before the
/// catalog loads.
///
/// A native id is not a miss: `add_bare_aliases` bridges it to the
/// OR-fronted entry for the same model, which carries the same card.
pub fn caps_or_default(model: &str) -> ModelCaps {
    caps(model).unwrap_or_default()
}

/// The context window for `model`, skipping the [`ModelCaps`] clone [`caps`]
/// makes.
///
/// `agent::deliberate` reads this one field at every turn boundary to decide
/// whether to compact, and falls back to its byte heuristic on `None`.
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
        .use_preconfigured_tls(crate::provider::tls::config())
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
                .map_or(0.0, parse_price),
            cache_write: entry
                .pricing
                .input_cache_write
                .as_deref()
                .map_or(0.0, parse_price),
        };
        // An entry whose base rates didn't parse would bill at $0; leaving it
        // out makes it a miss instead, which renders honestly as `—`.
        if p.input > 0.0 || p.output > 0.0 {
            prices.insert(entry.id.clone(), p);
        }
        // Caps outlive a failed pricing parse — the window is still usable.
        let context_window = entry.context_length;
        let supported_parameters = entry.supported_parameters.clone();
        let any = context_window.is_some() || !supported_parameters.is_empty();
        if any {
            caps.insert(
                entry.id,
                ModelCaps {
                    context_window,
                    supported_parameters,
                },
            );
        }
    }
    add_bare_aliases(&mut prices);
    add_bare_aliases(&mut caps);
    Snapshot { prices, caps }
}

/// Index each entry under its bare suffix too (`inception/mercury-2` →
/// `mercury-2`), the second spelling OR's router accepts on the wire, and — for
/// a dotted suffix — under its dash form (`anthropic/claude-opus-4.8` →
/// `claude-opus-4-8`), the name the native Anthropic provider gives the same
/// model.  An alias lands only when nothing else can claim its key: the source
/// suffix must be unique across the catalog, a dash key must not tie any other
/// alias, and a literal catalog key always wins.  Binding to the wrong
/// vendor's rate is worse than the miss.
fn add_bare_aliases<V: Clone>(map: &mut HashMap<String, V>) {
    let mut suffix_count: HashMap<&str, usize> = HashMap::new();
    for key in map.keys() {
        if let Some((_, suffix)) = key.split_once('/') {
            *suffix_count.entry(suffix).or_default() += 1;
        }
    }
    let unique_suffix = |suffix: &str| suffix_count.get(suffix).copied() == Some(1);

    // Both sources a dash key can come from: every plain bare suffix, and the
    // normalized form of every dotted one.  A dash key that ties is dropped.
    let mut dash_count: HashMap<String, usize> = HashMap::new();
    for key in map.keys() {
        if let Some((_, suffix)) = key.split_once('/')
            && unique_suffix(suffix)
        {
            *dash_count.entry(suffix.to_string()).or_default() += 1;
            if suffix.contains('.') {
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

/// `OpenRouter` posts prices as decimal strings in $/token.  Anything
/// unparseable becomes `0.0`: dropped by `build_snapshot` for a base rate,
/// read as "no separate rate" for a cache rate.
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
    supported_parameters: Vec<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_peak_hour_matches_documented_windows() {
        for h in 0i8..24 {
            let expected = matches!(h, 1..=3 | 6..=9);
            assert_eq!(is_peak_hour(h), expected, "hour {h}");
        }
    }

    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "exact sentinel 0.0 returned by parse_price on the empty/invalid path"
    )]
    fn parse_price_accepts_dollar_strings() {
        assert!((parse_price("0.000003") - 3e-6).abs() < 1e-12);
        assert_eq!(parse_price("0"), 0.0);
        assert_eq!(parse_price(""), 0.0);
        assert_eq!(parse_price("nonsense"), 0.0);
    }

    /// Pins the wire shape, so a change at OR fails here rather than silently
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

    /// Both cap fields default, so one entry omitting them cannot fail the
    /// whole catalog parse.
    #[test]
    fn deserialises_full_caps_payload() {
        let raw = r#"{
            "data": [
                {
                    "id": "anthropic/claude-opus-4-7",
                    "context_length": 200000,
                    "pricing": {
                        "prompt": "0.000015",
                        "completion": "0.000075"
                    },
                    "supported_parameters": ["tools", "reasoning", "temperature"]
                }
            ]
        }"#;
        let resp: ModelsResponse = serde_json::from_str(raw).unwrap();
        let e = &resp.data[0];
        assert_eq!(e.context_length, Some(200_000));
        assert!(e.supported_parameters.iter().any(|p| p == "tools"));
    }

    /// Without the alias, `--model mercury-2` makes a successful call that
    /// costs $0 on the banner.
    #[test]
    fn bare_alias_resolves_unique_suffix() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("inception/mercury-2".into(), 42);
        m.insert("anthropic/claude-opus-4".into(), 99);
        add_bare_aliases(&mut m);
        assert_eq!(m.get("mercury-2"), Some(&42));
        assert_eq!(m.get("claude-opus-4"), Some(&99));
        assert_eq!(m.get("inception/mercury-2"), Some(&42));
    }

    /// Two vendors, one bare name: picking either would charge the wrong
    /// price, so the user must pass the prefixed form.
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

    /// Anthropic claude-opus-4 rates as OR publishes them: $15/M in, $75/M
    /// out, cache write 1.25× and cache read 0.10× the input rate.
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
        #[allow(
            clippy::suboptimal_flops,
            reason = "hand-computed reference value in a test; keep the readable literal form"
        )]
        let expected = 200.0 * 15e-6 + 300.0 * 18.75e-6 + 500.0 * 1.5e-6 + 100.0 * 75e-6;
        assert!((d - expected).abs() < 1e-12, "got {d}, expected {expected}");
    }

    /// The fallback must not bill cache tokens at $0, which would underreport
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
        // The split collapses: 600 uncached and 400 read both bill at `input`.
        #[allow(
            clippy::suboptimal_flops,
            reason = "hand-computed reference value in a test; keep the readable literal form"
        )]
        let expected = 1000.0 * 2e-6 + 100.0 * 8e-6;
        assert!((d - expected).abs() < 1e-12, "got {d}, expected {expected}");
    }

    #[test]
    fn bare_alias_does_not_overwrite_existing_bare_entry() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("foo".into(), 10);
        m.insert("vendor/foo".into(), 20);
        add_bare_aliases(&mut m);
        assert_eq!(m.get("foo"), Some(&10));
    }

    /// All three spellings must resolve, or every native-Anthropic launch
    /// bills at $0.
    #[test]
    fn dash_alias_resolves_unique_dotted_suffix() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("anthropic/claude-opus-4.8".into(), 7);
        add_bare_aliases(&mut m);
        assert_eq!(m.get("claude-opus-4.8"), Some(&7));
        assert_eq!(m.get("claude-opus-4-8"), Some(&7));
        assert_eq!(m.get("anthropic/claude-opus-4.8"), Some(&7));
    }

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

    #[test]
    fn dash_alias_does_not_overwrite_existing_literal_key() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("claude-opus-4-8".into(), 10);
        m.insert("anthropic/claude-opus-4.8".into(), 20);
        add_bare_aliases(&mut m);
        assert_eq!(m.get("claude-opus-4-8"), Some(&10));
        assert_eq!(m.get("claude-opus-4.8"), Some(&20));
    }

    /// A dash alias can land on another vendor's plain bare suffix.  The plain
    /// suffix keeps its own key deterministically; without the guard the rate
    /// would follow `HashMap` iteration order.
    #[test]
    fn dash_alias_yields_to_plain_bare_suffix_collision() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("anthropic/claude-opus-4.8".into(), 7);
        m.insert("vendor/claude-opus-4-8".into(), 42);
        add_bare_aliases(&mut m);
        assert_eq!(
            m.get("claude-opus-4-8"),
            Some(&42),
            "the plain bare suffix wins its own key"
        );
        assert_eq!(
            m.get("claude-opus-4.8"),
            Some(&7),
            "the dotted bare alias still resolves to the dotted entry"
        );
    }

    /// Two dotted suffixes can normalize onto one dash key, so neither may
    /// claim it — each still keeps its own dotted bare alias.
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
