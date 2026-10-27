//! Session configuration arrives once through whitespace-separated `ral.` kernel words.
//! Non-`ral.` words are ignored; repeated keys take their last value.
//! Unknown or valueless `ral.` keys are refused.

use std::{fmt, net::Ipv4Addr};

use crate::link::{LINK, Link};
use crate::ports;

pub const PREFIX: &str = "ral.";

/// Compiled by `media-manifest`, compared by `check-media`, both in build-tools.
pub const CONTRACT: u32 = 3;

/// The engine path defaults because it belongs to the image, not the session.
pub const DEFAULT_ENGINE: &str = "/usr/libexec/ral/engine";

/// Both export names obey virtiofs's stricter limit.
const MAX_TAG: usize = 36;

const ZERO_PORT: &str =
    "names no port; 0 is the kernel's wildcard, not something a guest can connect to.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Export {
    Virtiofs {
        tag: String,
    },
    Plan9 {
        name: String,
        /// The host's workspace transport port, distinct from [`Boot::port`].
        port: u32,
    },
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
    pub workspace: Export,
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
        let (mut workspace, mut plan9, mut port, mut epoch, mut engine, mut net) =
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
                "workspace" => workspace = Some(validate_name(value)?),
                "plan9" => plan9 = Some(parse_vsock_port("plan9", value)?),
                "port" => port = Some(parse_vsock_port("port", value)?),
                "epoch" => epoch = Some(parse_epoch(value)?),
                "engine" => engine = Some(validate_engine(value)?),
                "net" => net = Some(parse_net(value)?),
                _ => {
                    return Err(format!(
                        "the kernel command line carries `{PREFIX}{key}=…`, which ral-guest \
                         does not understand. The settings it knows are {PREFIX}workspace, \
                         {PREFIX}plan9, {PREFIX}port, {PREFIX}epoch, {PREFIX}engine, and \
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

        // Resolve the export after all keys: workspace and plan9 may arrive in either order.
        let name = workspace.ok_or_else(|| {
            format!(
                "the boot is missing `{PREFIX}workspace=<name>`, the name the host published the \
                 granted folder under — a virtiofs tag, or a 9p share's name when \
                 `{PREFIX}plan9` gives the port its server answers on. Without it the guest \
                 cannot find the one directory it is allowed to work in."
            )
        })?;

        Ok(Self {
            workspace: match plan9 {
                Some(port) => Export::Plan9 { name, port },
                None => Export::Virtiofs { tag: name },
            },
            port: port.ok_or_else(|| {
                format!(
                    "the boot is missing `{PREFIX}port=<port>`, the host's vsock port for the \
                     control plane — not `{PREFIX}plan9`, which is the workspace's transport. \
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
    match &boot.workspace {
        Export::Virtiofs { tag } => {
            let _ = write!(line, " {PREFIX}workspace={tag}");
        }
        Export::Plan9 { name, port } => {
            let _ = write!(line, " {PREFIX}workspace={name} {PREFIX}plan9={port}");
        }
    }
    let _ = write!(
        line,
        " {PREFIX}port={} {PREFIX}epoch={}",
        boot.port, boot.epoch
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

fn validate_name(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!(
            "`{PREFIX}workspace=` is empty; it must name the export the host published the \
             granted folder as — a virtiofs tag, or a 9p share's name."
        ));
    }
    if value.len() > MAX_TAG {
        return Err(format!(
            "`{PREFIX}workspace={value}` is {} characters; the workspace's name may be at most \
             {MAX_TAG}.",
            value.len()
        ));
    }
    if value.contains('/') {
        return Err(format!(
            "`{PREFIX}workspace={value}` looks like a path. It is the export's *name* — a word \
             the host and guest agree on, a virtiofs tag or a 9p share's `aname` — not a \
             directory."
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
            "`{PREFIX}net={value}` is not `<port>,<address>/<prefix>,<gateway>` — the host's \
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
            "console=hvc0 root=/dev/vda ro ral.workspace=work ral.port=1729 ral.epoch=1771200000",
        )
        .expect("a complete command line must parse");
        assert_eq!(
            boot,
            Boot {
                workspace: Export::Virtiofs { tag: "work".into() },
                port: 1729,
                epoch: 1_771_200_000,
                engine: DEFAULT_ENGINE.into(),
                net: None,
            }
        );
    }

    #[test]
    fn a_plan9_port_makes_the_workspace_a_share_the_guest_dials_for() {
        let boot = Boot::read(
            "console=ttyS0 ral.workspace=work ral.plan9=50001 ral.port=1729 ral.epoch=1771200000",
        )
        .expect("a Hyper-V command line must parse");
        assert_eq!(
            boot.workspace,
            Export::Plan9 {
                name: "work".into(),
                port: 50001,
            }
        );
        assert_eq!(
            boot.port, 1729,
            "the control plane keeps its own port; ral.plan9 never displaces ral.port"
        );
    }

    #[test]
    fn the_plan9_port_may_be_written_before_the_workspace_it_serves() {
        let before = Boot::read("ral.plan9=7 ral.workspace=w ral.port=1 ral.epoch=0")
            .expect("plan9 before workspace must parse");
        let after = Boot::read("ral.workspace=w ral.port=1 ral.epoch=0 ral.plan9=7")
            .expect("plan9 after workspace must parse");
        assert_eq!(before, after);
        assert_eq!(
            before.workspace,
            Export::Plan9 {
                name: "w".into(),
                port: 7
            }
        );
    }

    #[test]
    fn the_engine_path_may_be_overridden() {
        let boot = Boot::read("ral.workspace=w ral.port=1 ral.epoch=0 ral.engine=/opt/ral/engine")
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
        let missing_workspace =
            Boot::read("ral.port=1 ral.epoch=0").expect_err("workspace is required");
        assert!(
            missing_workspace.contains("ral.workspace"),
            "{missing_workspace}"
        );

        let missing_port = Boot::read("ral.workspace=w ral.epoch=0").expect_err("port is required");
        assert!(missing_port.contains("ral.port"), "{missing_port}");

        let missing_epoch =
            Boot::read("ral.workspace=w ral.port=1").expect_err("epoch is required");
        assert!(missing_epoch.contains("ral.epoch"), "{missing_epoch}");
    }

    #[test]
    fn an_unknown_ral_setting_is_refused() {
        let err = Boot::read("ral.workspace=w ral.port=1 ral.epoch=0 ral.netowrk=on")
            .expect_err("a misspelled setting must be refused");
        assert!(err.contains("does not understand"), "{err}");
        for known in [
            "ral.workspace",
            "ral.plan9",
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
        let boot = Boot::read("ral.workspace=first ral.port=1 ral.epoch=0 ral.workspace=second")
            .expect("a repeated setting must parse");
        assert_eq!(
            boot.workspace,
            Export::Virtiofs {
                tag: "second".into()
            }
        );
    }

    #[test]
    fn a_workspace_name_is_a_short_pathless_word() {
        for line in [
            "ral.workspace=/work ral.port=1 ral.epoch=0",
            "ral.workspace=/work ral.plan9=7 ral.port=1 ral.epoch=0",
        ] {
            let slash = Boot::read(line).expect_err("a path is not a name");
            assert!(slash.contains("export's *name*"), "{slash}");
        }

        let long = format!(
            "ral.workspace={} ral.port=1 ral.epoch=0",
            "t".repeat(MAX_TAG + 1)
        );
        let err = Boot::read(&long).expect_err("an over-long name must be refused");
        assert!(err.contains("at most"), "{err}");
    }

    #[test]
    fn a_vsock_port_of_zero_is_refused_whichever_setting_carries_it() {
        let control = Boot::read("ral.workspace=w ral.port=0 ral.epoch=0").expect_err("port 0");
        assert!(control.contains("wildcard"), "{control}");
        assert!(control.contains("ral.port=0"), "{control}");

        let workspace =
            Boot::read("ral.workspace=w ral.plan9=0 ral.port=1 ral.epoch=0").expect_err("plan9 0");
        assert!(workspace.contains("wildcard"), "{workspace}");
        assert!(
            workspace.contains("ral.plan9=0"),
            "the refusal names the setting the host wrote: {workspace}"
        );
    }

    #[test]
    fn a_negative_epoch_is_refused() {
        let err = Boot::read("ral.workspace=w ral.port=1 ral.epoch=-1").expect_err("before 1970");
        assert!(err.contains("before 1970"), "{err}");
    }

    #[test]
    fn a_relative_engine_path_is_refused() {
        let err = Boot::read("ral.workspace=w ral.port=1 ral.epoch=0 ral.engine=engine")
            .expect_err("a relative engine path must be refused");
        assert!(err.contains("absolute path"), "{err}");
    }

    #[test]
    fn a_net_key_reads_into_its_four_fields() {
        let boot =
            Boot::read("ral.workspace=w ral.port=1 ral.epoch=0 ral.net=1730,10.0.2.15/24,10.0.2.2")
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
        let err =
            Boot::read("ral.workspace=w ral.port=1 ral.epoch=0 ral.net=1730,10.0.2.15/24,10.0.3.2")
                .expect_err("a gateway outside the subnet must be refused");
        assert_eq!(
            err,
            "`ral.net=1730,10.0.2.15/24,10.0.3.2` names a gateway, 10.0.3.2, that is not inside \
             10.0.2.15/24; the guest could never route to it. Fix the prefix or the gateway."
        );
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
            let err = Boot::read(&format!("ral.workspace=w ral.port=1 ral.epoch=0 {value}"))
                .expect_err(value);
            assert!(!err.is_empty(), "{value}");
        }
    }

    #[test]
    fn command_line_round_trips_through_read() {
        for boot in [
            Boot {
                workspace: Export::Virtiofs { tag: "work".into() },
                port: 1729,
                epoch: 1_771_200_000,
                engine: DEFAULT_ENGINE.into(),
                net: None,
            },
            Boot {
                workspace: Export::Plan9 {
                    name: "work".into(),
                    port: 50001,
                },
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
