//! Which platforms' rules a grant's raw payload is live on, and why one is
//! refused on every platform.

use super::head::{HeadKind, parse_bytes};
use crate::path::PathRules;

/// Which platforms' rules admit a raw `absolute` payload as a live grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EligibleOn {
    Posix,
    Windows,
    Both,
}

impl EligibleOn {
    pub(crate) fn admits(self, rules: PathRules) -> bool {
        match rules {
            PathRules::Posix => matches!(self, Self::Posix | Self::Both),
            PathRules::Windows => matches!(self, Self::Windows | Self::Both),
        }
    }
}

/// Why a raw `absolute` payload is refused on every platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantRefusal {
    Malformed,
    MixedHead,
    NtObject,
    Driveless,
    DriveRelative,
    /// Relative under both platforms' rules, or (in `authority::grant::freeze::host_absolute`) a held
    /// path whose head is live only on the other platform.
    ForeignOrRelative,
}

/// Classify a raw `absolute` payload from one [`parse_bytes`] call under
/// Windows rules into the platforms it is live on or the refusal it raises.
pub(crate) fn grant_eligibility(raw: &str) -> Result<EligibleOn, GrantRefusal> {
    let parsed = parse_bytes(raw.as_bytes(), PathRules::Windows);
    match parsed.kind {
        HeadKind::Posix => Ok(EligibleOn::Posix),
        HeadKind::Unc { .. }
            if parsed.spelling.namespace.is_none() && parsed.spelling.opens_with_slash =>
        {
            Ok(EligibleOn::Both)
        }
        HeadKind::Drive { .. } | HeadKind::Unc { .. } => Ok(EligibleOn::Windows),
        _ if parsed.is_mixed_head() => Err(GrantRefusal::MixedHead),
        HeadKind::Device | HeadKind::Malformed => Err(GrantRefusal::Malformed),
        _ if parsed.is_nt_object() => Err(GrantRefusal::NtObject),
        HeadKind::Driveless => Err(GrantRefusal::Driveless),
        HeadKind::DriveRelative { .. } => Err(GrantRefusal::DriveRelative),
        HeadKind::Relative => Err(GrantRefusal::ForeignOrRelative),
    }
}

/// Why a relative `subpath` payload is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubpathRefusal {
    Empty,
    Drive,
    MixedHead,
    Rooted,
    NtObject,
    Driveless,
}

/// Classify a relative `subpath` payload, judged under Windows rules so a
/// profile fails alike on every platform.
pub(crate) fn classify_subpath(s: &str) -> Result<(), SubpathRefusal> {
    let parsed = parse_bytes(s.as_bytes(), PathRules::Windows);
    match parsed.kind {
        HeadKind::Relative if s.is_empty() => Err(SubpathRefusal::Empty),
        HeadKind::Relative => Ok(()),
        _ if parsed.has_drive() => Err(SubpathRefusal::Drive),
        _ if parsed.is_mixed_head() => Err(SubpathRefusal::MixedHead),
        _ if parsed.spelling.opens_with_slash => Err(SubpathRefusal::Rooted),
        _ if parsed.is_nt_object() => Err(SubpathRefusal::NtObject),
        _ => Err(SubpathRefusal::Driveless),
    }
}
