//! Booting a `Shell` in a host process — the interactive `ral` REPL,
//! `exarch`, a test binary.
//!
//! Probing the host *machine* (cwd, user, home, XDG bases) is [`crate::host`];
//! evaluating on the booted shell is [`crate::run`].
//!
//! The prelude arrives baked: a host's build script calls
//! [`bake_prelude_to_out_dir`], and the host embeds the blob it wrote with
//! [`baked_prelude!`](crate::baked_prelude).  Core cannot bake its own — a
//! build script cannot depend on the crate it is building — so a test
//! binary takes [`BakedPrelude::runtime`] instead.

use crate::evaluator::{Mode, run_phrases};
use crate::ir::{CompKind, Phrase, Toplevel};
use crate::terminal::TerminalState;
use crate::typecheck::Manifest;
use crate::types::{
    Break, BuiltinEntry, BuiltinTable, Env, Escape, Mooring, PreludeMap, Set, Shell,
    language_constants,
};
use std::sync::{Arc, OnceLock};

/// A prelude baked ahead of time.
///
/// The annotated [`Toplevel`] is a postcard blob that decodes and memoises on
/// first access; the map of values running it yields is evaluated once and
/// [seated](Self::seat) in every shell.
pub struct BakedPrelude {
    ir: &'static [u8],
    comp: OnceLock<Arc<Toplevel>>,
    map: OnceLock<Arc<PreludeMap>>,
}

impl BakedPrelude {
    /// Wrap the blob a host embedded via [`baked_prelude!`](crate::baked_prelude).
    /// `const` so a host can hold its prelude in a `static`.
    pub const fn from_blob(ir: &'static [u8]) -> Self {
        Self {
            ir,
            comp: OnceLock::new(),
            map: OnceLock::new(),
        }
    }

    /// The annotated prelude toplevel.
    ///
    /// # Panics
    /// Panics if the embedded IR blob fails to deserialize.
    pub fn comp(&self) -> &Arc<Toplevel> {
        self.comp.get_or_init(|| {
            Arc::new(postcard::from_bytes(self.ir).expect("prelude IR deserialization failed"))
        })
    }

    /// Seat the prelude's bindings as `shell`'s Σ prelude tier, whole.
    ///
    /// The prelude runs once per bake, on a core-only shell: every phrase is a
    /// `Define` of `Return(V)` ([`validate_prelude_shape`]), so the run is a
    /// fold of closing values, and the resulting map, frozen, is the one every
    /// shell seated from this bake starts from.
    ///
    /// # Panics
    /// Panics if the prelude fails to run: a partial prelude is never seated.
    pub fn seat(&self, shell: &mut Shell) {
        let map = self.map.get_or_init(|| self.evaluate());
        Arc::make_mut(&mut shell.sig).install_prelude(Arc::clone(map));
    }

    fn evaluate(&self) -> Arc<PreludeMap> {
        let mut shell = HostSurface::default().shell(TerminalState::default());
        let ran = run_phrases(
            &self.comp().phrases,
            Env::new(),
            Mode::Prelude,
            &Mooring::adrift(),
            &mut shell,
        );
        if let Err(brk) = ran.outcome {
            let why = match brk {
                Break::Error(e) => e.to_string(),
                Break::Escape(Escape::Exit(code)) => format!("exit {code}"),
            };
            panic!("the prelude failed to run: {why}");
        }
        Arc::new(
            ran.env
                .iter()
                .map(|(name, binding)| (name.to_string(), binding.clone()))
                .collect(),
        )
    }

    /// The prelude baked at runtime from core's own source, for a test binary
    /// with no build-time blob, once per process.
    #[cfg(any(test, feature = "test-util"))]
    pub fn runtime() -> &'static Self {
        static RUNTIME: OnceLock<BakedPrelude> = OnceLock::new();
        RUNTIME.get_or_init(|| {
            let this = Self::from_blob(&[]);
            let _ = this.comp.set(Arc::new(bake()));
            this
        })
    }
}

