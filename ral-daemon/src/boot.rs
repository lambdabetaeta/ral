//! The boot configuration, as the host wrote it on the kernel command line.
//!
//! The guest has no configuration file, no environment, and no network: at
//! the moment the daemon starts, the *only* thing the host has told it is
//! the kernel command line.  Everything session-specific therefore arrives
//! there, under a `ral.` prefix, and is read exactly once — here — into a
//! [`Boot`] that the rest of the daemon treats as immutable fact.
//!
//! Parsing is a pure function of a string, so the whole contract between
//! this daemon and the VM backend that writes the command line is testable
//! without a virtual machine.
//!
//! Kernel command-line convention applies: words are whitespace-separated,
//! unknown words (the kernel's own `console=`, `root=`, …) are ignored, and
//! a key given twice takes its last value.  A `ral.` key this daemon does
//! not know is *not* ignored — a setting the host meant and the guest
//! silently dropped is exactly the kind of quiet divergence a boot should
//! refuse.
//!
//! ## Why the guest is told how its workspace arrives
//!
//! `dev/docs/VM/SYNOD.md` §2 gives synod two lifecycle backends over one
//! guest, and they do not agree about shared filesystems.  A
//! Virtualization.framework machine can hand a live directory to the guest,
//! so the granted folder arrives as a virtiofs export and the guest mounts
//! it by the tag the host published.  A Hyper-V machine has no virtiofs at
//! all, so the host runs a 9p (9p2000.L) server and the guest reaches it by
//! opening an `AF_VSOCK` stream to the host on a port the host names — the
//! same arrangement Microsoft's own LCOW guest agent uses
//! (`microsoft/hcsshim`, `internal/guest/storage/plan9`).
//!
//! Which of the two is in force is therefore a property of the *hypervisor
//! the host chose*, and the host is the only party that knows it before the
//! machine exists.  A guest could in principle inspect its own buses and
//! guess; it is told instead, on the command line, because a contract stated
//! once cannot be inferred wrongly twice.  [`Export`] is that telling, and
//! it is a sum type rather than a pair of loosely-related fields because
//! exactly one of the two arrangements is ever the case.
//!
//! ## Two vsock ports, which are not the same port
//!
//! [`Boot::port`] is the *control plane*: where the daemon dials the host to
//! get the engine's protocol socket (`dev/docs/VM/SYNOD.md` §3).  The port
//! inside [`Export::Plan9`] is the *workspace transport*: where the host's
//! 9p server answers.  They are written by the same host, both are host-side
//! `AF_VSOCK` ports, and confusing them yields a boot in which the engine
//! talks 9p at the session manager — so they are named apart everywhere,
//! `ral.port` and `ral.plan9`.

/// The prefix every setting meant for this daemon carries.
pub const PREFIX: &str = "ral.";

/// Where the engine binary lives in the boot artifact, when the command
/// line does not say otherwise.  It is a property of the image, not of the
/// session, so unlike every other setting it has a default.
pub const DEFAULT_ENGINE: &str = "/usr/libexec/ral/engine";

/// The longest word the host and guest may agree on as the workspace's name.
///
/// It governs both arrangements: a virtiofs tag and a 9p share's `aname` are
/// the same kind of thing — one word, agreed in advance, carrying no path.
/// 36 is the virtiofs device's own limit, and 9p sets no comparable one, so
/// the stricter of the two rules is the rule for both.
const MAX_TAG: usize = 36;

