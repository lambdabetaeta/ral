//! REPL value-output styling.
//!
//! `OutputTheme` controls how the REPL renders the result of a top-level
//! expression — a `value-prefix` string (default `"=> "`) and an optional
//! `value-color` (an ANSI SGR escape).  Both fields are configurable from
//! the RC file's `theme` key.
//!
//! Color is suppressed automatically when [`ral_core::io::terminal::ui_color`]
//! returns false, so the theme can store an unconditional `Some(color)`.

use ral_core::View;
use ral_core::io::ansi::{YELLOW, named_color};
use ral_core::shell::runtime::MapRef;
use ral_core::text::code;

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
    /// Decode a checked rc theme table. `inherit` keeps the built-in value;
    /// `set` takes text, with a colour name or `none` for `value-color`.
    pub(crate) fn from_map(pairs: MapRef<'_>) -> Result<Self, String> {
        let mut theme = Self::default();
        for (k, v) in pairs {
            let Some(value) = super::config::decode_setting(&v, &format!("theme.{k}"))? else {
                continue;
            };
            match (k, value.view()) {
                ("value-prefix", View::String(s)) => theme.value_prefix = s.to_string(),
                ("value-color", View::String(s)) if s.eq_ignore_ascii_case("none") => {
                    theme.value_color = None;
                }
                ("value-color", View::String(s)) => {
                    theme.value_color = Some(named_color(s.as_str()).ok_or_else(|| {
                        format!(
                            "{} must be one of black, red, green, \
                             yellow, blue, magenta, cyan, white, or none; got '{s}'",
                            code("theme.value-color")
                        )
                    })?);
                }
                ("value-prefix" | "value-color", _) => {
                    return Err(format!(
                        "{} payload for {} must be a String; got {}",
                        code("`set"),
                        code(&format!("theme.{k}")),
                        value.type_name()
                    ));
                }
                _ => unreachable!("the checker admits only theme's keys (SPEC §15.3)"),
            }
        }
        Ok(theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ral_core::ValClos;
    use ral_core::typecheck::table::THEME;

    fn theme_source(value: &str) -> ValClos {
        let source = ral_core::test_helper::rc_source(&[("theme", value)]);
        let mut shell = ral_core::test_helper::core_shell();
        let value = ral_core::shell::load::evaluate_closed(
            &ral_core::shell::runtime::Mooring::adrift(),
            &mut shell,
            &source,
            &ral_core::source::SourceOrigin::named("rc.ral"),
            Some(ral_core::typecheck::table::declared(
                ral_core::typecheck::Form::Rc,
            )),
        )
        .unwrap();
        let View::Map(rc) = value.view() else {
            panic!("a checked rc returns a map");
        };
        rc.get("theme").unwrap().into_owned()
    }

    #[test]
    fn theme_inherit_keeps_defaults() {
        let pairs = theme_source("[value-prefix: `inherit, value-color: `inherit]");
        let theme = OutputTheme::from_map(pairs.as_map("theme").unwrap()).unwrap();
        assert_eq!(theme.value_prefix, "=> ");
        assert_eq!(theme.value_color.as_deref(), Some(YELLOW));
    }

    #[test]
    fn theme_set_overrides_defaults() {
        let pairs = theme_source("[value-prefix: `set '> ', value-color: `set red]");
        let theme = OutputTheme::from_map(pairs.as_map("theme").unwrap()).unwrap();
        assert_eq!(theme.value_prefix, "> ");
        assert_eq!(theme.value_color, named_color("red"));
    }

    #[test]
    fn theme_value_color_none_suppresses_color() {
        let pairs = theme_source("[value-prefix: `inherit, value-color: `set none]");
        let theme = OutputTheme::from_map(pairs.as_map("theme").unwrap()).unwrap();
        assert_eq!(theme.value_color, None);
        assert_eq!(theme.value_prefix, "=> ");
    }

    #[test]
    fn theme_value_color_unknown_name_rejected() {
        let pairs = theme_source("[value-prefix: `inherit, value-color: `set purple]");
        assert!(
            OutputTheme::from_map(pairs.as_map("theme").unwrap())
                .unwrap_err()
                .contains("purple")
        );
    }

    #[test]
    fn every_declared_theme_key_is_handled_by_from_map() {
        for key in THEME.keys {
            let pairs = ValClos::map(vec![(key.label.to_owned(), ValClos::UNIT)]);
            let error = OutputTheme::from_map(pairs.as_map("theme").unwrap()).unwrap_err();
            assert!(error.contains(key.label), "{error}");
            let pairs = ValClos::map(vec![(
                key.label.to_owned(),
                ValClos::variant("set", Some(ValClos::int(1))),
            )]);
            let error = OutputTheme::from_map(pairs.as_map("theme").unwrap()).unwrap_err();
            assert!(
                error.contains(key.label) && error.contains("String"),
                "{error}"
            );
        }
    }
}
