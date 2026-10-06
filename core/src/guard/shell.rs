//! The guard's `Shell` face: the live dynamic context handed to the
//! decisions in [`super::enforce`], their refusals recorded at one door, and
//! the scope guards that push and pop grant layers.  A layer holding no
//! opinion on an effect abstains, so a check passes unless some layer
//! withholds.

use super::enforce::{Denial, check_device_name, check_exec, check_flag, check_fs_exact};
use crate::capability::{Admitted, Capabilities, Flag, FsOp, GrantStack, Program};
use crate::path::walk::Leaf;
use crate::path::{LexicalPath, Located};
use crate::sandbox::SandboxProjection;
use crate::types::{Break, Check, Resource, Settled, Shell};
use std::collections::BTreeMap;

impl Shell {
    /// Check a boolean capability: `Err` if a layer of the active stack
    /// withholds `flag`, naming `who` in the refusal.
    ///
    /// # Errors
    /// `Err` if a layer of the active stack withholds it.
    pub fn check(&self, flag: Flag, who: &str) -> Settled<()> {
        check_flag(&self.context, flag, who)
    }

    /// Record `check` and mint the `Break` that refuses.  Its callers hold no
    /// `Mooring`, so the denial reaches the trail alone.
    fn refused(&mut self, Denial { check, error }: Denial) -> Break {
        self.record_check(None, check);
        error.into()
    }

    fn judged<T>(&mut self, verdict: Result<T, Box<Denial>>) -> Settled<T> {
        verdict.map_err(|denial| self.refused(*denial))
    }

    /// Check `exec` for `program`, shown as `shown`, then hold `args` to any
    /// subcommand restriction.  The [`Admitted`] is what the launcher demands.
    ///
    /// # Errors
    /// `Err` if the active grant denies the program, or admits only a
    /// subcommand set that `args`'s first element misses.
    pub(crate) fn check_exec(
        &mut self,
        shown: &str,
        program: Program,
        args: Vec<String>,
    ) -> Settled<Admitted> {
        let verdict = check_exec(&self.context, shown, program, args);
        self.judged(verdict)
    }

    /// Check an fs read *by name* — a predicate, a listing, a module load.
    /// `path` comes from `Shell::resolve`, so the guard's sole input is
    /// already cwd-anchored and `.`/`..`-collapsed.  Anything that opens the
    /// name, for reading or writing, takes [`Self::locate`] instead.
    ///
    /// # Errors
    /// `Err` if, at some layer with an `fs` opinion, `path` falls under a
    /// `deny_paths` entry or outside every read prefix.
    pub fn check_fs_read(&mut self, path: &LexicalPath) -> Settled<()> {
        self.check_named(path, &FsOp::Read)
    }

    /// Judge `op` on `path` as a name, canonicalised leniently.  A
    /// [discard device](LexicalPath::is_discard) is exempt from both regions,
    /// and a [reserved device name](check_device_name) is refused before
    /// either, asked before canonicalisation, since the question is about the
    /// name and not about what is on the disk under it.
    fn check_named(&mut self, path: &LexicalPath, op: &FsOp) -> Settled<()> {
        if path.is_discard() {
            return Ok(());
        }
        check_device_name(path)?;
        let verdict = check_fs_exact(&self.context, &path.canonicalise_lenient(), op);
        self.judged(verdict)
    }

    /// The fs door: walk `path` to the object it names, symlink-free, and
    /// authorise *that object* for `op`.  The [`Located`] handed back
    /// performs every operation relative to the object's directory handle,
    /// so what was judged is what gets opened — a dangling link is judged at
    /// its target, and a component swapped after the judgment is never
    /// followed.  Every write in ral comes through here.
    ///
    /// # Errors
    /// The walk's I/O error, phrased with the name as written; the grant's
    /// refusal; or the refusal of a reserved device name.
    pub fn locate(&mut self, path: &LexicalPath, op: &FsOp) -> Settled<Located> {
        check_device_name(path)?;
        let located = crate::path::walk::walk(path, Leaf::Resolve)
            .map_err(|e| crate::types::Error::io(path.display(), &e))?;
        if !path.is_discard() {
            let verdict = check_fs_exact(&self.context, located.real(), op);
            self.judged(verdict)?;
        }
        Ok(located)
    }