/// Parse, elaborate and check `prelude.ral` against core's manifest alone,
/// and vet its shape.
///
/// # Panics
/// Panics if the embedded `prelude.ral` fails to parse, elaborate, or
/// type-check, or has a phrase of the wrong shape.
fn bake() -> Toplevel {
    let ast = crate::syntax::parser::parse(include_str!("prelude.ral"))
        .unwrap_or_else(|e| panic!("prelude parse error: {e}"));
    let top = crate::elaborator::elaborate(&ast, [], "")
        .unwrap_or_else(|e| panic!("prelude elaborate error: {e}"));
    let annotated = crate::bake_prelude(&top, &HostSurface::default().manifest());
    validate_prelude_shape(&annotated);
    annotated
}

/// Reject a prelude phrase that is not a `Define` of `Return(V)` — a literal
/// or a thunk — naming the bound name(s).
///
/// The wire ships the prelude tier by name alone, never by value: a
/// re-exec'd engine child (`hatch`) re-derives it by running the same
/// prelude source under its own process.  A `Define` whose right-hand side
/// is anything but `Return` — an `if`, a call, a store read — could close
/// over *this* process's facts (its stdout, its args, its clock) and so
/// differ from the child's own bake, silently.  `Return(V)` cannot: closing
/// a literal or a thunk reads nothing about the process it runs in.
///
/// # Panics
/// If any phrase fails the shape check.
fn validate_prelude_shape(top: &Toplevel) {
    for phrase in &top.phrases {
        let Phrase::Define { comp, schemes, .. } = &phrase.item else {
            panic!(
                "prelude phrase must be a `Define`, found {}",
                describe_phrase(&phrase.item)
            );
        };
        if !matches!(comp.item, CompKind::Return(_)) {
            let names: Vec<&str> = schemes.iter().map(|(n, _)| n.as_str()).collect();
            panic!(
                "prelude binding `{}` is computed at boot ({}), so it could differ between \
                 processes; bind a value or a thunk",
                names.join(", "),
                describe_comp(&comp.item),
            );
        }
    }
}

/// A phrase's shape, named for [`validate_prelude_shape`]'s message.
fn describe_phrase(phrase: &Phrase) -> &'static str {
    match phrase {
        Phrase::Define { .. } => "a `Define`",
        Phrase::Run(_) => "a bare statement",
    }
}

/// A computation's shape, named for [`validate_prelude_shape`]'s message.
fn describe_comp(kind: &CompKind) -> &'static str {
    match kind {
        CompKind::If { .. } => "`if …`",
        CompKind::Case { .. } => "`case …`",
        CompKind::Tilde(_) => "a `~` path",
        CompKind::App { .. } => "a call",
        CompKind::Exec(_) => "a command",
        CompKind::Bind { .. } => "`to`",
        CompKind::Force(_) => "`force`",
        _ => "a computation",
    }
}

/// Expand in a host crate whose build script called
/// [`bake_prelude_to_out_dir`](crate::boot::bake_prelude_to_out_dir).
///
/// A macro, because `include_bytes!` has to expand against the *host's*
/// `OUT_DIR`; keeping it here keeps the filenames beside the writer.
#[macro_export]
macro_rules! baked_prelude {
    () => {
        $crate::boot::BakedPrelude::from_blob(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/prelude_baked.bin"
        )))
    };
}

/// A host's builtin surface beyond [`CORE_SETS`](crate::builtins::CORE_SETS).
///
/// One value serves the runtime ([`Self::shell`], [`boot_shell`]) and
/// shell-free typechecking ([`Self::manifest`]), so the checker's surface and
/// the runtime's cannot drift.  `Default` is bare core.
#[derive(Default)]
pub struct HostSurface {
    pub statics: Vec<&'static [BuiltinEntry]>,
    /// Sets a host built at run time, closing over its own state.
    pub captured: Vec<Arc<[BuiltinEntry]>>,
}

