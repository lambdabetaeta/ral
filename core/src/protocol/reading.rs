//! Readings: every class a probe may ask, typed once — its label, its payload
//! rule, the engine's answer, and the host's decode — so the two ends of a
//! probe cannot disagree about what a class means.
//!
//! An answer outside its class's shape is the engine breaking the protocol,
//! and severs the transport that carried it.

use std::path::{Path, PathBuf};

use super::{Ending, ProbeError, Report, Severed, Transport};
use crate::serial::FOValue;
use crate::serial::datum::{self, Datum};
use crate::types::Shell;

mod fs;
mod rows;
mod source;

pub use rows::{
    BindEffect, BindingRow, CompletionNames, HandleRow, PathEntry, Spine, SpineError, SpineStage,
    WorkerRow,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    BindingCount,
    LeasedBindingCount,
    LargestBindingBytes,
    EnvVar,
    Cwd,
    Home,
    BuiltinNames,
    PathBytes,
    Workers,
    SessionEnded,
    CompletionNames,
    Bindings,
    Spine,
    BindEffects,
    PathEntries,
    #[cfg(feature = "test-util")]
    WorkerCount,
    #[cfg(feature = "test-util")]
    GrantDepth,
}

const CLASSES: &[(&str, Class)] = &[
    ("binding-count", Class::BindingCount),
    ("leased-binding-count", Class::LeasedBindingCount),
    ("largest-binding-bytes", Class::LargestBindingBytes),
    ("env-var", Class::EnvVar),
    ("cwd", Class::Cwd),
    ("home", Class::Home),
    ("builtin-names", Class::BuiltinNames),
    ("path-bytes", Class::PathBytes),
    ("workers", Class::Workers),
    ("session-ended", Class::SessionEnded),
    ("completion-names", Class::CompletionNames),
    ("bindings", Class::Bindings),
    ("spine", Class::Spine),
    ("bind-effects", Class::BindEffects),
    ("path-entries", Class::PathEntries),
    #[cfg(feature = "test-util")]
    ("worker-count", Class::WorkerCount),
    #[cfg(feature = "test-util")]
    ("grant-depth", Class::GrantDepth),
];

impl Class {
    fn label(self) -> &'static str {
        CLASSES
            .iter()
            .find_map(|&(label, class)| (class == self).then_some(label))
            .expect("every class has a label")
    }

    fn named(label: &str) -> Option<Self> {
        CLASSES
            .iter()
            .find_map(|&(name, class)| (name == label).then_some(class))
    }

    /// The classes that read a string payload — a name, a path, a source text
    /// — rather than none.
    fn reads_string(self) -> bool {
        matches!(
            self,
            Self::EnvVar | Self::PathBytes | Self::Spine | Self::BindEffects | Self::PathEntries
        )
    }
}

// ── The engine's answer ───────────────────────────────────────────────

