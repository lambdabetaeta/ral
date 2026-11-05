//! Session configuration arrives once through whitespace-separated `ral.` kernel words.
//! Non-`ral.` words are ignored; repeated keys take their last value.
//! Unknown or valueless `ral.` keys are refused.

use std::{fmt, net::Ipv4Addr};

use crate::link::{LINK, Link};
use crate::ports;

pub const PREFIX: &str = "ral.";

/// Compiled by `media-manifest`, compared by `check-media`, both in build-tools.
pub const CONTRACT: u32 = 4;

/// The engine path defaults because it belongs to the image, not the session.
pub const DEFAULT_ENGINE: &str = "/usr/libexec/ral/engine";

/// Virtiofs tags have a 36-byte limit.
const MAX_TAG: usize = 36;

const ZERO_PORT: &str =
    "names no port; 0 is the kernel's wildcard, not something a guest can connect to.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shares {
    Virtiofs { tag: String },
    Plan9 { port: u32 },
}

/// A nonzero port and a distinct gateway inside the guest's subnet, validated once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Net {
    port: u32,
    link: Link,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    Port,
    Prefix(u8),
    GatewayIsAddress,
    GatewayOutside {
        gateway: Ipv4Addr,
        address: Ipv4Addr,
        prefix: u8,
    },
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Port => f.write_str(ZERO_PORT),
            Self::Prefix(prefix) => write!(
                f,
                "names /{prefix} as the prefix length; it must be a whole number from 0 to 32.",
            ),
            Self::GatewayIsAddress => f.write_str(
                "names the guest's own address as the gateway; a gateway must be a different host on the subnet.",
            ),
            Self::GatewayOutside {
                gateway,
                address,
                prefix,
            } => write!(
                f,
                "names a gateway, {gateway}, that is not inside {address}/{prefix}; the guest could never route to it. Fix the prefix or the gateway.",
            ),
        }
    }
}

impl Net {
    /// # Errors
    /// Refuses port zero, a prefix above 32, or a gateway equal to or outside the guest's address/subnet.
    pub const fn new(port: u32, link: Link) -> Result<Self, NetError> {
        if port == 0 {
            return Err(NetError::Port);
        }
        if link.prefix > 32 {
            return Err(NetError::Prefix(link.prefix));
        }
        if link.gateway.to_bits() == link.address.to_bits() {
            return Err(NetError::GatewayIsAddress);
        }
        let mask = netmask(link.prefix).to_bits();
        if link.gateway.to_bits() & mask != link.address.to_bits() & mask {
            return Err(NetError::GatewayOutside {
                gateway: link.gateway,
                address: link.address,
                prefix: link.prefix,
            });
        }
        Ok(Self { port, link })
    }

    pub const STANDARD: Self = match Self::new(ports::NET, LINK) {
        Ok(net) => net,
        Err(_) => panic!("guest_abi::link::LINK is not routable"),
    };

    pub const fn port(&self) -> u32 {
        self.port
    }

    pub const fn link(&self) -> Link {
        self.link
    }

    pub const fn netmask(&self) -> Ipv4Addr {
        netmask(self.link.prefix)
    }
}