    /// [`locate`](Self::locate) for a probe, which must tell *absent* from
    /// *refused*: `Ok(None)` where the walk finds nothing, so `exists` and its
    /// siblings answer `false` rather than raising, while a grant refusal is
    /// still an `Err`.
    ///
    /// The refusal is judged on the object where there is one, and on the
    /// name where the walk found nothing — otherwise a denied path that does
    /// not exist would leak the difference by reading as merely absent.
    ///
    /// # Errors
    /// The grant's refusal, on the object or on the name; or the refusal of a
    /// reserved device name.
    pub(crate) fn locate_existing(
        &mut self,
        path: &LexicalPath,
        op: &FsOp,
        leaf: Leaf,
    ) -> Settled<Option<Located>> {
        check_device_name(path)?;
        let Ok(located) = crate::path::walk::walk(path, leaf) else {
            self.check_named(path, op)?;
            return Ok(None);
        };
        let verdict = check_fs_exact(&self.context, located.real(), op);
        self.judged(verdict)?;
        Ok(Some(located))
    }

    /// [`locate`](Self::locate) as a predicate: `None` where the walk fails
    /// or the grant refuses, so a scan skips what it may not read instead of
    /// aborting.  One off-limits entry must not blank a whole listing.
    pub fn locate_if_admitted(&mut self, path: &LexicalPath, op: &FsOp) -> Option<Located> {
        let located = crate::path::walk::walk(path, Leaf::Resolve).ok()?;
        self.admits_fs_exact(op, located.real()).then_some(located)
    }

    /// Whether the live stack admits `op` on a path already located — no
    /// audit, no refusal: the question a door asks about a *side* read it
    /// may simply forgo, such as a write's before-image.
    pub fn admits_fs_exact(&self, op: &FsOp, real: &std::path::Path) -> bool {
        self.context
            .grants
            .admits_fs_exact(op, &self.context.resolver(), real)
    }

    /// The OS-renderable projection of the live capability stack; `None` when
    /// no layer restricts enough to need an OS sandbox at all.
    pub fn sandbox_projection(&self) -> Option<SandboxProjection> {
        SandboxProjection::of(&self.context.grants, &self.context.resolver(), None)
    }

    /// Run `f` with `capabilities` pushed for its dynamic extent.  The single
    /// gate into capability-checked code — `grant { … }` blocks and plugin
    /// hook / keybinding / alias dispatch all funnel through here.  The push
    /// sits on top of the caller's stack, so effective authority is always
    /// caller ∩ this layer.
    pub fn with_capabilities<R>(
        &mut self,
        capabilities: Capabilities,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.with_layers(GrantStack::of(capabilities), f)
    }

    /// [`with_capabilities`](Self::with_capabilities) for a whole ceiling: every
    /// layer of `stack` is pushed for `f`'s dynamic extent and popped after —
    /// never folded into one frame, since the stack is the meet.
    pub(crate) fn with_layers<R>(
        &mut self,
        stack: GrantStack,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let (at, depth) = (self.context.grants.len(), stack.len());
        for layer in stack {
            self.context.grants.push(layer);
        }
        self.audit_deputy_prefixes();
        let r = f(self);
        self.context.grants.remove(at, depth);
        r
    }

    /// Push a capability frame with no paired pop: it survives to process
    /// exit.  Where `ral --capabilities <file.ral>`'s session-wide ceiling
    /// lands, above the [`Capabilities::root`] frame [`Shell::root`] installs.
    /// Lexical attenuation (`grant {}`) wants [`Self::with_capabilities`].
    pub fn push_session_capabilities(&mut self, capabilities: Capabilities) {
        self.context.grants.push(capabilities);
        self.audit_deputy_prefixes();
    }

    /// Observe a `deputy` capability check per flagged prefix of the stack
    /// just pushed.  [`crate::capability::deputy_prefixes`] takes the stack
    /// and only reports, never denies.  No-op unless a trail is open; a
    /// `Flagged` check has no rail branch, and the scope guards hold no
    /// `Mooring`.
    pub(crate) fn audit_deputy_prefixes(&mut self) {
        if !self.local.audit.active() {
            return;
        }
        for prefix in crate::capability::deputy_prefixes(&self.context.grants) {
            let fields = BTreeMap::from([("prefix".to_string(), prefix.as_str().to_string())]);
            self.record_check(None, Check::new(Resource::Deputy, fields));
        }
    }

    /// True when a non-root capabilities layer is active.
    pub(crate) fn has_active_capabilities(&self) -> bool {
        self.context.grants.is_restrictive()
    }
}
