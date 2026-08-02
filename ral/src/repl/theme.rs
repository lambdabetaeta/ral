//! REPL value-output styling.
//!
//! `OutputTheme` controls how the REPL renders the result of a top-level
//! expression — a `value_prefix` string (default `"=> "`) and an optional
//! `value_color` (an ANSI SGR escape).  Both fields are configurable from
//! the RC file's `theme` key.
//!
//! Color is suppressed automatically when [`ral_core::ansi::use_ui_color`]
//! returns false, so the theme can store an unconditional `Some(color)`.
//!
//! The theme is process-global state — there is exactly one REPL per
//! process and it consults the theme from the value-printing path
//! (`repl::exec::print_result`).  Stored behind a `RwLock` so the RC
//! file can replace it once during startup without imposing locking on
//! the read path beyond a snapshot clone.

use ral_core::ansi::{YELLOW, named_color};
use ral_core::{Map, Value};
use std::sync::{LazyLock, RwLock};

/// Styling applied to ral-computed values printed at the REPL prompt.
#[derive(Clone, Debug)]
pub(crate) struct OutputTheme {
    /// String prepended to every printed value.  Default: `"=> "`.
    pub value_prefix: String,
    /// ANSI SGR escape for value output.  `None` suppresses color entirely.
    pub value_color: Option<String>,
}

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
    /// overriding the `value_prefix` / `value_color` keys it carries.  A
    /// `value_color` names an ANSI colour (see [`named_color`]) or `none` to
    /// suppress color entirely.  An unknown key warns and is ignored; a
    /// recognised key with a malformed value is rejected with an error
    /// naming the key.
    pub(crate) fn from_map(pairs: &Map) -> Result<Self, String> {
        let mut theme = Self::default();
        for (k, v) in pairs {
            match k.as_str() {
                "value_prefix" => match v {
                    Value::String(s) => theme.value_prefix.clone_from(s),
                    other => {
                        return Err(format!(
                            "rc theme 'value_prefix' must be a string; got {}",
                            other.type_name()
                        ));
                    }
                },
                "value_color" => match v {
                    Value::String(s) if s.eq_ignore_ascii_case("none") => {
                        theme.value_color = None;
                    }
                    Value::String(s) => match named_color(s) {
                        Some(color) => theme.value_color = Some(color),
                        None => {
                            return Err(format!(
                                "rc theme 'value_color' must be one of black, red, green, \
                                 yellow, blue, magenta, cyan, white, or none; got '{s}'"
                            ));
                        }
                    },
                    other => {
                        return Err(format!(
                            "rc theme 'value_color' must be a string; got {}",
                            other.type_name()
                        ));
                    }
                },
                other => ral_core::diagnostic::shell_warning(&format!(
                    "ral: theme: unknown key '{other}', ignoring"
                )),
            }
        }
        Ok(theme)
    }
}

static OUTPUT_THEME: LazyLock<RwLock<OutputTheme>> =
    LazyLock::new(|| RwLock::new(OutputTheme::default()));

/// Replace the active output theme.  Called once after the RC file is loaded.
pub(crate) fn set_output_theme(theme: OutputTheme) {
    if let Ok(mut g) = OUTPUT_THEME.write() {
        *g = theme;
    }
}

/// Return a snapshot of the current output theme.
pub(crate) fn output_theme() -> OutputTheme {
    OUTPUT_THEME.read().map(|g| g.clone()).unwrap_or_default()
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
        let pairs = map_of(vec![("value_color".into(), Value::String("none".into()))]);
        let theme = OutputTheme::from_map(&pairs).unwrap();
        assert_eq!(theme.value_color, None);
        assert_eq!(theme.value_prefix, "=> ");
    }

    #[test]
    fn theme_value_color_unknown_name_rejected() {
        let pairs = map_of(vec![("value_color".into(), Value::String("purple".into()))]);
        assert!(OutputTheme::from_map(&pairs).is_err());
    }

    #[test]
    fn theme_value_color_wrong_type_rejected() {
        let pairs = map_of(vec![("value_color".into(), Value::Int(3))]);
        assert!(OutputTheme::from_map(&pairs).is_err());
    }

    #[test]
    fn theme_value_prefix_wrong_type_rejected() {
        let pairs = map_of(vec![("value_prefix".into(), Value::Int(3))]);
        assert!(OutputTheme::from_map(&pairs).is_err());
    }

    #[test]
    fn theme_unknown_key_ignored_known_keys_apply() {
        let pairs = map_of(vec![
            ("wat".into(), Value::String("x".into())),
            ("value_prefix".into(), Value::String("> ".into())),
        ]);
        let theme = OutputTheme::from_map(&pairs).unwrap();
        assert_eq!(theme.value_prefix, "> ");
        assert_eq!(theme.value_color, Some(YELLOW.into()));
    }
}