impl HostSurface {
    /// Core first, then the host's own: exactly what a shell built from this
    /// surface dispatches.
    fn sets(&self) -> impl Iterator<Item = Set> + '_ {
        crate::builtins::CORE_SETS
            .into_iter()
            .chain(self.statics.iter().copied())
            .map(Set::Static)
            .chain(self.captured.iter().cloned().map(Set::Captured))
    }

    /// Core plus every set here, bodies and all.
    ///
    /// # Panics
    /// Panics if a name here collides with a core builtin or repeats.
    pub fn builtin_table(&self) -> BuiltinTable {
        let mut table = BuiltinTable::default();
        for set in self.sets() {
            table.install(set);
        }
        table
    }

    /// The checker's Σ for this surface: what [`bake_prelude`](crate::bake_prelude)
    /// and [`SessionSchemes::from_prelude`](crate::typecheck::SessionSchemes)
    /// read where there is no live shell (`--check`).
    pub fn manifest(&self) -> Manifest {
        self.builtin_table().manifest().clone()
    }

    /// Core plus this surface, no env, no prelude: a loader's scaffold.
    ///
    /// # Panics
    /// Panics if a name here collides with a core builtin or repeats.
    pub fn shell(&self, terminal: TerminalState) -> Shell {
        let mut shell = Shell::root(terminal);
        for set in self.sets() {
            shell.install_set(&set);
        }
        // Language-given names live in Σ, ahead of the prelude.
        Arc::make_mut(&mut shell.sig).install_natives(language_constants());
        shell
    }
}

/// A shell as every front end boots one: `surface` installed, the env seeded
/// from the host, the prelude seated.
///
/// `surface` lands before any rc file or user code is checked, so the
/// typechecker and the runtime agree by construction.  Terminal handling,
/// output capture, watchdogs, capability frames and rc files stay the host's,
/// interposed around this call.
///
/// # Panics
/// Panics if a name in `surface` collides with a core builtin or repeats
/// within the surface.
pub fn boot_shell(terminal: TerminalState, prelude: &BakedPrelude, surface: &HostSurface) -> Shell {
    let mut shell = surface.shell(terminal);
    seed_env(&mut shell);
    prelude.seat(&mut shell);
    shell
}

/// Adopt the host process's env and cwd onto a fresh shell, defaulting
/// anything unset.
///
/// Ral code reads these as `!{env}[KEY]`, so every front end sees one
/// baseline whoever launched the process.  `SHLVL` is incremented rather than
/// passed through, as in every other shell.  `PWD` is no variable: it is the
/// cwd cell, which `apply_env` in `core/src/runtime/command/process.rs`
/// threads into each child.  A host fact nothing binds stays unbound: seeding
/// `HOME=.` once made every `~` in the session mean "here".
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: seeding the baseline `env` at boot; the host process env is the source the overlay later shadows"
)]
fn seed_env(shell: &mut Shell) {
    let user = crate::host::user();
    let shlvl = std::env::var("SHLVL")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    let base = [
        ("HOME", crate::host::home()),
        ("USER", user.clone()),
        (
            "PATH",
            inherited("PATH", || {
                Some(if cfg!(windows) {
                    "C:\\Windows\\System32;C:\\Windows;C:\\Windows\\System32\\Wbem".into()
                } else {
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()
                })
            }),
        ),
        (
            "SHELL",
            inherited("SHELL", || {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.into_os_string().into_string().ok())
                    .or_else(|| Some("ral".into()))
            }),
        ),
        ("TERM", inherited("TERM", || Some("xterm-256color".into()))),
        ("LANG", inherited("LANG", || Some("C.UTF-8".into()))),
        ("LOGNAME", inherited("LOGNAME", || user.clone())),
    ];
    let forwarded = [
        "TMUX",
        "TMUX_PANE",
        "STY",
        "COLORTERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
    ]
    .map(|key| (key, std::env::var(key).ok()));
    // Compile-time facts, in `env` so rc can branch on the machine without
    // shelling out to `uname`.
    let facts = [
        ("OS_NAME", std::env::consts::OS.to_string()),
        ("OS_ARCH", std::env::consts::ARCH.to_string()),
        ("OS_FAMILY", std::env::consts::FAMILY.to_string()),
        ("SHLVL", shlvl.to_string()),
    ];
    shell.context.extend_env(
        base.into_iter()
            .chain(forwarded)
            .filter_map(|(key, value)| Some((key, value?)))
            .chain(facts),
    );
    if let Some(cwd) = crate::host::cwd() {
        shell.seed_cwd(cwd);
    }
}

/// The host's `key`, else `default()` where the host binds nothing. A value
/// that is not UTF-8 is `None`: no overlay entry, so children inherit its
/// bytes rather than a default in their place.
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: seeding the baseline `env` at boot"
)]
fn inherited(key: &str, default: impl FnOnce() -> Option<String>) -> Option<String> {
    match std::env::var(key) {
        Ok(v) => Some(v),
        Err(std::env::VarError::NotPresent) => default(),
        Err(std::env::VarError::NotUnicode(_)) => None,
    }
}

