//! REPL value-output styling.
//!
//! `OutputTheme` controls how the REPL renders the result of a top-level
//! expression — a `value-prefix` string (default `"=> "`) and an optional
//! `value-color` (an ANSI SGR escape).  Both fields are configurable from
//! the RC file's `theme` key.
//!
//! Color is suppressed automatically when [`ral_core::terminal::ui_color`]
//! returns false, so the theme can store an unconditional `Some(color)`.

use ral_core::ansi::{YELLOW, named_color};
use ral_core::{Map, Value};

/// Styling applied to ral-computed values printed at the REPL prompt.
#[derive(Clone, Debug)]
pub(crate) struct OutputTheme {
    /// String prepended to every printed value.  Default: `"=> "`.
    pub value_prefix: String,
    /// ANSI SGR escape for value output.  `None` suppresses color entirely.
    pub value_color: Option<String>,
}

ral_core::record!(typed OutputTheme {
    value_prefix: "value-prefix",
    value_color: "value-color",
});

impl Default for OutputTheme {
    fn default() -> Self {
        Self {
            value_prefix: "=> ".into(),
            value_color: Some(YELLOW.into()),
        }
    }
}

impl OutputTheme {
    /// Build a theme from an RC `theme:` map, starting from the default and
    /// overriding the `value-prefix` / `value-color` keys it carries.  A
    /// `value-color` names an ANSI colour (see [`named_color`]) or `none` to
    /// suppress color entirely.  An unknown key or a malformed value is
    /// rejected with an error naming it.
    pub(crate) fn from_map(pairs: &Map) -> Result<Self, String> {
        let mut theme = Self::default();
        for (k, v) in pairs {
            match k {
                "value-prefix" => match v.as_ref() {
                    Value::String(s) => theme.value_prefix = s.to_string(),
                    other => {
                        return Err(format!(
                            "theme 'value-prefix' must be a string; got {}",
                            other.type_name()
                        ));
                    }
                },
                "value-color" => match v.as_ref() {
                    Value::String(s) if s.eq_ignore_ascii_case("none") => {
                        theme.value_color = None;
                    }
                    Value::String(s) => match named_color(s.as_str()) {
                        Some(color) => theme.value_color = Some(color),
                        None => {
                            return Err(format!(
                                "theme 'value-color' must be one of black, red, green, \
                                 yellow, blue, magenta, cyan, white, or none; got '{s}'"
                            ));
                        }
                    },
                    other => {
                        return Err(format!(
                            "theme 'value-color' must be a string; got {}",
                            other.type_name()
                        ));
                    }
                },
                other => {
                    return Err(format!(
                        "theme: unknown key '{other}'; known keys are value-prefix and value-color"
                    ));
                }
            }
        }
        Ok(theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_of(pairs: Vec<(String, Value)>) -> Map {
        let Value::Map(m) = Value::map(pairs) else {
            unreachable!()
        };
        m
    }

    #[test]
    fn theme_value_color_none_suppresses_color() {
        let pairs = map_of(vec![("value-color".into(), Value::string("none"))]);
        let theme = OutputTheme::from_map(&pairs).unwrap();
        assert_eq!(theme.value_color, None);
        assert_eq!(theme.value_prefix, "=> ");
    }

    #[test]
    fn theme_value_color_unknown_name_rejected() {
        let pairs = map_of(vec![("value-color".into(), Value::string("purple"))]);
        assert!(OutputTheme::from_map(&pairs).is_err());
    }

    #[test]
    fn theme_value_color_wrong_type_rejected() {
        let pairs = map_of(vec![("value-color".into(), Value::Int(3))]);
        assert!(OutputTheme::from_map(&pairs).is_err());
    }

    #[test]
    fn theme_value_prefix_wrong_type_rejected() {
        let pairs = map_of(vec![("value-prefix".into(), Value::Int(3))]);
        assert!(OutputTheme::from_map(&pairs).is_err());
    }

    #[test]
    fn theme_unknown_key_rejected() {
        let pairs = map_of(vec![
            ("wat".into(), Value::string("x")),
            ("value-prefix".into(), Value::string("> ")),
        ]);
        assert!(OutputTheme::from_map(&pairs).unwrap_err().contains("wat"));
    }
}
