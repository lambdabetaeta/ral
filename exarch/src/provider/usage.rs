//! Usage accumulation, pricing projection, and token formatting.

use genai::adapter::AdapterKind;
use std::fmt;

#[derive(Default, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    /// Tokens written into the prompt cache, or `None` when unreported.
    pub cache_creation: Option<u64>,
    /// Tokens read from the prompt cache, or `None` when unreported.
    pub cache_read: Option<u64>,
    pub dollars: f64,
    /// Whether the turn belongs to a flat subscription.
    pub unmetered: bool,
}

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
        self.unmetered = self.unmetered || rhs.unmetered;
    }
}

/// Humanise a token count with the one format shared by every surface.
pub fn humanize_tokens(n: u64) -> String {
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

/// The humanised pieces of a usage line.
pub struct UsageParts {
    pub input: String,
    pub output: String,
    /// `(write, read)` when the cache segment is worth showing.
    pub cache: Option<(String, String)>,
    pub cost: String,
}

impl Usage {
    pub fn parts(&self) -> UsageParts {
        let field = |value: Option<u64>| match value {
            Some(n) => humanize_tokens(n),
            None => "—".into(),
        };
        let show_cache = matches!(self.cache_creation, Some(n) if n > 0)
            || matches!(self.cache_read, Some(n) if n > 0);
        UsageParts {
            input: humanize_tokens(self.input),
            output: humanize_tokens(self.output),
            cache: show_cache.then(|| (field(self.cache_creation), field(self.cache_read))),
            cost: if self.unmetered {
                "subscription".into()
            } else if self.dollars > 0.0 {
                format!("${:.4}", self.dollars)
            } else {
                "—".into()
            },
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
}