/// How the granted folder reaches the guest.
///
/// One variant per hypervisor's answer to "can the host share a directory
/// live?", as the module docs explain.  The guest never chooses; it is told,
/// and `crate::mounts::plan` turns the telling into the one mount at
/// `crate::mounts::WORK`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Export {
    /// The granted folder as a virtiofs export, named by the tag the host
    /// published.  The mount source is that tag rather than any device: the
    /// host and guest agree only through this word.
    Virtiofs {
        /// The tag the host's filesystem device was configured with.
        tag: String,
    },
    /// The granted folder as a 9p share the host serves on a vsock port.
    /// The guest dials the port, and the connection *is* the transport: no
    /// block device and no filesystem device is involved at all.
    Plan9 {
        /// The share's name: the word 9p's `attach` carries as `aname`, and
        /// the only thing that tells the server which tree it is serving.
        name: String,
        /// The host-side `AF_VSOCK` port the 9p server answers on.  Not
        /// [`Boot::port`]; see the module docs.
        port: u32,
    },
}

/// Everything the host told this guest at boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boot {
    /// How the granted folder arrives, and under what name.  The guest
    /// mounts it at `crate::mounts::WORK` whichever shape it takes.
    pub workspace: Export,
    /// The host's `AF_VSOCK` port for the control plane.  The daemon
    /// connects there and hands the connection to the engine as its
    /// protocol socket.
    pub port: u32,
    /// The host's wall clock at boot, in seconds since the Unix epoch.
    /// The guest has no clock worth trusting until this is applied.
    pub epoch: i64,
    /// Absolute path to the ral/exarch multicall binary to run under
    /// `--engine`.
    pub engine: String,
}

impl Boot {
    /// Read the configuration out of a kernel command line.
    ///
    /// # Errors
    /// Returns a sentence naming what the host must fix when the command
    /// line carries no `ral.` settings at all (this kernel was not booted
    /// by a ral VM backend), when a required setting is missing, when a
    /// value does not parse, or when an unknown `ral.` key appears.
    pub fn read(cmdline: &str) -> Result<Self, String> {
        let (mut workspace, mut plan9, mut port, mut epoch, mut engine) =
            (None, None, None, None, None);
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
                _ => {
                    return Err(format!(
                        "the kernel command line carries `{PREFIX}{key}=…`, which this daemon \
                         does not understand. The settings it knows are {PREFIX}workspace, \
                         {PREFIX}plan9, {PREFIX}port, {PREFIX}epoch, and {PREFIX}engine."
                    ));
                }
            }
        }

        if !addressed_to_us {
            return Err(format!(
                "this kernel was not booted by a ral virtual-machine manager: its command line \
                 carries no `{PREFIX}` settings at all. ral-daemon is the init of a synod or \
                 exarch guest and has nothing to do here."
            ));
        }

        // The name is one word whichever shape the export takes, and
        // `ral.plan9` is what decides the shape — so the two are read
        // independently and joined only here, once the whole line has been
        // seen.  Nothing earlier would do: either word may be the last one on
        // the command line, and the last spelling of a key wins.
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
        })
    }
}

/// Accept the workspace's name: a non-empty, path-free word the host and
/// guest both spell the same way, short enough for [`MAX_TAG`].
///
/// One rule serves both arrangements deliberately.  The name is validated
/// when it is read, which may be *before* `ral.plan9` has been seen, so
/// there is no shape to specialise on yet — and there is no reason to want
/// one: a virtiofs tag and a 9p `aname` are both agreed words, and a value
/// that would be a mistake as one is a mistake as the other.
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

/// Accept a vsock port under the key that carried it: any non-zero 32-bit
/// port.  Zero is the kernel's "any port" and cannot be connected to.
///
/// `ral.port` and `ral.plan9` are both host-side `AF_VSOCK` ports and are
/// read by this one function; `key` travels with the value only so that the
/// refusal quotes back the setting the host actually wrote.
fn parse_vsock_port(key: &str, value: &str) -> Result<u32, String> {
    match value.parse::<u32>() {
        Ok(0) => Err(format!(
            "`{PREFIX}{key}=0` names no port; 0 is the kernel's wildcard, not something a guest \
             can connect to."
        )),
        Ok(port) => Ok(port),
        Err(err) => Err(format!(
            "`{PREFIX}{key}={value}` is not a vsock port number: {err}."
        )),
    }
}

/// Accept a wall clock: seconds since the Unix epoch, at or after it.
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

