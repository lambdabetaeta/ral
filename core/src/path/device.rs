//! Device names: the discard sink, and the DOS names no file may take.

use super::PathRules;
use super::identity::is_sep;

/// True iff `path` names the discard device: the sink that swallows every
/// byte and keeps none.  `/dev/null` under POSIX rules; under Windows', the
/// device-namespace spelling `\\.\NUL`, in either slash spelling and any case,
/// and nothing else: every other path ending in a reserved name is turned
/// away by [`reserved_device_refusal`].
///
/// The platform gate sits at the sole call site,
/// [`LexicalPath::is_discard`](super::LexicalPath::is_discard).
pub(crate) fn is_discard_device(path: &str, rules: PathRules) -> bool {
    match rules {
        PathRules::Posix => path == "/dev/null",
        PathRules::Windows => match path.as_bytes() {
            [a, b, b'.', c, name @ ..] => {
                [a, b, c].into_iter().all(|&s| is_sep(s)) && name.eq_ignore_ascii_case(b"nul")
            }
            _ => false,
        },
    }
}

/// The refusal owed `path` under Windows rules when its last component is a
/// DOS reserved device name (`NUL`, `CON`, `PRN`, `AUX`, `COM1`-`COM9`,
/// `LPT1`-`LPT9`, in any case, with or without an extension, trailing dots and
/// blanks as Win32 trims them), and `None` for every other path, the discard
/// device's own spelling included.
///
/// Such a name is no file: most Windows tools read it as the device, so a file
/// made under it is unusable to them, and ral does not take the device meaning
/// either; the one discard it offers is [`is_discard_device`]'s.
///
/// The gate is
/// [`LexicalPath::reserved_device_refusal`](super::LexicalPath::reserved_device_refusal).
pub(crate) fn reserved_device_refusal(path: &str, rules: PathRules) -> Option<String> {
    if rules == PathRules::Posix || is_discard_device(path, rules) {
        return None;
    }
    let last = path.rsplit(['/', '\\']).find(|c| !c.is_empty())?;
    let stem = last
        .trim_end_matches(['.', ' '])
        .split('.')
        .next()?
        .trim_end_matches(' ');
    if !is_reserved_device_stem(stem) {
        return None;
    }
    Some(if stem.eq_ignore_ascii_case("nul") {
        format!(
            "`{last}` is a DOS device name, which ral does not treat as a device. \
             Did you mean `\\\\.\\NUL`? \
             (A file named `{last}` would be unusable from most Windows tools.)"
        )
    } else {
        format!(
            "`{last}` is a reserved DOS device name, \
             and ral refuses reserved device names as file names"
        )
    })
}

fn is_reserved_device_stem(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "NUL" | "CON" | "PRN" | "AUX")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9']))
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: PathRules = PathRules::Windows;
    const P: PathRules = PathRules::Posix;

    // Only the device-namespace spelling is the discard under Windows rules;
    // both tables are pinned on every host.
    #[test]
    fn discard_device_table() {
        for p in [r"\\.\NUL", r"\\.\nul", "//./NUL", r"\/.\Nul"] {
            assert!(is_discard_device(p, W), "{p}");
        }
        for p in [
            "NUL",
            "nul",
            "nul.txt",
            "NULL",
            r"C:\denied\nul.txt",
            r"\\?\C:\denied\nul.txt",
            r"C:\x\NUL",
            r"\\?\C:\x\nul",
            r"\\?\NUL",
            r"\\.\NUL\x",
            r"\\.\NUL.txt",
            "/dev/null",
        ] {
            assert!(!is_discard_device(p, W), "{p}");
        }
        assert!(is_discard_device("/dev/null", P));
        for p in ["/dev/null/x", "/tmp/nul", "NUL", r"\\.\NUL"] {
            assert!(!is_discard_device(p, P), "{p}");
        }
    }

    #[test]
    fn reserved_device_names_are_refused_under_windows_rules() {
        for p in [
            "NUL",
            "nul",
            "NUL.txt",
            "con",
            "COM1.log",
            "LPT9",
            "prn.",
            "AUX ",
            r"C:\x\NUL",
            r"C:\x\nul .txt",
            "C:/x/aux",
            r"\\?\C:\denied\nul.txt",
        ] {
            assert!(reserved_device_refusal(p, W).is_some(), "{p}");
        }
    }

    #[test]
    fn ordinary_names_and_the_discard_are_not_refused() {
        for p in [
            r"\\.\NUL",
            "//./nul",
            "null",
            "nullable.txt",
            "CONFIG",
            "COM10",
            "COM0",
            "LPTA",
            r"C:\x\nul\file",
            r"\\.\NUL\test-agent",
            r"C:\",
            "...",
            "",
        ] {
            assert_eq!(reserved_device_refusal(p, W), None, "{p}");
        }
    }

    #[test]
    fn nothing_is_refused_off_windows() {
        for p in ["NUL", "con", "/tmp/aux.c"] {
            assert_eq!(reserved_device_refusal(p, P), None, "{p}");
        }
    }

    #[test]
    fn nul_refusal_names_the_component_and_the_discard() {
        assert_eq!(
            reserved_device_refusal(r"C:\x\NUL", W).as_deref(),
            Some(
                r"`NUL` is a DOS device name, which ral does not treat as a device. Did you mean `\\.\NUL`? (A file named `NUL` would be unusable from most Windows tools.)"
            )
        );
        let verbatim = reserved_device_refusal(r"\\?\C:\denied\nul.txt", W).unwrap();
        assert!(
            verbatim.starts_with("`nul.txt` is a DOS device name"),
            "{verbatim}"
        );
    }

    #[test]
    fn other_reserved_names_suggest_no_discard() {
        assert_eq!(
            reserved_device_refusal("COM1.log", W).as_deref(),
            Some(
                "`COM1.log` is a reserved DOS device name, and ral refuses reserved device names as file names"
            )
        );
    }
}
