//! The upstreams a model is served via.
//!
//! A routing service fronts many upstreams of one model and, left alone,
//! chooses among them per request by its own lights. A [`Via`] is the human's
//! choice instead: endpoint tags in order of preference, sent with fallbacks
//! off.

use super::models::ProviderEndpoint;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Endpoint tags, most preferred first: never empty, none empty, none twice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct Via(Vec<String>);

impl Via {
    /// One upstream: what the picker's via row chooses.
    pub fn one(tag: String) -> Self {
        Self(vec![tag])
    }

    /// Comma-separated tags, as `--via` takes them.
    ///
    /// # Errors
    /// As [`TryFrom`].
    pub fn parse(s: &str) -> Result<Self, String> {
        Self::try_from(s.split(',').map(str::to_owned).collect::<Vec<_>>())
    }

    /// The tags, most preferred first.
    pub fn tags(&self) -> &[String] {
        &self.0
    }

    /// The window every upstream holds: the least of the tags' endpoint
    /// windows, an endpoint listing none counting as `listed` (the model's own
    /// window); unknown when any is unknown.
    ///
    /// # Errors
    /// If `endpoints` does not list a tag, naming it and the tags listed.
    pub(crate) fn window(
        &self,
        endpoints: &[ProviderEndpoint],
        listed: Option<u64>,
    ) -> Result<Option<u64>, String> {
        let windows = self
            .0
            .iter()
            .map(|tag| {
                endpoints
                    .iter()
                    .find(|endpoint| &endpoint.tag == tag)
                    .map(|endpoint| endpoint.context_length.or(listed))
                    .ok_or_else(|| unlisted(tag, endpoints))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(windows
            .into_iter()
            .collect::<Option<Vec<u64>>>()
            .and_then(|windows| windows.into_iter().min()))
    }
}

/// The refusal of `tag`, which `endpoints` does not list, naming those it does.
fn unlisted(tag: &str, endpoints: &[ProviderEndpoint]) -> String {
    if endpoints.is_empty() {
        return format!("'{tag}' is not a listed upstream; none are listed");
    }
    let tags: Vec<_> = endpoints.iter().map(|e| e.tag.as_str()).collect();
    format!(
        "'{tag}' is not a listed upstream; the listed ones are {}",
        tags.join(", ")
    )
}

impl TryFrom<Vec<String>> for Via {
    type Error = String;

    /// Refuses no tags, an empty tag, and a tag given twice. Tags are taken
    /// as given: one with spaces is simply not listed.
    fn try_from(tags: Vec<String>) -> Result<Self, String> {
        if tags.is_empty() {
            return Err("a via names at least one upstream".into());
        }
        if tags.iter().any(String::is_empty) {
            return Err("an upstream tag is empty".into());
        }
        let mut seen = BTreeSet::new();
        if let Some(tag) = tags.iter().find(|tag| !seen.insert(*tag)) {
            return Err(format!("upstream '{tag}' is given twice"));
        }
        Ok(Self(tags))
    }
}

impl From<Via> for Vec<String> {
    fn from(via: Via) -> Self {
        via.0
    }
}

/// The tags joined with `,`, as `--via` takes them.
impl std::fmt::Display for Via {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(tag: &str, window: Option<u64>) -> ProviderEndpoint {
        ProviderEndpoint {
            provider_name: tag.into(),
            tag: tag.into(),
            context_length: window,
            quantization: None,
        }
    }

    #[test]
    fn parse_keeps_the_order_given() {
        let via = Via::parse("novita,deepinfra/fp8").unwrap();
        assert_eq!(via.tags(), ["novita", "deepinfra/fp8"]);
        assert_eq!(via.to_string(), "novita,deepinfra/fp8");
    }

    #[test]
    fn an_empty_or_repeated_tag_is_refused() {
        for empty in ["", "a,,b", "a,"] {
            assert_eq!(
                Via::parse(empty),
                Err("an upstream tag is empty".into()),
                "{empty}"
            );
        }
        assert_eq!(
            Via::parse("a,b,a"),
            Err("upstream 'a' is given twice".into())
        );
        assert_eq!(
            Via::try_from(Vec::new()),
            Err("a via names at least one upstream".into())
        );
    }

    #[test]
    fn a_via_is_a_plain_json_array() {
        let via = Via::parse("a,b").unwrap();
        let json = serde_json::to_value(&via).unwrap();
        assert_eq!(json, serde_json::json!(["a", "b"]));
        assert_eq!(serde_json::from_value::<Via>(json).unwrap(), via);
        assert!(serde_json::from_value::<Via>(serde_json::json!([])).is_err());
    }

    #[test]
    fn the_window_is_the_least_its_upstreams_hold() {
        let endpoints = [
            endpoint("a", Some(200)),
            endpoint("b", Some(100)),
            endpoint("bare", None),
        ];
        let window = |tags| Via::parse(tags).unwrap().window(&endpoints, Some(150));
        assert_eq!(window("a,b"), Ok(Some(100)));
        assert_eq!(window("a,bare"), Ok(Some(150)));
        assert_eq!(
            Via::parse("a,bare").unwrap().window(&endpoints, None),
            Ok(None)
        );
    }

    #[test]
    fn an_unlisted_tag_is_refused_naming_the_listed_ones() {
        let endpoints = [endpoint("a", Some(200)), endpoint("b", None)];
        let via = Via::parse("a,c").unwrap();
        assert_eq!(
            via.window(&endpoints, None),
            Err("'c' is not a listed upstream; the listed ones are a, b".into())
        );
        assert_eq!(
            via.window(&[], None),
            Err("'a' is not a listed upstream; none are listed".into())
        );
    }
}