/// Accept an engine path: absolute, because the daemon has no working
/// directory worth resolving against and no `PATH` to search.
fn validate_engine(value: &str) -> Result<String, String> {
    if value.starts_with('/') {
        return Ok(value.to_string());
    }
    Err(format!(
        "`{PREFIX}engine={value}` is not an absolute path. The daemon runs before anything has a \
         meaningful working directory, so the engine must be named from the root."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command line as the Virtualization.framework backend would write
    /// it: the kernel's own words are ignored, ours are read, and a line
    /// with no `ral.plan9` means the workspace arrives as a virtiofs export
    /// — byte-for-byte the arrangement that predates the Hyper-V backend.
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
            }
        );
    }

    /// `ral.plan9` is the whole of what tells the guest it is under a
    /// hypervisor with no virtiofs: the workspace's name becomes a 9p share
    /// name, and the port names the host's server.
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

    /// The two ports are independent settings, and either order of writing
    /// them yields the same boot: kernel words carry no sequence.
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

    /// The engine path is the one setting with a default, because it is a
    /// property of the image rather than of the session.
    #[test]
    fn the_engine_path_may_be_overridden() {
        let boot = Boot::read("ral.workspace=w ral.port=1 ral.epoch=0 ral.engine=/opt/ral/engine")
            .expect("an overridden engine path must parse");
        assert_eq!(boot.engine, "/opt/ral/engine");
    }

    /// A command line with no `ral.` settings is not a ral guest's, and the
    /// refusal says so rather than complaining about a missing key.
    #[test]
    fn a_command_line_without_ral_settings_is_not_a_ral_guest() {
        let err = Boot::read("console=ttyAMA0 root=/dev/vda").expect_err("not our kernel");
        assert!(
            err.contains("not booted by a ral virtual-machine manager"),
            "{err}"
        );
    }

    /// Each required setting names itself, and says why it cannot be
    /// guessed, when it is absent.
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

    /// An unknown `ral.` key is refused, not ignored: a setting the host
    /// meant and the guest dropped is a silent divergence.
    #[test]
    fn an_unknown_ral_setting_is_refused() {
        let err = Boot::read("ral.workspace=w ral.port=1 ral.epoch=0 ral.netowrk=on")
            .expect_err("a misspelled setting must be refused");
        assert!(err.contains("does not understand"), "{err}");
        for known in ["ral.workspace", "ral.plan9", "ral.port", "ral.epoch"] {
            assert!(
                err.contains(known),
                "the refusal lists every known key, and not {known}: {err}"
            );
        }
    }

    /// A `ral.` word with no `=` names no value.
    #[test]
    fn a_valueless_ral_word_is_refused() {
        let err = Boot::read("ral.quiet").expect_err("a valueless setting must be refused");
        assert!(err.contains("names no value"), "{err}");
    }

    /// Kernel convention: the last spelling of a key wins.
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

    /// The workspace's name is a name, not a path, and fits [`MAX_TAG`] —
    /// under both arrangements, because a virtiofs tag and a 9p `aname` are
    /// the same kind of agreed word.
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

    /// Port zero is the kernel's wildcard and cannot be connected to — and
    /// that holds of every vsock port on the line, not just the control
    /// plane's, since both are dialled the same way.
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

    /// A clock before 1970 is a mistake, not a session.
    #[test]
    fn a_negative_epoch_is_refused() {
        let err = Boot::read("ral.workspace=w ral.port=1 ral.epoch=-1").expect_err("before 1970");
        assert!(err.contains("before 1970"), "{err}");
    }

    /// The engine is named from the root; the daemon has no `PATH`.
    #[test]
    fn a_relative_engine_path_is_refused() {
        let err = Boot::read("ral.workspace=w ral.port=1 ral.epoch=0 ral.engine=engine")
            .expect_err("a relative engine path must be refused");
        assert!(err.contains("absolute path"), "{err}");
    }
}