/// `/0` is special-cased to avoid shifting a `u32` by its own width.
const fn netmask(prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from_bits(if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boot {
    pub shares: Shares,
    /// The host's folder-maintenance port, separate from control and net.
    pub mounts: u32,
    /// The host's control port; its connection becomes the engine's protocol socket.
    pub port: u32,
    /// Host Unix seconds; the guest's clock is untrusted until these are applied.
    pub epoch: i64,
    pub engine: String,
    pub net: Option<Net>,
}

impl Boot {
    /// # Errors
    /// Refuses missing, unknown, valueless or invalid `ral.` settings with a remedy.
    pub fn read(cmdline: &str) -> Result<Self, String> {
        let (mut shares, mut mounts, mut port, mut epoch, mut engine, mut net) =
            (None, None, None, None, None, None);
        let mut addressed_to_us = false;

        for word in cmdline.split_ascii_whitespace() {
            let Some(setting) = word.strip_prefix(PREFIX) else {
                continue;
            };
            addressed_to_us = true;
            let (key, value) = setting.split_once('=').ok_or_else(|| {
                format!(
                    "the kernel command line carries `{word}`, which names no value. \
                     Every ral setting is spelled `{PREFIX}<key>=<value>`."
                )
            })?;
            match key {
                "shares" => shares = Some(parse_shares(value)?),
                "mounts" => mounts = Some(parse_vsock_port("mounts", value)?),
                "port" => port = Some(parse_vsock_port("port", value)?),
                "epoch" => epoch = Some(parse_epoch(value)?),
                "engine" => engine = Some(validate_engine(value)?),
                "net" => net = Some(parse_net(value)?),
                _ => {
                    return Err(format!(
                        "the kernel command line carries `{PREFIX}{key}=…`, which ral-guest \
                         does not understand. The settings it knows are {PREFIX}shares, \
                         {PREFIX}mounts, {PREFIX}port, {PREFIX}epoch, {PREFIX}engine, and \
                         {PREFIX}net."
                    ));
                }
            }
        }

        if !addressed_to_us {
            return Err(format!(
                "this kernel was not booted by a ral virtual-machine manager: its command line \
                 carries no `{PREFIX}` settings at all. ral-guest is the init of a synod or \
                 exarch guest and has nothing to do here."
            ));
        }

        Ok(Self {
            shares: shares.ok_or_else(|| {
                format!(
                    "the boot is missing `{PREFIX}shares=virtiofs:<tag>` or \
                     `{PREFIX}shares=9p:<port>`. Name the host's share transport; \
                     folders are mounted separately after boot."
                )
            })?,
            mounts: mounts.ok_or_else(|| {
                format!(
                    "the boot is missing `{PREFIX}mounts=<port>`, the host's vsock port for \
                     folder maintenance. Supply its listener port; there is no safe default."
                )
            })?,
            port: port.ok_or_else(|| {
                format!(
                    "the boot is missing `{PREFIX}port=<port>`, the host's vsock port for the \
                     control plane, not the share transport or folder-maintenance wire. \
                     There is no safe default: a guessed port would leave the engine waiting on \
                     a connection nobody is listening for."
                )
            })?,
            epoch: epoch.ok_or_else(|| {
                format!(
                    "the boot is missing `{PREFIX}epoch=<seconds>`, the host's wall clock at \
                     boot. The guest has no real-time clock, so without it every file it writes \
                     would be stamped 1970."
                )
            })?,
            engine: engine.unwrap_or_else(|| DEFAULT_ENGINE.to_string()),
            net,
        })
    }
}

/// The single command-line writer for both hypervisor backends.
pub fn command_line(boot: &Boot, console: &str) -> String {
    use std::fmt::Write as _;

    let mut line = format!("console={console}");
    match &boot.shares {
        Shares::Virtiofs { tag } => {
            let _ = write!(line, " {PREFIX}shares=virtiofs:{tag}");
        }
        Shares::Plan9 { port } => {
            let _ = write!(line, " {PREFIX}shares=9p:{port}");
        }
    }
    let _ = write!(
        line,
        " {PREFIX}mounts={} {PREFIX}port={} {PREFIX}epoch={}",
        boot.mounts, boot.port, boot.epoch
    );
    if boot.engine != DEFAULT_ENGINE {
        let _ = write!(line, " {PREFIX}engine={}", boot.engine);
    }
    if let Some(net) = &boot.net {
        let link = net.link();
        let _ = write!(
            line,
            " {PREFIX}net={},{}/{},{}",
            net.port(),
            link.address,
            link.prefix,
            link.gateway
        );
    }
    line
}

fn parse_shares(value: &str) -> Result<Shares, String> {
    match value.split_once(':') {
        Some(("virtiofs", tag)) => Ok(Shares::Virtiofs {
            tag: validate_tag(tag)?,
        }),
        Some(("9p", port)) => Ok(Shares::Plan9 {
            port: parse_vsock_port("shares", port)?,
        }),
        _ => Err(format!(
            "`{PREFIX}shares={value}` names an unknown share transport. \
             Use `{PREFIX}shares=virtiofs:<tag>` or `{PREFIX}shares=9p:<port>`."
        )),
    }
}

fn validate_tag(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!(
            "`{PREFIX}shares=virtiofs:` has an empty tag; supply the host's virtiofs device tag."
        ));
    }
    if value.len() > MAX_TAG {
        return Err(format!(
            "`{PREFIX}shares=virtiofs:{value}` has a {}-byte tag; use at most {MAX_TAG} bytes.",
            value.len()
        ));
    }
    if value.contains('/') {
        return Err(format!(
            "`{PREFIX}shares=virtiofs:{value}` looks like a path. Supply the virtiofs device \
             tag, not a directory."
        ));
    }
    Ok(value.to_string())
}

