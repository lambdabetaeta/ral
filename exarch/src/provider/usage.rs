//! Token counts and dollar cost for one turn, and the shared rendering of both.

use genai::adapter::AdapterKind;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Default, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Usage {
    /// genai's `prompt_tokens`, which already includes both cache counts below;
    /// `ModelPricing::dollars` subtracts them before billing the remainder.
    pub input: u64,
    pub output: u64,
    /// `None` when the provider reports no cache figures; printed `—`, not `0`.
    pub cache_creation: Option<u64>,
    pub cache_read: Option<u64>,
    pub dollars: f64,
    pub unmetered: bool,
    /// Turns whose usage the provider never reported (a stream cut short, an
    /// `End` without usage); the other fields cover only the rest, so a total
    /// with any is a lower bound.
    pub unreported: u32,
}

/// Sum two per-turn counts, yielding `None` only when both sides are: a total
/// must not fold "never reported" into a reported zero, which [`Usage::parts`]
/// would then print as `0` rather than `—`.
fn add_opt_u64(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(x), Some(y)) => Some(x + y),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_creation = add_opt_u64(self.cache_creation, rhs.cache_creation);
        self.cache_read = add_opt_u64(self.cache_read, rhs.cache_read);
        self.dollars += rhs.dollars;
        // A total spanning a provider switch reads unmetered if any turn was,
        // so `parts` says "subscription" over real dollars from the metered ones.
        self.unmetered = self.unmetered || rhs.unmetered;
        self.unreported += rhs.unreported;
    }
}

/// Humanise a token count with the one format shared by every surface.
pub fn humanize_tokens(n: u64) -> String {
    // Just under a million: otherwise the thousands branch rounds 999_999 to
    // "1000.0k".
    if n >= 999_950 {
        #[allow(
            clippy::cast_precision_loss,
            reason = "display rounding of a token count; magnitude far below 2^52"
        )]
        let millions = n as f64 / 1_000_000.0;
        format!("{millions:.1}m")
    } else if n >= 10_000 {
        #[allow(
            clippy::cast_precision_loss,
            reason = "display rounding of a token count; magnitude far below 2^52"
        )]
        let thousands = n as f64 / 1_000.0;
        let formatted = format!("{thousands:.1}");
        format!("{}k", formatted.strip_suffix(".0").unwrap_or(&formatted))
    } else {
        n.to_string()
    }
}

/// The humanised pieces of a usage line: `Display` below joins them for the
/// log, `tui::line::usage_text` styles the same pieces for the status bar.
pub struct UsageParts {
    pub input: String,
    pub output: String,
    /// `(write, read)`; `None` hides the cache segment entirely.
    pub cache: Option<(String, String)>,
    pub cost: String,
}

impl Usage {
    /// A turn whose usage never arrived.
    pub(crate) fn unreported(metered: bool) -> Self {
        Self {
            unmetered: !metered,
            unreported: 1,
            ..Self::default()
        }
    }

    /// The input count as a context measurement; `None` when it was never reported.
    pub fn measured_input(&self) -> Option<u64> {
        (self.unreported == 0).then_some(self.input)
    }

    pub fn parts(&self) -> UsageParts {
        let field = |value: Option<u64>| match value {
            Some(n) => humanize_tokens(n),
            None => "—".into(),
        };
        // A reported zero hides the segment too: no cache traffic, nothing to say.
        let show_cache = matches!(self.cache_creation, Some(n) if n > 0)
            || matches!(self.cache_read, Some(n) if n > 0);
        let cost = if self.unmetered {
            "subscription".into()
        } else {
            let known = if self.dollars > 0.0 {
                format!("${:.4}", self.dollars)
            } else {
                // Metered yet costing nothing means `pricing::lookup_for` found
                // no rate for the model, not that the turn was free.
                "—".into()
            };
            if self.unreported > 0 {
                format!("{known} + ?")
            } else {
                known
            }
        };
        UsageParts {
            input: humanize_tokens(self.input),
            output: humanize_tokens(self.output),
            cache: show_cache.then(|| (field(self.cache_creation), field(self.cache_read))),
            cost,
        }
    }
}

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts = self.parts();
        write!(f, "total {} in / {} out", parts.input, parts.output)?;
        if let Some((write, read)) = &parts.cache {
            write!(f, " [{write} wr/{read} rd]")?;
        }
        write!(f, " · {}", parts.cost)
    }
}