/// Answer one probe against `shell`.
///
/// # Errors
/// A request that is not a variant, an unknown class, or a payload its class
/// does not take — each named.
pub(crate) fn answer(shell: &Shell, req: &FOValue) -> Result<FOValue, String> {
    let FOValue::Variant { label, payload } = req else {
        return Err(format!(
            "a probe must be a variant naming its class, such as `cwd, but got {}",
            req.shape()
        ));
    };
    let class = Class::named(label).ok_or_else(|| format!("unknown probe class `{label}"))?;
    let arg = match (class.reads_string(), payload.as_deref()) {
        (true, Some(FOValue::String { value })) => value.as_str(),
        (true, other) => {
            return Err(format!(
                "the `{label} probe reads a string payload, but got {}",
                other.map_or_else(|| "none".to_string(), FOValue::shape)
            ));
        }
        (false, None) => "",
        (false, Some(other)) => {
            return Err(format!(
                "the `{label} probe takes no payload, but got {}",
                other.shape()
            ));
        }
    };
    Ok(match class {
        Class::BindingCount => shell.binding_count().encode(),
        Class::LeasedBindingCount => shell.leased_binding_count().encode(),
        Class::LargestBindingBytes => shell.largest_binding_shallow_size().encode(),
        Class::EnvVar => shell.env_var(arg).encode(),
        Class::Cwd => shell.cwd().display().to_string().encode(),
        Class::Home => shell.context.home().encode(),
        Class::BuiltinNames => shell
            .builtin_names()
            .map(str::to_string)
            .collect::<Vec<_>>()
            .encode(),
        Class::PathBytes => fs::tree_bytes(&shell.cwd().join(arg)).encode(),
        Class::Workers => shell
            .workers()
            .iter()
            .map(WorkerRow::of)
            .collect::<Vec<_>>()
            .encode(),
        Class::SessionEnded => shell
            .durable_root()
            .as_scope()
            .cause()
            .map(|cause| crate::types::Status::Cancelled(cause).code())
            .encode(),
        Class::CompletionNames => CompletionNames::of(shell).encode(),
        Class::Bindings => BindingRow::all(shell).encode(),
        Class::Spine => source::spine(shell, arg).encode(),
        Class::BindEffects => source::bind_effects(shell, arg).encode(),
        Class::PathEntries => fs::entries(&shell.cwd().join(arg)).encode(),
        #[cfg(feature = "test-util")]
        Class::WorkerCount => shell.worker_count().encode(),
        #[cfg(feature = "test-util")]
        Class::GrantDepth => shell.grant_depth().encode(),
    })
}

// ── The probe's wire form ─────────────────────────────────────────────

/// A probe's answer as the wire engine reports it, under the probe's own
/// `DispatchId`.
#[cfg(unix)]
pub(crate) fn report(answer: Result<FOValue, String>) -> Report {
    let ending = match answer {
        Ok(value) => Ending::Settled { value, status: 0 },
        Err(rendered) => Ending::Raised {
            record: FOValue::try_from(&crate::evaluator::scope::error_record(
                "<probe>",
                &crate::types::Status::Raised(1),
                &rendered,
                None,
            ))
            .expect("an error record is data"),
            rendered,
            command_exit: false,
            single_command: false,
            status: 1.into(),
        },
    };
    Report::Ran {
        ending,
        captured: None,
        trail: Vec::new(),
    }
}

/// [`report`]'s inverse, on the front-end.
pub(crate) fn unreport(report: Report) -> Result<FOValue, ProbeError> {
    match report {
        Report::Ran {
            ending: Ending::Settled { value, .. },
            ..
        } => Ok(value),
        Report::Ran {
            ending: Ending::Raised { rendered, .. } | Ending::Walled { rendered, .. },
            ..
        }
        | Report::Static { rendered, .. } => Err(ProbeError::Rejected(rendered)),
        Report::Ran { ending, .. } => Err(ProbeError::Rejected(format!(
            "probe answered abnormally: {ending:?}"
        ))),
    }
}

// ── The host's decode ─────────────────────────────────────────────────

/// Ask `class` of `t`, and decode its answer as a `T`; an answer outside that
/// shape severs `t`.
pub(crate) fn read<T: Datum>(
    t: &dyn Transport,
    class: Class,
    arg: Option<&str>,
) -> Result<T, ProbeError> {
    let answer = t.probe(datum::tag(
        class.label(),
        arg.map(|value| value.to_string().encode()),
    ))?;
    T::decode(&answer).map_err(|why| {
        ProbeError::Severed(t.sever(Severed::Faulted(format!(
            "the `{} probe answered outside its shape: {why}",
            class.label()
        ))))
    })
}

/// The engine's logical cwd.
///
/// # Errors
/// [`ProbeError`]: a refusal, or the severance an ill-shaped answer causes.
pub fn cwd(t: &dyn Transport) -> Result<PathBuf, ProbeError> {
    read::<String>(t, Class::Cwd, None).map(PathBuf::from)
}

