use super::command::WINDOWS_BATCH_EXTENSIONS;
use super::fold::fold_bytes;
use super::head::{HeadSpelling, Namespace, Parsed};
use super::*;
use crate::path::PathRules;
use HeadKind::*;
use Namespace::{Device as NsDevice, Object, Verbatim};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// One row per head form (`Relative`, `Driveless`, `DriveRelative`,
/// `Drive`, `Unc`, `Posix`, `Device`, `Malformed`) and separator spelling
/// (`\\`, `//`, `/\`, `\/`; for 4-byte namespace prefixes all eight
/// spellings of the three separator slots), plus every incomplete UNC and
/// verbatim form.  Expectations are literal, never computed from the
/// parser under test.
const PARSED_WINDOWS: &[(&str, HeadKind<'static>, HeadSpelling, usize)] = &[
    // Posix
    (
        "/x",
        Posix,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        1,
    ),
    (
        "/",
        Posix,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        1,
    ),
    (
        "/??/C:/x",
        Posix,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        1,
    ),
    // Driveless (plain and NT object manager)
    (
        r"\x",
        Driveless,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        1,
    ),
    (
        r"\",
        Driveless,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        1,
    ),
    (
        r"\?\x",
        Driveless,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        1,
    ),
    (
        r"\?\UNC\srv",
        Driveless,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        1,
    ),
    (
        r"\??\C:\x",
        Driveless,
        HeadSpelling {
            namespace: Some(Object),
            mixed_opening: false,
            opens_with_slash: false,
        },
        1,
    ),
    // DriveRelative
    (
        "C:x",
        DriveRelative { letter: b'C' },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        2,
    ),
    (
        "C:",
        DriveRelative { letter: b'C' },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        2,
    ),
    // Drive (non-verbatim, both separator spellings)
    (
        r"C:\x",
        Drive {
            letter: b'C',
            volume: b"C:",
        },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        3,
    ),
    (
        r"C:\",
        Drive {
            letter: b'C',
            volume: b"C:",
        },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        3,
    ),
    (
        "c:/x",
        Drive {
            letter: b'c',
            volume: b"c:",
        },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        3,
    ),
    (
        "C:/?/x",
        Drive {
            letter: b'C',
            volume: b"C:",
        },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        3,
    ),
    // Unc (non-verbatim, `\\` and `//` openings)
    (
        r"\\srv\sh",
        Unc {
            volume: br"\\srv\sh",
        },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        8,
    ),
    (
        r"\\srv\sh\x",
        Unc {
            volume: br"\\srv\sh",
        },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        9,
    ),
    (
        r"\\srv/sh",
        Unc {
            volume: br"\\srv/sh",
        },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        8,
    ),
    (
        "//srv/sh",
        Unc {
            volume: b"//srv/sh",
        },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        8,
    ),
    (
        "//srv/sh/x",
        Unc {
            volume: b"//srv/sh",
        },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        9,
    ),
    (
        r"//srv\sh\x",
        Unc {
            volume: br"//srv\sh",
        },
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        9,
    ),
    // Verbatim Drive: all 8 spellings of the three separator slots
    (
        r"\\?\C:\x",
        Drive {
            letter: b'C',
            volume: br"\\?\C:",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        8,
    ),
    (
        r"\\?\C:\",
        Drive {
            letter: b'C',
            volume: br"\\?\C:",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"\\?/C:\x",
        Drive {
            letter: b'C',
            volume: br"\\?/C:",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"\/?\C:\x",
        Drive {
            letter: b'C',
            volume: br"\/?\C:",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"\/?/C:\x",
        Drive {
            letter: b'C',
            volume: br"\/?/C:",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"/\?\C:\x",
        Drive {
            letter: b'C',
            volume: br"/\?\C:",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: true,
        },
        7,
    ),
    (
        r"/\?/C:\x",
        Drive {
            letter: b'C',
            volume: br"/\?/C:",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: true,
        },
        7,
    ),
    (
        r"//?\C:\x",
        Drive {
            letter: b'C',
            volume: br"//?\C:",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: true,
        },
        7,
    ),
    (
        "//?/C:/x",
        Drive {
            letter: b'C',
            volume: b"//?/C:",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: true,
        },
        7,
    ),
    // Verbatim Unc: all 8 spellings of the three separator slots
    (
        r"\\?\UNC\srv\sh",
        Unc {
            volume: br"\\?\UNC\srv\sh",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        14,
    ),
    (
        r"\\?\UNC\srv\sh\x",
        Unc {
            volume: br"\\?\UNC\srv\sh",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        16,
    ),
    (
        r"\\?\unc\srv\sh",
        Unc {
            volume: br"\\?\unc\srv\sh",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        14,
    ),
    (
        r"\\?/UNC\srv\sh\x",
        Unc {
            volume: br"\\?/UNC\srv\sh",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        15,
    ),
    (
        r"\/?\UNC\srv\sh\x",
        Unc {
            volume: br"\/?\UNC\srv\sh",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: false,
        },
        15,
    ),
    (
        r"\/?/UNC\srv\sh\x",
        Unc {
            volume: br"\/?/UNC\srv\sh",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: false,
        },
        15,
    ),
    (
        r"/\?\UNC\srv\sh\x",
        Unc {
            volume: br"/\?\UNC\srv\sh",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: true,
        },
        15,
    ),
    (
        r"/\?/unc\srv/sh",
        Unc {
            volume: br"/\?/unc\srv/sh",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: true,
        },
        14,
    ),
    (
        r"//?\UNC\srv\sh\x",
        Unc {
            volume: br"//?\UNC\srv\sh",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: true,
        },
        15,
    ),
    (
        "//?/UNC/srv/sh",
        Unc {
            volume: b"//?/UNC/srv/sh",
        },
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: true,
        },
        14,
    ),
    // Device: all 8 spellings of the three separator slots
    (
        r"\\.\C:\x",
        Device,
        HeadSpelling {
            namespace: Some(NsDevice { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        8,
    ),
    (
        r"\\.\pipe\x",
        Device,
        HeadSpelling {
            namespace: Some(NsDevice { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        10,
    ),
    (
        r"\\.\nul",
        Device,
        HeadSpelling {
            namespace: Some(NsDevice { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"\\./nul",
        Device,
        HeadSpelling {
            namespace: Some(NsDevice { exact: false }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"\/.\nul",
        Device,
        HeadSpelling {
            namespace: Some(NsDevice { exact: false }),
            mixed_opening: true,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"\/./nul",
        Device,
        HeadSpelling {
            namespace: Some(NsDevice { exact: false }),
            mixed_opening: true,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"/\.\pipe\x",
        Device,
        HeadSpelling {
            namespace: Some(NsDevice { exact: false }),
            mixed_opening: true,
            opens_with_slash: true,
        },
        10,
    ),
    (
        r"/\./nul",
        Device,
        HeadSpelling {
            namespace: Some(NsDevice { exact: false }),
            mixed_opening: true,
            opens_with_slash: true,
        },
        7,
    ),
    (
        r"//.\nul",
        Device,
        HeadSpelling {
            namespace: Some(NsDevice { exact: false }),
            mixed_opening: false,
            opens_with_slash: true,
        },
        7,
    ),
    (
        "//./pipe/x",
        Device,
        HeadSpelling {
            namespace: Some(NsDevice { exact: false }),
            mixed_opening: false,
            opens_with_slash: true,
        },
        10,
    ),
    // Malformed: all 8 spellings of `\\?\x`
    (
        r"\\?\x",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        5,
    ),
    (
        r"\\?/x",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        5,
    ),
    (
        r"\/?\x",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: false,
        },
        5,
    ),
    (
        r"\/?/x",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: false,
        },
        5,
    ),
    (
        r"/\?\x",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: true,
        },
        5,
    ),
    (
        r"/\?/x",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: true,
            opens_with_slash: true,
        },
        5,
    ),
    (
        r"//?\x",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: true,
        },
        5,
    ),
    (
        "//?/x",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: true,
        },
        5,
    ),
    // Malformed: incomplete UNC & verbatim forms and mixed 2-separator openings
    (
        r"\\",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        2,
    ),
    (
        "//",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        2,
    ),
    (
        r"/\",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: true,
            opens_with_slash: true,
        },
        2,
    ),
    (
        r"\/",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: true,
            opens_with_slash: false,
        },
        2,
    ),
    (
        "///x",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        4,
    ),
    (
        r"\\srv",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        5,
    ),
    (
        r"\\srv\",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        6,
    ),
    (
        "//srv",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        5,
    ),
    (
        "//srv/",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        6,
    ),
    (
        r"\\\srv\sh",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        9,
    ),
    (
        "///srv/sh",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        9,
    ),
    (
        r"\\?\",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        4,
    ),
    (
        r"\\?",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        3,
    ),
    (
        r"\\?\UNC\",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        8,
    ),
    (
        r"\\?\UNC\srv",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        11,
    ),
    (
        r"\\?\UNC\srv\",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        12,
    ),
    (
        r"\\?\UNC\?\sh",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        12,
    ),
    (
        r"\\?\UNC\.\sh",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        12,
    ),
    (
        "//?/UNC/",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: true,
        },
        8,
    ),
    (
        "//?/UNC/srv",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: false }),
            mixed_opening: false,
            opens_with_slash: true,
        },
        11,
    ),
    (
        r"\\?\C:",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        6,
    ),
    (
        r"\\?\?\x",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"\\..\sh",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"\\srv\.",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        7,
    ),
    (
        r"\\srv\..\x",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        10,
    ),
    (
        "//x/../y",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: true,
        },
        8,
    ),
    (
        r"\\?\UNC\..\sh",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        13,
    ),
    (
        r"\\?\UNC\srv\..",
        Malformed,
        HeadSpelling {
            namespace: Some(Verbatim { exact: true }),
            mixed_opening: false,
            opens_with_slash: false,
        },
        14,
    ),
    (
        r"/\x",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: true,
            opens_with_slash: true,
        },
        3,
    ),
    (
        r"/\srv\sh",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: true,
            opens_with_slash: true,
        },
        8,
    ),
    (
        r"\/srv/sh",
        Malformed,
        HeadSpelling {
            namespace: None,
            mixed_opening: true,
            opens_with_slash: false,
        },
        8,
    ),
    // Relative
    (
        "x",
        Relative,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        0,
    ),
    (
        "./x",
        Relative,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        0,
    ),
    (
        "",
        Relative,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash: false,
        },
        0,
    ),
];

/// Platform-independent: `parse_bytes` classifies `kind`, `spelling`,
/// `head_len`, and `volume` on the raw byte slice under both `PathRules`.
#[test]
fn parse_bytes_classifies_kind_and_spelling_under_both_rules() {
    for &(raw, want_kind, want_spelling, want_head_len) in PARSED_WINDOWS {
        let win = parse_bytes(raw.as_bytes(), PathRules::Windows);
        assert_eq!(
            win,
            Parsed {
                raw: raw.as_bytes(),
                rules: PathRules::Windows,
                kind: want_kind,
                spelling: want_spelling,
                head_len: want_head_len,
            },
            "{raw:?} windows"
        );
        let want_vol = match want_kind {
            Drive { volume, .. } | Unc { volume } => Some(volume),
            _ => None,
        };
        assert_eq!(win.volume(), want_vol, "{raw:?} volume");

        let posix = parse_bytes(raw.as_bytes(), PathRules::Posix);
        let want_posix = if want_spelling.opens_with_slash {
            Parsed {
                raw: raw.as_bytes(),
                rules: PathRules::Posix,
                kind: Posix,
                spelling: HeadSpelling {
                    namespace: None,
                    mixed_opening: false,
                    opens_with_slash: true,
                },
                head_len: 1,
            }
        } else {
            Parsed {
                raw: raw.as_bytes(),
                rules: PathRules::Posix,
                kind: Relative,
                spelling: HeadSpelling {
                    namespace: None,
                    mixed_opening: false,
                    opens_with_slash: false,
                },
                head_len: 0,
            }
        };
        assert_eq!(posix, want_posix, "{raw:?} posix");
    }
}

/// Platform-independent: the kernel's answer under each rules set, root by
/// root.  POSIX roots whatever opens with `/`; Windows whatever opens with
/// two separators, well formed or not, and a drive root.
#[test]
fn absoluteness_is_read_under_either_rules() {
    for (raw, posix, windows) in [
        ("/x", true, false),
        ("/", true, false),
        ("//srv/sh/x", true, true),
        (r"\\srv\sh\x", false, true),
        ("//?/C:/x", true, true),
        (r"\\?\C:\x", false, true),
        (r"/\?\C:\x", true, true),
        ("//?/UNC/srv/sh", true, true),
        (r"\\?\UNC\srv\sh", false, true),
        (r"C:\x", false, true),
        ("c:/x", false, true),
        ("//", true, true),
        ("///x", true, true),
        ("//srv", true, true),
        (r"\\", false, true),
        (r"\\srv", false, true),
        (r"\\?\x", false, true),
        ("//?/x", true, true),
        (r"\\.\pipe\x", false, true),
        (r"\\.\C:\x", false, true),
        (r"/\x", true, true),
        (r"/\srv\sh", true, true),
        (r"\/srv/sh", false, true),
        (r"\x", false, false),
        (r"\", false, false),
        ("C:x", false, false),
        ("C:", false, false),
        ("x", false, false),
        ("./x", false, false),
        ("", false, false),
    ] {
        assert_eq!(absolute_under(raw, PathRules::Posix), posix, "{raw} posix");
        assert_eq!(
            absolute_under(raw, PathRules::Windows),
            windows,
            "{raw} windows"
        );
    }
}

#[test]
fn network_paths_are_the_unc_roots() {
    for raw in [r"\\srv\sh\x", r"\\?\UNC\srv\sh\x", r"\\?\unc\srv\sh"] {
        assert!(is_network(Path::new(raw)), "{raw}");
    }
    for raw in [r"\\?\C:\x", r"\\.\Volume{1234}\x", r"\\.\C:\x", r"C:\x"] {
        assert!(!is_network(Path::new(raw)), "{raw}");
    }
    assert!(is_verbatim_drive(Path::new(r"\\?\C:\x")));
    assert!(!is_verbatim_drive(Path::new(r"C:\x")));
    assert!(!is_verbatim_drive(Path::new(r"\\.\C:\x")));
}

#[test]
fn volume_guid_paths_open_with_a_literal_head() {
    for raw in [
        r"\\?\Volume{1234}\",
        r"\??\volume{1234}\",
        r"\\.\Volume{1234}\x",
    ] {
        assert!(is_volume_guid(Path::new(raw)), "{raw}");
    }
    for raw in [
        r"\\?\D:\x",
        r"\\?\VolumeX",
        r"D:\Volume{1}",
        "//?/Volume{1}/",
    ] {
        assert!(!is_volume_guid(Path::new(raw)), "{raw}");
    }
}

/// A descendant name is entry names alone: a root, a dot, an empty
/// piece and, on Windows, a drive or a stream are refused.
#[test]
fn a_descendant_is_entry_names_alone() {
    let names = |rel: &str| {
        descendant(rel).map(|names| {
            names
                .iter()
                .map(|n| super::super::shown(n.as_os_str()))
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(names("a/b.txt").unwrap(), ["a", "b.txt"]);
    assert_eq!(names("a").unwrap(), ["a"]);
    for refused in ["", "/a", "a/", "a//b", "./a", "../a", "a/../b", "."] {
        assert!(names(refused).is_err(), "{refused:?}");
    }
    assert_eq!(
        names(r"a\b").unwrap(),
        if cfg!(windows) {
            vec!["a", "b"]
        } else {
            vec![r"a\b"]
        }
    );
    for windows_only in ["C:", "a.txt:s", r"\a", r"a\..\b", r"a\\b", r"a\"] {
        assert_eq!(
            names(windows_only).is_err(),
            cfg!(windows),
            "{windows_only}"
        );
    }
}

#[test]
fn a_stripped_suffix_inverts_the_appended_one() {
    for (base, ext) in [("ledger.json", "tmp"), ("x", "crash"), ("a", "tar.gz")] {
        let appended = with_appended_suffix(Path::new(base), ext);
        assert_eq!(strip_appended_suffix(&appended, ext), Some(pb(base)));
    }
    assert_eq!(strip_appended_suffix(Path::new("x.lock"), "tmp"), None);
    assert_eq!(strip_appended_suffix(Path::new(".lock"), "lock"), None);
}

fn pb(s: &str) -> PathBuf {
    PathBuf::from(s)
}

/// `b` and `parts`, joined with the separator of `rules`.
fn under_b(parts: &[&str], rules: PathRules) -> String {
    let separator = String::from(char::from(rules.separator()));
    std::iter::once("b")
        .chain(parts.iter().copied())
        .collect::<Vec<_>>()
        .join(&separator)
}

/// Accepted joins under either rules: separators split, empties drop,
/// so a single leading separator roots nothing, and `.`/`..` stay for
/// the resolver's fold.
#[test]
fn a_subpath_joins_component_by_component() {
    for rules in [PathRules::Posix, PathRules::Windows] {
        for (sub, want) in [
            ("", &[][..]),
            ("x", &["x"]),
            ("/x", &["x"]),
            ("x//y/", &["x", "y"]),
            ("../x", &["..", "x"]),
            ("./x", &[".", "x"]),
        ] {
            assert_eq!(
                join_under("b", sub, rules),
                Ok(under_b(want, rules)),
                "{sub:?} under {rules:?}"
            );
        }
    }
}

/// Off Windows a backslash and a colon are filename bytes: `C:\x` is one
/// component, and every root is the one root, so nothing is refused.
#[test]
fn posix_rules_take_backslash_and_colon_as_bytes() {
    for (sub, want) in [
        (r"C:\x", &[r"C:\x"][..]),
        (r"\x", &[r"\x"]),
        (r"\\?\C:\x", &[r"\\?\C:\x"]),
        ("a/C:", &["a", "C:"]),
        ("//x", &["x"]),
        ("//srv/sh", &["srv", "sh"]),
        ("//?/UNC/srv/sh", &["?", "UNC", "srv", "sh"]),
    ] {
        assert_eq!(
            join_under("b", sub, PathRules::Posix),
            Ok(under_b(want, PathRules::Posix)),
            "{sub:?}"
        );
    }
}

/// Under Windows rules a drive is refused at the head or after a
/// separator, a verbatim drive head included.
#[test]
fn windows_rules_refuse_a_drive_at_any_position() {
    for sub in [
        r"C:\x",
        "C:/x",
        "C:x",
        "C:",
        r"a\C:\x",
        "a/c:",
        r"\\?\C:\x",
        r"?\C:\x",
        "//?/C:/x",
    ] {
        assert_eq!(
            join_under("b", sub, PathRules::Windows),
            Err(Rerooting),
            "{sub:?}"
        );
    }
}

/// Under Windows rules a head of two separators names a share or a
/// namespace, well formed or not, and is refused rather than flattened
/// into components under the base.
#[test]
fn windows_rules_refuse_a_share_or_namespace_head() {
    for sub in [
        r"\\srv\sh\x",
        "//srv/sh/x",
        r"\\?\UNC\srv\sh",
        "//?/UNC/srv/sh",
        r"\\.\pipe\x",
        "//x",
        r"\\",
        r"\/srv/sh",
        r"/\srv\sh",
        r"/\x",
    ] {
        assert_eq!(
            join_under("b", sub, PathRules::Windows),
            Err(Rerooting),
            "{sub:?}"
        );
    }
}

/// Under Windows rules a root of the current volume, by either
/// separator, joins under the base.
#[test]
fn windows_rules_keep_a_driveless_root_under_the_base() {
    for (sub, want) in [
        (r"\x", &["x"][..]),
        ("/x", &["x"]),
        (r"\?\UNC\srv", &["?", "UNC", "srv"]),
    ] {
        assert_eq!(
            join_under("b", sub, PathRules::Windows),
            Ok(under_b(want, PathRules::Windows)),
            "{sub:?}"
        );
    }
}

/// The join is the given rules' on every host, never the host's: under
/// Posix rules `C:` is a name, which a host `Path::join` on Windows would
/// read as a drive replacing the base, and `\` a byte of one.
#[test]
fn a_join_under_foreign_rules_is_the_same_on_every_host() {
    for (base, sub, want) in [
        ("b", "C:", "b/C:"),
        ("b", r"C:\x", r"b/C:\x"),
        ("/", "C:/x", "/C:/x"),
        (r"b\", "x", r"b\/x"),
        ("", "x", "x"),
    ] {
        assert_eq!(
            join_under(base, sub, PathRules::Posix),
            Ok(want.to_owned()),
            "{base:?} + {sub:?}"
        );
    }
    for (base, sub, want) in [
        ("b", "x/y", r"b\x\y"),
        ("b/", "x", r"b/x"),
        (r"C:\", "x", r"C:\x"),
        ("C:", "x", "C:x"),
        ("/", "x", "/x"),
        ("", "x", "x"),
    ] {
        assert_eq!(
            join_under(base, sub, PathRules::Windows),
            Ok(want.to_owned()),
            "{base:?} + {sub:?}"
        );
    }
}

/// The guest's fold, pinned on every host: a guest path folds to a guest
/// path, separator intact, where a fold under Windows rules would join
/// with `\`.
#[test]
fn a_posix_fold_keeps_the_guests_separator() {
    for (raw, want) in [
        ("/work", "/work"),
        ("/work/./drafts/../letters", "/work/letters"),
        ("/work/", "/work"),
        ("/", "/"),
        ("//x", "/x"),
        ("/..", "/"),
        ("/a/../../x", "/x"),
        ("../x", "../x"),
        ("a/../../x", "../x"),
        (r"a\..\x", r"a\..\x"),
    ] {
        assert_eq!(fold_dots_under(raw, PathRules::Posix), want, "{raw}");
    }
}

/// Under Windows rules the names past the head join with `\`, and so
/// does a drive, share or driveless head; a Posix or slash-spelled
/// verbatim head is kept as written, and a malformed or mixed one, or an
/// exact `\\?\` verbatim path, kept whole.
#[test]
fn a_windows_fold_respells_a_head_only_where_that_keeps_it() {
    for (raw, want) in [
        (r"C:\a\..\b", r"C:\b"),
        ("C:/a/./b/", r"C:\a\b"),
        ("C:/", r"C:\"),
        (r"C:\..", r"C:\"),
        (r"C:a\..\..\b", r"C:..\b"),
        (r"\\srv\sh\..\x", r"\\srv\sh\x"),
        (r"\\srv\sh", r"\\srv\sh"),
        ("//srv/sh/a/../x", r"\\srv\sh\x"),
        (r"\\srv/sh", r"\\srv\sh"),
        (r"\\?\C:\a\..", r"\\?\C:\a\.."),
        (r"\\?\C:\a/b", r"\\?\C:\a/b"),
        (r"\\?\C:\a\.\b\", r"\\?\C:\a\.\b\"),
        ("//?/C:/a/b", r"//?/C:/a\b"),
        ("//?/C:/a/../b/.", r"//?/C:/b"),
        (r"\/?\C:\a/..\b", r"\/?\C:\b"),
        ("//?/UNC/srv/sh/a", r"//?/UNC/srv/sh/a"),
        (r"\\?\UNC\srv\sh\a\..\..", r"\\?\UNC\srv\sh\a\..\.."),
        (r"\\?\UNC\srv\sh/a", r"\\?\UNC\srv\sh/a"),
        ("/a/../x", "/x"),
        ("/a/b", r"/a\b"),
        (r"\a\..\x", r"\x"),
        ("//x/../y", "//x/../y"),
        (r"\\srv\.\a\..", r"\\srv\.\a\.."),
        (r"\\\srv\sh\..\x", r"\\\srv\sh\..\x"),
        (r"/\x\..\y", r"/\x\..\y"),
        (r"\\.\pipe\a\..\b", r"\\.\pipe\a\..\b"),
        (r"a\..\C:\x", r".\C:\x"),
        (r".\C:\x", r".\C:\x"),
    ] {
        assert_eq!(fold_dots_under(raw, PathRules::Windows), want, "{raw}");
    }
}

/// Compare `before` and `after` parsed heads after folding under `rules`:
/// `kind`, `volume` (with `/` respelled to `\` when `before.respell_head()`
/// under Windows rules), and `spelling.namespace` are preserved — save for
/// [`Namespace::Object`] (`\??\`), which Win32 reads as a [`Driveless`]
/// path (`head_len == 1`) whose first component is `??`, so `..` climbing
/// to `\??` or `\` keeps `Driveless` while dropping the 4-byte `\??\`
/// prefix (`namespace: None`).
fn assert_fold_preserves_head(before: Parsed<'_>, after: Parsed<'_>, ctx: &str) {
    let want_ns = if before.spelling.namespace == Some(Namespace::Object) {
        after
            .raw
            .starts_with(b"\\??\\")
            .then_some(Namespace::Object)
    } else {
        before.spelling.namespace
    };
    assert_eq!(after.spelling.namespace, want_ns, "{ctx}: namespace");
    let respell_vol = |vol: &[u8]| -> Vec<u8> {
        if before.rules == PathRules::Windows && before.respell_head() {
            vol.iter()
                .map(|&b| if b == b'/' { b'\\' } else { b })
                .collect()
        } else {
            vol.to_vec()
        }
    };
    match (before.kind, after.kind) {
        (
            Drive {
                letter: l1,
                volume: v1,
            },
            Drive {
                letter: l2,
                volume: v2,
            },
        ) => {
            assert_eq!(l2, l1, "{ctx}: drive letter");
            assert_eq!(v2, respell_vol(v1).as_slice(), "{ctx}: drive volume");
        }
        (Unc { volume: v1 }, Unc { volume: v2 }) => {
            assert_eq!(v2, respell_vol(v1).as_slice(), "{ctx}: unc volume");
        }
        (k1, k2) => assert_eq!(k2, k1, "{ctx}: kind"),
    }
}

/// The fold keeps the root its rules read, and is its own fixed point:
/// every head in [`PARSED_WINDOWS`], bare and under each tail, folds to a
/// path of that head, which folds to itself (for `\??\`, `Driveless` is
/// kept while `..` climbing to `\??` or `\` drops `Namespace::Object`).  So
/// [`LexicalPath`](super::super::LexicalPath)'s invariant, a fixed point
/// of [`fold_dots`], holds of whatever the resolver hands it.
#[test]
fn a_fold_keeps_the_root_under_either_rules() {
    let tails = ["", "/", "/x", r"\.\y\..", "/../..", r"\a\..\C:\z", "/./?/x"];
    for &(raw, _, _, _) in PARSED_WINDOWS {
        for tail in tails {
            let path = format!("{raw}{tail}");
            for rules in [PathRules::Posix, PathRules::Windows] {
                let parsed = parse_bytes(path.as_bytes(), rules);
                let folded = fold_dots_under(&path, rules);
                let parsed_folded = parse_bytes(folded.as_bytes(), rules);
                assert_fold_preserves_head(
                    parsed,
                    parsed_folded,
                    &format!("{path:?} -> {folded:?} ({rules:?})"),
                );
                if rules == PathRules::Windows {
                    if parsed.respell_head() {
                        assert!(!folded.contains('/'), "{path:?} -> {folded:?}");
                    } else {
                        assert!(
                            folded.starts_with(&path[..parsed.head_len]),
                            "{path:?} -> {folded:?}"
                        );
                    }
                }
                assert_eq!(
                    fold_dots_under(&folded, rules),
                    folded,
                    "{path:?} under {rules:?}"
                );
            }
        }
    }
}

/// Laws 1–3 (§9): lossless parsing, host-independent explicit-rule parsing
/// and folding, and fold stability (`P_r(F_r(s))` preserves head — with
/// [`Namespace::Object`] `\??\` keeping `Driveless` and retaining
/// `Some(Namespace::Object)` iff the folded path still opens with `\??\` —
/// and `F_r(F_r(s)) == F_r(s)`, with `F_W(s) == s` on opaque heads),
/// checked by exhaustive enumeration over the token alphabet
/// `["a", ".", "..", ""]` to depth 4 across both separator spellings
/// (`"/"`, `r"\"`), plus non-UTF-8 byte slices on Unix and
/// unpaired-surrogate `OsString`s on Windows.
#[test]
fn laws_1_to_3_hold_across_token_alphabet_to_depth_4() {
    let check_bytes = |s: &[u8], rules: PathRules| {
        let p = parse_bytes(s, rules);
        // Law 1: lossless parsing and head/tail partition.
        assert_eq!(p.raw, s);
        assert_eq!([&p.raw[..p.head_len], &p.raw[p.head_len..]].concat(), s);

        // Law 3: fold stability and opaque preservation.
        let f = fold_bytes(s, rules);
        let ff = fold_bytes(&f, rules);
        assert_eq!(ff, f, "idempotence failed on {s:?} under {rules:?}");
        if rules == PathRules::Windows && p.head_len == s.len() && !p.respell_head() {
            assert_eq!(f, s, "opaque head modified on {s:?}");
        }
        let pf = parse_bytes(&f, rules);
        assert_fold_preserves_head(p, pf, &format!("{s:?} ({rules:?})"));
    };

    let heads = [
        "",
        "/",
        r"\",
        "C:",
        r"C:\",
        "C:/",
        r"\\srv\sh\",
        "//srv/sh/",
        r"\\?\C:\",
        "//?/C:/",
        r"\\.\pipe\",
        r"\??\C:\",
        r"\\srv\",
        r"/\x\",
    ];
    let tokens = ["a", ".", "..", ""];
    let seps = ["/", r"\"];
    for head in heads {
        for sep in seps {
            for t0 in tokens {
                for t1 in tokens {
                    for t2 in tokens {
                        for t3 in tokens {
                            let tail = [t0, t1, t2, t3].join(sep);
                            let s = format!("{head}{tail}");
                            for rules in [PathRules::Posix, PathRules::Windows] {
                                check_bytes(s.as_bytes(), rules);
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        for raw in [
            &b"/a/\xff/./b/../\xfe"[..],
            &b"\\\xfe\\./..\\\xff"[..],
            &b"C:\\\xff\\..\\a"[..],
            &b"\\\\?\\C:\\\xff\\.."[..],
        ] {
            for rules in [PathRules::Posix, PathRules::Windows] {
                check_bytes(raw, rules);
            }
            let folded = fold_dots(Path::new(OsStr::from_bytes(raw)));
            assert_eq!(fold_dots(&folded), folded);
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let wide: Vec<u16> = r"C:\a\"
            .encode_utf16()
            .chain([0xD800])
            .chain(r"\.\b\..\".encode_utf16())
            .chain([0xD801])
            .collect();
        let os = std::ffi::OsString::from_wide(&wide);
        for rules in [PathRules::Posix, PathRules::Windows] {
            check_bytes(os.as_encoded_bytes(), rules);
        }
        let folded = fold_dots(Path::new(&os));
        assert_eq!(fold_dots(&folded), folded);
    }
}

/// Off Windows a name the host holds need not be UTF-8, and folds as
/// any other: its bytes are kept, past `..` and `.` alike.
#[cfg(unix)]
#[test]
fn fold_dots_keeps_a_name_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let raw = Path::new(OsStr::from_bytes(b"/a/\xff/./b/../c"));
    let want = Path::new(OsStr::from_bytes(b"/a/\xff/c"));
    assert_eq!(fold_dots(raw), want);
}

/// `std::path` finds no prefix in a doubled-separator head that is no
/// share or namespace, and would read it as on the current drive.
#[test]
fn an_unparsed_head_is_one_std_reads_on_the_current_drive() {
    for raw in [r"\\x", "//", r"\\\srv\sh", r"/\x"] {
        assert_eq!(is_unparsed_head(Path::new(raw)), cfg!(windows), "{raw}");
    }
    for raw in [
        r"C:\x",
        r"\\srv\sh\x",
        r"\\?\C:\x",
        "//?/C:/x",
        r"\\.\pipe\x",
        "/x",
        "x",
    ] {
        assert!(!is_unparsed_head(Path::new(raw)), "{raw}");
    }
}

#[cfg(unix)]
#[test]
fn relative_key_is_the_slash_joined_names_beneath_the_root() {
    use crate::path::LexicalPath;
    use std::os::unix::ffi::OsStrExt;
    let at = |p: &Path| LexicalPath::absolute(p).expect("an absolute fixture");
    let root = at(Path::new("/r"));
    for (shown, want) in [
        (Path::new("/r/a/b"), Some("a/b")),
        (Path::new("/r"), Some("")),
        (Path::new("/x/a"), None),
        (Path::new("/rr/a"), None),
        (Path::new("/r/a/../b"), None),
        (Path::new(OsStr::from_bytes(b"/r/\xff")), None),
    ] {
        assert_eq!(
            relative_key(&at(shown), &root).as_deref(),
            want,
            "{shown:?}"
        );
    }
}

#[test]
fn a_batch_image_is_named_by_its_extension_in_any_case() {
    for name in ["run.bat", r"C:\x\RUN.CMD", "a.b.Bat", ".bat"] {
        assert!(is_batch_image(OsStr::new(name)), "{name}");
    }
    for name in ["run.exe", "bat", "run.bat.exe", "run.batch"] {
        assert!(!is_batch_image(OsStr::new(name)), "{name}");
    }
}

/// Win32 trims trailing dots and blanks before it runs a name, so a
/// batch image spelled with them reaches `cmd.exe` all the same.
#[test]
fn a_batch_image_is_named_as_win32_trims_it() {
    for name in ["run.bat.", "run.bat ", "run.CMD. .", r"C:\x\run.bat.."] {
        assert!(is_batch_image(OsStr::new(name)), "{name:?}");
    }
    for name in ["run.exe.", "run.bat.exe ", "bat."] {
        assert!(!is_batch_image(OsStr::new(name)), "{name:?}");
    }
}

/// A batch image is one Windows runs by name, so the two lists may only
/// drift together.
#[test]
fn batch_extensions_are_exec_extensions() {
    for ext in WINDOWS_BATCH_EXTENSIONS {
        assert!(WINDOWS_EXEC_EXTENSIONS.contains(ext), "{ext}");
    }
}

#[test]
fn a_key_is_within_itself_and_its_descendants_only() {
    for (key, root, within) in [
        ("scans", "scans", true),
        ("scans/photo.jpg", "scans", true),
        ("scans/2026/a.jpg", "scans", true),
        ("scans2", "scans", false),
        ("scans2/photo.jpg", "scans", false),
        ("scan", "scans", false),
    ] {
        assert_eq!(key_within(key, root), within, "{key} under {root}");
    }
}