/// A turn's [`Usage`] from genai's raw counts, priced only when `metered` (as
/// `Transport::metered` decides): a subscription turn reports tokens but never
/// a cost.  A negative raw count — a provider bug — clamps to `0`.
pub(super) fn usage_from(
    model: &str,
    raw: &genai::chat::Usage,
    metered: bool,
    adapter: AdapterKind,
) -> Usage {
    let input = u64::try_from(raw.prompt_tokens.unwrap_or(0)).unwrap_or(0);
    let output = u64::try_from(raw.completion_tokens.unwrap_or(0)).unwrap_or(0);
    let cache_creation = raw
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cache_creation_tokens)
        .map(|n| u64::try_from(n).unwrap_or(0));
    let cache_read = raw
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .map(|n| u64::try_from(n).unwrap_or(0));
    let dollars = if metered {
        super::pricing::lookup_for(model, adapter).map_or(0.0, |pricing| {
            pricing.dollars(
                input,
                output,
                cache_creation.unwrap_or(0),
                cache_read.unwrap_or(0),
            )
        })
    } else {
        0.0
    };
    Usage {
        input,
        output,
        cache_creation,
        cache_read,
        dollars,
        unmetered: !metered,
        unreported: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_parts_unmetered_reads_subscription() {
        let usage = Usage {
            input: 1_000,
            output: 50,
            unmetered: true,
            ..Usage::default()
        };
        assert_eq!(usage.parts().cost, "subscription");
        assert_eq!(usage.to_string(), "total 1000 in / 50 out · subscription");
    }

    #[test]
    fn usage_display_omits_empty_cache_suffix() {
        assert_eq!(Usage::default().to_string(), "total 0 in / 0 out · —");
        let measured_zero = Usage {
            input: 100,
            output: 50,
            cache_creation: Some(0),
            cache_read: Some(0),
            ..Usage::default()
        };
        assert_eq!(measured_zero.to_string(), "total 100 in / 50 out · —");
    }

    #[test]
    fn usage_add_assign_propagates_some() {
        let mut total = Usage::default();
        total += Usage {
            input: 1_000,
            output: 50,
            cache_read: Some(800),
            ..Usage::default()
        };
        total += Usage {
            input: 1_500,
            output: 60,
            cache_read: Some(1_200),
            ..Usage::default()
        };
        total += Usage {
            input: 500,
            output: 10,
            cache_creation: Some(42),
            ..Usage::default()
        };
        assert_eq!(total.input, 3_000);
        assert_eq!(total.cache_creation, Some(42));
        assert_eq!(total.cache_read, Some(2_000));
    }

    #[test]
    fn unreported_adds_and_voids_the_measurement() {
        let mut total = Usage {
            input: 1_000,
            dollars: 0.0364,
            ..Usage::default()
        };
        assert_eq!(total.measured_input(), Some(1_000));
        total += Usage::unreported(true);
        total += Usage::unreported(true);
        assert_eq!(total.unreported, 2);
        assert_eq!(total.input, 1_000);
        assert_eq!(total.measured_input(), None);
        assert_eq!(Usage::unreported(true).measured_input(), None);
    }

    #[test]
    fn unreported_cost_is_a_lower_bound() {
        let priced = Usage {
            dollars: 0.0364,
            unreported: 1,
            ..Usage::default()
        };
        assert_eq!(priced.parts().cost, "$0.0364 + ?");
        assert_eq!(Usage::unreported(true).parts().cost, "— + ?");
        assert_eq!(Usage::unreported(false).parts().cost, "subscription");
    }
}
