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

const REMEDY: &str = "Rebuild the media from this checkout: `just guest-boot amd64` for the \
                      Windows guest, `just guest-boot` for the Mac's; then package again.";

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
mod media {
    use super::read;
    use ral_core::path::shown;

    #[test]
    fn absent_media_needs_no_manifest() {
        let boot = tempfile::tempdir().unwrap();
        assert_eq!(read(boot.path()).unwrap(), None);
    }

    #[test]
    fn either_media_file_requires_a_manifest() {
        for media in ["kernel", "initramfs.img"] {
            let boot = tempfile::tempdir().unwrap();
            std::fs::write(boot.path().join(media), []).unwrap();
            let refusal = read(boot.path()).unwrap_err();
            let manifest = shown(boot.path().join("boot-manifest.txt"));
            assert!(refusal.contains(&manifest), "{refusal}");
            assert!(refusal.contains("just guest-boot"), "{refusal}");
        }
    }

    #[test]
    fn present_media_reads_its_manifest() {
        let boot = tempfile::tempdir().unwrap();
        std::fs::write(boot.path().join("kernel"), []).unwrap();
        let text = super::lines();
        std::fs::write(boot.path().join("boot-manifest.txt"), &text).unwrap();
        assert_eq!(read(boot.path()).unwrap(), Some(text));
    }
}

#[cfg(test)]
mod boot_contract {
    use super::{BOOT_CONSEQUENCE, BOOT_KEY, check};
    use guest_abi::boot::CONTRACT;

    #[test]
    fn media_recording_this_contract_is_fit_to_package() {
        let manifest = format!(
            "arch=amd64\nkernel_version=7.0.0-14-generic\n{BOOT_KEY}={CONTRACT}\n\
             rust_target=x86_64-unknown-linux-musl\n"
        );
        check(
            &manifest,
            "out/boot/boot-manifest.txt",
            BOOT_KEY,
            CONTRACT,
            BOOT_CONSEQUENCE,
        )
        .expect("matching media must pass");
    }

    #[test]
    fn media_a_contract_behind_the_host_names_both_numbers() {
        let stale = format!("arch=amd64\n{BOOT_KEY}={}\n", CONTRACT - 1);
        let err = check(
            &stale,
            "vm-image/out/boot/boot-manifest.txt",
            BOOT_KEY,
            CONTRACT,
            BOOT_CONSEQUENCE,
        )
        .expect_err("media from an older contract must not be packaged");
        assert!(err.contains(&format!("version {}", CONTRACT - 1)), "{err}");
        assert!(err.contains(&format!("version {CONTRACT}")), "{err}");
        assert!(err.contains("vm-image/out/boot/boot-manifest.txt"), "{err}");
        assert!(err.contains("just guest-boot amd64"), "{err}");
    }

    #[test]
    fn media_predating_the_contract_line_gets_its_own_sentence() {
        let ancient = "arch=amd64\nral_git_hash=37fe06b\n";
        let err = check(
            ancient,
            "out/boot/boot-manifest.txt",
            BOOT_KEY,
            CONTRACT,
            BOOT_CONSEQUENCE,
        )
        .expect_err("a manifest with no contract line must not be packaged");
        assert!(err.contains(&format!("no `{BOOT_KEY}=` line")), "{err}");
        assert!(err.contains("just guest-boot"), "{err}");
    }

    #[test]
    fn a_contract_line_that_is_not_a_number_is_refused_as_such() {
        let err = check(
            &format!("{BOOT_KEY}=two\n"),
            "m.txt",
            BOOT_KEY,
            CONTRACT,
            BOOT_CONSEQUENCE,
        )
        .expect_err("an unparsable contract version must be refused");
        assert!(err.contains("not a number"), "{err}");
    }
}

#[cfg(test)]
mod engine_protocol {
    use super::{PROTO_CONSEQUENCE, PROTO_KEY, check};
    use ral_core::protocol::PROTOCOL_VERSION;

    #[test]
    fn media_speaking_this_protocol_is_fit_to_package() {
        let manifest = format!(
            "arch=amd64\nboot_contract=2\n{PROTO_KEY}={PROTOCOL_VERSION}\n\
             rust_target=x86_64-unknown-linux-musl\n"
        );
        check(
            &manifest,
            "out/boot/boot-manifest.txt",
            PROTO_KEY,
            PROTOCOL_VERSION,
            PROTO_CONSEQUENCE,
        )
        .expect("matching media must pass");
    }

    #[test]
    fn an_engine_behind_the_host_names_both_numbers() {
        let stale = format!("arch=amd64\n{PROTO_KEY}={}\n", PROTOCOL_VERSION - 3);
        let err = check(
            &stale,
            "vm-image/out/boot/boot-manifest.txt",
            PROTO_KEY,
            PROTOCOL_VERSION,
            PROTO_CONSEQUENCE,
        )
        .expect_err("media from an older protocol must not be packaged");
        assert!(
            err.contains(&format!("version {}", PROTOCOL_VERSION - 3)),
            "{err}"
        );
        assert!(err.contains(&PROTOCOL_VERSION.to_string()), "{err}");
        assert!(err.contains("vm-image/out/boot/boot-manifest.txt"), "{err}");
        assert!(err.contains("just guest-boot amd64"), "{err}");
    }

    #[test]
    fn media_predating_the_line_gets_its_own_sentence() {
        let err = check(
            "arch=amd64\nral_git_hash=7c1c8ae6\n",
            "out/boot-manifest.txt",
            PROTO_KEY,
            PROTOCOL_VERSION,
            PROTO_CONSEQUENCE,
        )
        .expect_err("a manifest with no protocol line must not be packaged");
        assert!(err.contains(&format!("no `{PROTO_KEY}=` line")), "{err}");
        assert!(err.contains("just guest-boot"), "{err}");
    }

    #[test]
    fn a_line_that_is_not_a_number_is_refused_as_such() {
        let err = check(
            &format!("{PROTO_KEY}=seven\n"),
            "m.txt",
            PROTO_KEY,
            PROTOCOL_VERSION,
            PROTO_CONSEQUENCE,
        )
        .expect_err("an unparsable protocol version must be refused");
        assert!(err.contains("not a number"), "{err}");
    }
}

#[cfg(test)]
mod keys {
    use super::check;

    #[test]
    fn the_last_exact_key_wins() {
        let text = "version=broken\nversion=7\nversion_extra=8\n";
        check(
            text,
            "manifest.txt",
            "version",
            7,
            "An incompatible guest refuses the host.",
        )
        .expect("the last exact key must determine the version");
    }
}