fn parse_vsock_port(key: &str, value: &str) -> Result<u32, String> {
    match value.parse::<u32>() {
        Ok(0) => Err(format!("`{PREFIX}{key}=0` {ZERO_PORT}")),
        Ok(port) => Ok(port),
        Err(err) => Err(format!(
            "`{PREFIX}{key}={value}` is not a vsock port number: {err}."
        )),
    }
}

fn parse_net(value: &str) -> Result<Net, String> {
    let bad = || {
        format!(
            "`{PREFIX}net={value}` is not `<port>,<address>/<prefix>,<gateway>`: the host's \
             vsock port for the net wire, the guest's address in CIDR notation, and the \
             gateway, joined by commas, e.g. `1730,10.0.2.15/24,10.0.2.2`."
        )
    };
    let mut parts = value.splitn(3, ',');
    let (Some(port), Some(cidr), Some(gateway)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad());
    };
    let port = port
        .parse()
        .map_err(|err| format!("`{PREFIX}net={value}` names `{port}` as the vsock port: {err}."))?;
    let (address, prefix) = cidr.split_once('/').ok_or_else(bad)?;
    let address: Ipv4Addr = address.parse().map_err(|err| {
        format!("`{PREFIX}net={value}` names `{address}` as the guest's address: {err}.")
    })?;
    let prefix: u8 = prefix.parse().map_err(|err| {
        format!(
            "`{PREFIX}net={value}` names `{prefix}` as the prefix length: {err}; it must be a \
             whole number from 0 to 32."
        )
    })?;
    let gateway: Ipv4Addr = gateway
        .parse()
        .map_err(|err| format!("`{PREFIX}net={value}` names `{gateway}` as the gateway: {err}."))?;
    Net::new(
        port,
        Link {
            address,
            prefix,
            gateway,
        },
    )
    .map_err(|e| format!("`ral.net={value}` {e}"))
}

fn parse_epoch(value: &str) -> Result<i64, String> {
    match value.parse::<i64>() {
        Ok(epoch) if epoch < 0 => Err(format!(
            "`{PREFIX}epoch={value}` is before 1970. The host's clock is meant to be handed over \
             as whole seconds since the Unix epoch."
        )),
        Ok(epoch) => Ok(epoch),
        Err(err) => Err(format!(
            "`{PREFIX}epoch={value}` is not a count of seconds since the Unix epoch: {err}."
        )),
    }
}