/// The engine's `$HOME`, through its own env overlay.
///
/// # Errors
/// As [`cwd`].
pub fn home(t: &dyn Transport) -> Result<Option<PathBuf>, ProbeError> {
    read::<Option<String>>(t, Class::Home, None).map(|home| home.map(PathBuf::from))
}

/// One variable of the engine's environment: its overlay over its own process
/// env, which across a wire is not this process's.
///
/// # Errors
/// As [`cwd`].
pub fn env_var(t: &dyn Transport, name: &str) -> Result<Option<String>, ProbeError> {
    read(t, Class::EnvVar, Some(name))
}

/// Every builtin the engine's shell resolves, internals included.
///
/// # Errors
/// As [`cwd`].
pub fn builtin_names(t: &dyn Transport) -> Result<Vec<String>, ProbeError> {
    read(t, Class::BuiltinNames, None)
}

/// The recursive byte size of `path` in the engine's own filesystem, resolved
/// against its cwd.
///
/// # Errors
/// As [`cwd`].
pub fn path_bytes(t: &dyn Transport, path: &Path) -> Result<u64, ProbeError> {
    read(t, Class::PathBytes, Some(&path.to_string_lossy()))
}

/// How many bindings the session holds.
///
/// # Errors
/// As [`cwd`].
pub fn binding_count(t: &dyn Transport) -> Result<u64, ProbeError> {
    read(t, Class::BindingCount, None)
}

/// How many bindings the lease ledger governs.
///
/// # Errors
/// As [`cwd`].
pub fn leased_binding_count(t: &dyn Transport) -> Result<u64, ProbeError> {
    read(t, Class::LeasedBindingCount, None)
}

/// The shallow byte estimate of the session's largest binding.
///
/// # Errors
/// As [`cwd`].
pub fn largest_binding_bytes(t: &dyn Transport) -> Result<u64, ProbeError> {
    read(t, Class::LargestBindingBytes, None)
}

/// The engine's worker table.
///
/// # Errors
/// As [`cwd`].
pub fn workers(t: &dyn Transport) -> Result<Vec<WorkerRow>, ProbeError> {
    read(t, Class::Workers, None)
}

/// The exit status of whatever ended the session — its durable root's cancel
/// cause — or `None` while it lives.
///
/// # Errors
/// As [`cwd`].
pub fn session_ended(t: &dyn Transport) -> Result<Option<i32>, ProbeError> {
    read(t, Class::SessionEnded, None)
}

/// The binding and handler names in scope, sorted.
///
/// # Errors
/// As [`cwd`].
pub fn completion_names(t: &dyn Transport) -> Result<CompletionNames, ProbeError> {
    read(t, Class::CompletionNames, None)
}

/// Every binding in scope, rendered.
///
/// # Errors
/// As [`cwd`].
pub fn bindings(t: &dyn Transport) -> Result<Vec<BindingRow>, ProbeError> {
    read(t, Class::Bindings, None)
}

/// What `src` types as against the live session, unrun.
///
/// # Errors
/// As [`cwd`].
pub fn spine(t: &dyn Transport, src: &str) -> Result<Spine, ProbeError> {
    read(t, Class::Spine, Some(src))
}

/// The effect verdict of each top-level `let` in `src`, unrun.
///
/// # Errors
/// As [`cwd`].
pub fn bind_effects(t: &dyn Transport, src: &str) -> Result<Vec<BindEffect>, ProbeError> {
    read(t, Class::BindEffects, Some(src))
}

/// The entries of `dir` in the engine's own filesystem, resolved against its
/// cwd.
///
/// # Errors
/// As [`cwd`].
pub fn path_entries(t: &dyn Transport, dir: &Path) -> Result<Vec<PathEntry>, ProbeError> {
    read(t, Class::PathEntries, Some(&dir.to_string_lossy()))
}