/// The build-script half of the bake: write the postcard blob into the
/// calling host's `OUT_DIR`.
///
/// postcard carries no schema, so a field added to
/// [`CompKind`](crate::ir::CompKind), [`Val`](crate::ir::Val),
/// [`Pattern`](crate::ir::Pattern), or the scheme's type
/// vocabulary would silently invalidate an old bake.  No rerun line guards
/// it: `ral-core` is a build-dependency of each host, so a change to it
/// recompiles the build script, which reruns and bakes afresh.  A file the
/// script read from disk without core compiling it would need a line.
///
/// # Panics
/// Panics if the prelude fails to parse, elaborate or type-check, if `OUT_DIR`
/// is unset, or if serialising or writing the blob fails: a build script has
/// no better way to report.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:prelude-bake] build-script prelude bake: writes the postcard IR blob to OUT_DIR during host setup; build-time artifact emission, not turn-time model data I/O, raises no surface card."
)]
pub fn bake_prelude_to_out_dir() {
    let ir_bytes = postcard::to_allocvec(&bake()).expect("prelude IR serialization failed");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("prelude_baked.bin"), ir_bytes)
        .expect("failed to write prelude_baked.bin");
}

#[cfg(test)]
mod tests {
    use super::{BakedPrelude, HostSurface, validate_prelude_shape};
    use std::sync::Arc;

    /// A bake of `src`, checked against core alone but not shape-vetted, so a
    /// test can hand `seat` a prelude the real bake would refuse.
    fn baked(src: &str) -> BakedPrelude {
        let ast = crate::syntax::parser::parse(src).expect("parse");
        let top = crate::elaborator::elaborate(&ast, [], "").expect("elaborate");
        let this = BakedPrelude::from_blob(&[]);
        let annotated = crate::bake_prelude(&top, &HostSurface::default().manifest());
        let _ = this.comp.set(Arc::new(annotated));
        this
    }

    /// Two bakes in one process each seat their own map: the evaluated prelude
    /// belongs to the bake, never to the process.
    #[test]
    fn two_bakes_seat_two_preludes() {
        let (first, second) = (baked("let only_first = 1"), baked("let only_second = 2"));
        let (mut a, mut b) = (
            crate::test_helper::core_shell(),
            crate::test_helper::core_shell(),
        );
        first.seat(&mut a);
        second.seat(&mut b);
        assert!(a.sig.prelude_binding("only_first").is_some());
        assert!(a.sig.prelude_binding("only_second").is_none());
        assert!(b.sig.prelude_binding("only_first").is_none());
        assert!(b.sig.prelude_binding("only_second").is_some());
    }

    /// A prelude that fails to run is a bug in the bake, never a partial
    /// prelude seated.
    #[test]
    #[should_panic(expected = "the prelude failed to run: exit 3")]
    fn a_prelude_that_fails_to_run_panics() {
        baked("exit 3").seat(&mut crate::test_helper::core_shell());
    }

    /// A prelude phrase whose RHS is computed at boot — an `if`, here — fails
    /// the bake, naming the bound name.
    #[test]
    #[should_panic(expected = "prelude binding `x` is computed at boot")]
    fn non_return_prelude_binding_fails_the_bake() {
        let ast =
            crate::syntax::parser::parse("let x = if !{_ansi-ok} { return 1 } else { return 2 }")
                .expect("parse");
        let top = crate::elaborator::elaborate(&ast, [], "").expect("elaborate");
        let annotated = crate::bake_prelude(&top, &HostSurface::default().manifest());
        validate_prelude_shape(&annotated);
    }

    /// A prelude phrase whose RHS is `Return(V)` — a literal or a thunk —
    /// passes the bake.
    #[test]
    fn return_prelude_binding_passes_the_bake() {
        let ast =
            crate::syntax::parser::parse("let x = 1\nlet f = { |y| return $y }").expect("parse");
        let top = crate::elaborator::elaborate(&ast, [], "").expect("elaborate");
        let annotated = crate::bake_prelude(&top, &HostSurface::default().manifest());
        validate_prelude_shape(&annotated);
    }
}
