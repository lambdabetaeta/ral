use std::path::Path;

use ral_core::{own, path::shown};

use crate::site::{Silent, site};

pub const BOOT_KEY: &str = "boot_contract";
pub const PROTO_KEY: &str = "proto_version";

pub const BOOT_CONSEQUENCE: &str = "The `ral.` settings the two ends agree on are therefore not the same set: this host \
    would write a kernel command line that guest refuses outright, and the only symptom \
    would be a sixty-second wait for a control plane nobody ever dialled.";
pub const PROTO_CONSEQUENCE: &str = "That engine would refuse this host's attach and close the connection, and the \
    conversation would fail to start with nothing but a closed socket to show for it.";

const REMEDY: &str = "Rebuild the media from this checkout on the machine that will boot it: \
                      `just vm::boot`; then package again.";

pub fn lines() -> String {
    format!(
        "{BOOT_KEY}={}\n{PROTO_KEY}={}\n",
        guest_abi::boot::CONTRACT,
        ral_core::protocol::PROTOCOL_VERSION,
    )
}

/// # Errors
/// Present media with an absent or unreadable manifest is refused.
pub fn read(boot: &Path) -> Result<Option<String>, String> {
    if !["kernel", "initramfs.img"]
        .iter()
        .any(|media| site!(Silent::MediaProbe, own::kind_named(&boot.join(media))).is_some())
    {
        return Ok(None);
    }
    let manifest = boot.join("boot-manifest.txt");
    site!(Silent::MediaManifest, own::read_named(&manifest))
        .map(Some)
        .map_err(|err| {
            format!(
                "cannot read the boot media manifest: {err}. The media in {} cannot be \
                 checked against this host. {REMEDY}",
                shown(boot),
            )
        })
}

/// # Errors
/// Refuses a missing key, a value that is not a number, or a version mismatch.
pub fn check(
    text: &str,
    path: &str,
    key: &str,
    expected: u32,
    consequence: &str,
) -> Result<(), String> {
    let recorded = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix(key)?.strip_prefix('='))
        .next_back();
    let Some(recorded) = recorded else {
        return Err(format!(
            "the guest media described by {path} carries no `{key}=` line, so its version \
             cannot be checked against this host's version {expected}. {consequence} {REMEDY}"
        ));
    };
    let recorded = recorded.trim();
    let recorded: u32 = recorded.parse().map_err(|err| {
        format!(
            "{path} records `{key}={recorded}`, which is not a number: {err}. This host expects \
             version {expected}. {consequence} {REMEDY}"
        )
    })?;
    if recorded != expected {
        return Err(format!(
            "the guest media records version {recorded} for `{key}`, and this host expects \
             version {expected} ({path} records `{key}={recorded}`). {consequence} {REMEDY}"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use guest_abi::boot::CONTRACT;
    use ral_core::protocol::PROTOCOL_VERSION;

    const KEYS: [(&str, u32, &str); 2] = [
        (BOOT_KEY, CONTRACT, BOOT_CONSEQUENCE),
        (PROTO_KEY, PROTOCOL_VERSION, PROTO_CONSEQUENCE),
    ];

    fn verdict(text: &str, (key, expected, consequence): (&str, u32, &str)) -> Result<(), String> {
        check(text, "boot-manifest.txt", key, expected, consequence)
    }

    #[test]
    fn either_media_file_requires_a_manifest() {
        for media in ["kernel", "initramfs.img"] {
            let boot = tempfile::tempdir().unwrap();
            std::fs::write(boot.path().join(media), []).unwrap();
            let refusal = read(boot.path()).unwrap_err();
            let manifest = shown(boot.path().join("boot-manifest.txt"));
            assert!(refusal.contains(&manifest), "{refusal}");
        }
    }

    #[test]
    fn a_manifest_this_host_writes_passes_this_hosts_check() {
        let boot = tempfile::tempdir().unwrap();
        std::fs::write(boot.path().join("kernel"), []).unwrap();
        std::fs::write(boot.path().join("boot-manifest.txt"), lines()).unwrap();
        let text = read(boot.path()).unwrap().expect("media are present");
        for key in KEYS {
            verdict(&text, key).unwrap_or_else(|err| panic!("{}: {err}", key.0));
        }
    }

    #[test]
    fn a_line_surrounded_by_whitespace_or_crlf_still_counts() {
        for (key, expected, consequence) in KEYS {
            for text in [
                format!("arch=amd64\r\n{key}={expected}\r\n"),
                format!("  \t{key}={expected}  \n"),
                format!("{key}={expected}"),
            ] {
                verdict(&text, (key, expected, consequence))
                    .unwrap_or_else(|err| panic!("{text:?}: {err}"));
            }
        }
    }

    #[test]
    fn a_version_other_than_the_hosts_is_refused() {
        for (key, expected, consequence) in KEYS {
            for recorded in [expected - 1, expected + 1] {
                let text = format!("arch=amd64\n{key}={recorded}\n");
                assert!(verdict(&text, (key, expected, consequence)).is_err());
            }
        }
    }

    #[test]
    fn a_missing_or_malformed_line_is_refused() {
        for key in KEYS {
            for text in [
                "arch=amd64\nral_git_hash=37fe06b\n".to_string(),
                format!("{}=seven\n", key.0),
                format!("{}=\n", key.0),
            ] {
                assert!(verdict(&text, key).is_err(), "{text:?}");
            }
        }
    }

    #[test]
    fn the_last_exact_key_wins() {
        let text = "version=broken\nversion=7\nversion_extra=8\n";
        check(text, "manifest.txt", "version", 7, "").expect("the last exact key decides");
    }
}