fn validate_engine(value: &str) -> Result<String, String> {
    if value.starts_with('/') {
        return Ok(value.to_string());
    }
    Err(format!(
        "`{PREFIX}engine={value}` is not an absolute path. ral-guest runs before anything has a \
         meaningful working directory, so the engine must be named from the root."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slash_24_masks_the_last_octet() {
        assert_eq!(netmask(24), Ipv4Addr::new(255, 255, 255, 0));
    }

    #[test]
    fn a_slash_32_masks_nothing() {
        assert_eq!(netmask(32), Ipv4Addr::BROADCAST);
    }

    #[test]
    fn a_slash_0_masks_everything() {
        assert_eq!(netmask(0), Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn a_slash_17_masks_across_the_octet_boundary() {
        assert_eq!(netmask(17), Ipv4Addr::new(255, 255, 128, 0));
    }

    #[test]
    fn a_slash_32_link_has_no_room_for_a_gateway() {
        let address = Ipv4Addr::new(10, 0, 2, 15);
        assert_eq!(
            Net::new(
                1730,
                Link {
                    address,
                    prefix: 32,
                    gateway: address,
                },
            ),
            Err(NetError::GatewayIsAddress)
        );
        let gateway = Ipv4Addr::new(10, 0, 2, 16);
        assert_eq!(
            Net::new(
                1730,
                Link {
                    address,
                    prefix: 32,
                    gateway,
                },
            ),
            Err(NetError::GatewayOutside {
                gateway,
                address,
                prefix: 32,
            })
        );
    }

    #[test]
    fn a_full_command_line_reads_into_a_boot() {
        let boot = Boot::read(
            "console=hvc0 root=/dev/vda ro ral.shares=virtiofs:shares ral.mounts=1733 ral.port=1729 ral.epoch=1771200000",
        )
        .expect("a complete command line must parse");
        assert_eq!(
            boot,
            Boot {
                shares: Shares::Virtiofs {
                    tag: "shares".into()
                },
                mounts: 1733,
                port: 1729,
                epoch: 1_771_200_000,
                engine: DEFAULT_ENGINE.into(),
                net: None,
            }
        );
    }

    #[test]
    fn plan9_and_mounts_keep_their_own_ports() {
        let boot = Boot::read(
            "console=ttyS0 ral.shares=9p:564 ral.mounts=1733 ral.port=1729 ral.epoch=1771200000",
        )
        .unwrap();
        assert_eq!(boot.shares, Shares::Plan9 { port: 564 });
        assert_eq!(boot.port, 1729);
        assert_eq!(boot.mounts, 1733);
    }

    #[test]
    fn settings_may_arrive_in_any_order() {
        let before = Boot::read("ral.mounts=3 ral.shares=9p:7 ral.port=1 ral.epoch=0").unwrap();
        let after = Boot::read("ral.epoch=0 ral.port=1 ral.shares=9p:7 ral.mounts=3").unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn the_engine_path_may_be_overridden() {
        let boot = Boot::read(
            "ral.shares=virtiofs:s ral.mounts=3 ral.port=1 ral.epoch=0 ral.engine=/opt/ral/engine",
        )
        .expect("an overridden engine path must parse");
        assert_eq!(boot.engine, "/opt/ral/engine");
    }

    #[test]
    fn a_command_line_without_ral_settings_is_not_a_ral_guest() {
        let err = Boot::read("console=ttyAMA0 root=/dev/vda").expect_err("not our kernel");
        assert!(
            err.contains("not booted by a ral virtual-machine manager"),
            "{err}"
        );
    }

    #[test]
    fn each_missing_setting_names_itself() {
        for (line, key) in [
            ("ral.mounts=3 ral.port=1 ral.epoch=0", "ral.shares"),
            ("ral.shares=virtiofs:s ral.port=1 ral.epoch=0", "ral.mounts"),
            ("ral.shares=9p:7 ral.mounts=3 ral.epoch=0", "ral.port"),
            ("ral.shares=9p:7 ral.mounts=3 ral.port=1", "ral.epoch"),
        ] {
            let err = Boot::read(line).expect_err(key);
            assert!(err.contains(key), "{err}");
        }
    }

    #[test]
    fn an_unknown_ral_setting_is_refused() {
        let err =
            Boot::read("ral.shares=virtiofs:s ral.mounts=3 ral.port=1 ral.epoch=0 ral.netowrk=on")
                .expect_err("a misspelled setting must be refused");
        assert!(err.contains("does not understand"), "{err}");
        for known in [
            "ral.shares",
            "ral.mounts",
            "ral.engine",
            "ral.port",
            "ral.epoch",
            "ral.net",
        ] {
            assert!(
                err.contains(known),
                "the refusal lists every known key, and not {known}: {err}"
            );
        }
    }

    #[test]
    fn a_valueless_ral_word_is_refused() {
        let err = Boot::read("ral.quiet").expect_err("a valueless setting must be refused");
        assert!(err.contains("names no value"), "{err}");
    }

    #[test]
    fn a_repeated_setting_takes_its_last_value() {
        let boot = Boot::read("ral.shares=virtiofs:first ral.mounts=3 ral.port=1 ral.epoch=0 ral.shares=9p:7 ral.mounts=4").unwrap();
        assert_eq!(boot.shares, Shares::Plan9 { port: 7 });
        assert_eq!(boot.mounts, 4);
    }

    #[test]
    fn a_virtiofs_tag_is_a_short_pathless_word() {
        for tag in [
            String::new(),
            "/shares".into(),
            "t".repeat(MAX_TAG + 1),
            "é".repeat(19),
        ] {
            let line = format!("ral.shares=virtiofs:{tag} ral.mounts=3 ral.port=1 ral.epoch=0");
            Boot::read(&line).expect_err("invalid tag");
        }
        for tag in ["t".repeat(MAX_TAG), "é".repeat(18)] {
            let line = format!("ral.shares=virtiofs:{tag} ral.mounts=3 ral.port=1 ral.epoch=0");
            assert_eq!(Boot::read(&line).unwrap().shares, Shares::Virtiofs { tag });
        }
    }

    #[test]
    fn unknown_transports_name_both_legal_spellings() {
        for value in ["", "shares", "9p", "virtiofs", "other:s", "9P:7"] {
            let err = Boot::read(&format!("ral.shares={value}")).unwrap_err();
            assert!(err.contains("ral.shares=virtiofs:<tag>"), "{err}");
            assert!(err.contains("ral.shares=9p:<port>"), "{err}");
        }
    }

    #[test]
    fn retired_settings_have_no_compatibility_path() {
        for setting in ["ral.workspace=w", "ral.plan9=564"] {
            let err = Boot::read(setting).unwrap_err();
            assert!(err.contains("does not understand"), "{err}");
        }
    }

    #[test]
    fn a_vsock_port_of_zero_is_refused_whichever_setting_carries_it() {
        for setting in ["ral.port=0", "ral.mounts=0", "ral.shares=9p:0"] {
            let err = Boot::read(setting).unwrap_err();
            assert!(err.contains("wildcard"), "{err}");
        }
    }

    #[test]
    fn malformed_vsock_ports_are_refused() {
        for value in ["", "-1", "4294967296", "port", "1:2"] {
            for setting in [
                format!("ral.shares=9p:{value}"),
                format!("ral.mounts={value}"),
                format!("ral.port={value}"),
            ] {
                Boot::read(&setting).expect_err(&setting);
            }
        }
    }

    #[test]
    fn a_negative_epoch_is_refused() {
        let err = Boot::read("ral.shares=virtiofs:s ral.mounts=3 ral.port=1 ral.epoch=-1")
            .expect_err("before 1970");
        assert!(err.contains("before 1970"), "{err}");
    }

    #[test]
    fn a_relative_engine_path_is_refused() {
        let err = Boot::read(
            "ral.shares=virtiofs:s ral.mounts=3 ral.port=1 ral.epoch=0 ral.engine=engine",
        )
        .expect_err("a relative engine path must be refused");
        assert!(err.contains("absolute path"), "{err}");
    }

    #[test]
    fn a_net_key_reads_into_its_four_fields() {
        let boot =
            Boot::read("ral.shares=virtiofs:s ral.mounts=3 ral.port=1 ral.epoch=0 ral.net=1730,10.0.2.15/24,10.0.2.2")
                .expect("a well-formed net key must parse");
        assert_eq!(
            boot.net,
            Some(
                Net::new(
                    1730,
                    Link {
                        address: Ipv4Addr::new(10, 0, 2, 15),
                        prefix: 24,
                        gateway: Ipv4Addr::new(10, 0, 2, 2),
                    },
                )
                .unwrap()
            )
        );
    }

    #[test]
    fn a_gateway_outside_the_subnet_is_refused() {
        let address = Ipv4Addr::new(10, 0, 2, 15);
        let gateway = Ipv4Addr::new(10, 0, 3, 2);
        let link = Link {
            address,
            prefix: 24,
            gateway,
        };
        assert_eq!(
            Net::new(1730, link),
            Err(NetError::GatewayOutside {
                gateway,
                address,
                prefix: 24,
            })
        );
        Boot::read("ral.shares=virtiofs:s ral.mounts=3 ral.port=1 ral.epoch=0 ral.net=1730,10.0.2.15/24,10.0.3.2")
            .expect_err("a gateway outside the subnet must be refused");
    }

    #[test]
    fn a_malformed_net_key_is_refused() {
        for value in [
            "ral.net=1730,10.0.2.15/24",
            "ral.net=1730,not-an-address/24,10.0.2.2",
            "ral.net=1730,10.0.2.15/99,10.0.2.2",
            "ral.net=1730,10.0.2.15/24,not-a-gateway",
            "ral.net=0,10.0.2.15/24,10.0.2.2",
        ] {
            let err = Boot::read(&format!(
                "ral.shares=virtiofs:s ral.mounts=3 ral.port=1 ral.epoch=0 {value}"
            ))
            .expect_err(value);
            assert!(!err.is_empty(), "{value}");
        }
    }

    #[test]
    fn command_line_writes_only_the_new_share_and_mount_settings() {
        let mut boot = Boot {
            shares: Shares::Virtiofs {
                tag: "shares".into(),
            },
            mounts: 1733,
            port: 1729,
            epoch: 0,
            engine: DEFAULT_ENGINE.into(),
            net: None,
        };
        assert_eq!(
            command_line(&boot, "hvc0"),
            "console=hvc0 ral.shares=virtiofs:shares ral.mounts=1733 ral.port=1729 ral.epoch=0"
        );
        boot.shares = Shares::Plan9 { port: 564 };
        assert_eq!(
            command_line(&boot, "ttyS0"),
            "console=ttyS0 ral.shares=9p:564 ral.mounts=1733 ral.port=1729 ral.epoch=0"
        );
    }

    #[test]
    fn command_line_round_trips_through_read() {
        for boot in [
            Boot {
                shares: Shares::Virtiofs {
                    tag: "shares".into(),
                },
                mounts: 1733,
                port: 1729,
                epoch: 1_771_200_000,
                engine: DEFAULT_ENGINE.into(),
                net: None,
            },
            Boot {
                shares: Shares::Plan9 { port: 564 },
                mounts: 1733,
                port: 1729,
                epoch: 1_771_200_000,
                engine: "/opt/ral/engine".into(),
                net: Some(
                    Net::new(
                        1730,
                        Link {
                            address: Ipv4Addr::new(10, 0, 2, 15),
                            prefix: 24,
                            gateway: Ipv4Addr::new(10, 0, 2, 2),
                        },
                    )
                    .unwrap(),
                ),
            },
        ] {
            let rendered = command_line(&boot, "hvc0");
            let read_back = Boot::read(&rendered).expect("a rendered command line must parse");
            assert_eq!(read_back, boot, "{rendered}");
        }
    }
}
