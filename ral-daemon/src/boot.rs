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

/// The prefix every setting meant for this daemon carries.
pub const PREFIX: &str = "ral.";

/// Where the engine binary lives in the boot artifact, when the command
/// line does not say otherwise.  It is a property of the image, not of the
/// session, so unlike every other setting it has a default.
pub const DEFAULT_ENGINE: &str = "/usr/libexec/ral/engine";

/// The longest tag a virtiofs mount source may carry.
const MAX_TAG: usize = 36;

/// Everything the host told this guest at boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boot {
    /// The virtiofs tag naming the granted folder's device.  The guest
    /// mounts it at [`crate::mounts::WORK`]; the host chooses the tag and
    /// the two agree only through this word.
    pub workspace: String,
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
        let (mut workspace, mut port, mut epoch, mut engine) = (None, None, None, None);
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
                "workspace" => workspace = Some(validate_tag(value)?),
                "port" => port = Some(parse_port(value)?),
                "epoch" => epoch = Some(parse_epoch(value)?),
                "engine" => engine = Some(validate_engine(value)?),
                _ => {
                    return Err(format!(
                        "the kernel command line carries `{PREFIX}{key}=…`, which this daemon \
                         does not understand. The settings it knows are {PREFIX}workspace, \
                         {PREFIX}port, {PREFIX}epoch, and {PREFIX}engine."
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

        Ok(Self {
            workspace: workspace.ok_or_else(|| {
                format!(
                    "the boot is missing `{PREFIX}workspace=<tag>`, the virtiofs tag under which \
                     the host exported the granted folder. Without it the guest cannot find the \
                     one directory it is allowed to work in."
                )
            })?,
            port: port.ok_or_else(|| {
                format!(
                    "the boot is missing `{PREFIX}port=<port>`, the host's vsock port for the \
                     control plane. There is no safe default: a guessed port would leave the \
                     engine waiting on a connection nobody is listening for."
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

/// Accept a virtiofs tag: a non-empty, path-free word the host and guest
/// both spell the same way, short enough for the virtiofs device's own
/// limit.
fn validate_tag(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!(
            "`{PREFIX}workspace=` is empty; it must name the virtiofs tag the host exported the \
             granted folder under."
        ));
    }
    if value.len() > MAX_TAG {
        return Err(format!(
            "`{PREFIX}workspace={value}` is {} characters; a virtiofs tag may be at most \
             {MAX_TAG}.",
            value.len()
        ));
    }
    if value.contains('/') {
        return Err(format!(
            "`{PREFIX}workspace={value}` looks like a path. It is a virtiofs *tag* — a name the \
             host and guest agree on — not a directory."
        ));
    }
    Ok(value.to_string())
}

/// Accept a vsock port: any non-zero 32-bit port.  Zero is the kernel's
/// "any port" and cannot be connected to.
fn parse_port(value: &str) -> Result<u32, String> {
    match value.parse::<u32>() {
        Ok(0) => Err(format!(
            "`{PREFIX}port=0` names no port; 0 is the kernel's wildcard, not something a guest \
             can connect to."
        )),
        Ok(port) => Ok(port),
        Err(err) => Err(format!(
            "`{PREFIX}port={value}` is not a vsock port number: {err}."
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

    /// A command line as a VM backend would write it: the kernel's own
    /// words are ignored, ours are read.
    #[test]
    fn a_full_command_line_reads_into_a_boot() {
        let boot = Boot::read(
            "console=hvc0 root=/dev/vda ro ral.workspace=work ral.port=1729 ral.epoch=1771200000",
        )
        .expect("a complete command line must parse");
        assert_eq!(
            boot,
            Boot {
                workspace: "work".into(),
                port: 1729,
                epoch: 1_771_200_000,
                engine: DEFAULT_ENGINE.into(),
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
        assert!(
            err.contains("ral.workspace"),
            "the refusal lists the known keys: {err}"
        );
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
        assert_eq!(boot.workspace, "second");
    }

    /// The workspace tag is a name, not a path, and fits the virtiofs limit.
    #[test]
    fn a_workspace_tag_is_a_short_pathless_name() {
        let slash = Boot::read("ral.workspace=/work ral.port=1 ral.epoch=0")
            .expect_err("a path is not a tag");
        assert!(slash.contains("virtiofs *tag*"), "{slash}");

        let long = format!(
            "ral.workspace={} ral.port=1 ral.epoch=0",
            "t".repeat(MAX_TAG + 1)
        );
        let err = Boot::read(&long).expect_err("an over-long tag must be refused");
        assert!(err.contains("at most"), "{err}");
    }

    /// Port zero is the kernel's wildcard and cannot be connected to.
    #[test]
    fn port_zero_is_refused() {
        let err = Boot::read("ral.workspace=w ral.port=0 ral.epoch=0").expect_err("port 0");
        assert!(err.contains("wildcard"), "{err}");
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
