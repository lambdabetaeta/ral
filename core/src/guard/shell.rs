//! The guard's `Shell` face: the live dynamic context handed to the
//! decisions in [`super::enforce`], their refusals recorded at one door, and
//! the scope guards that push and pop grant layers.  A layer holding no
//! opinion on an effect abstains, so a check passes unless some layer
//! withholds.

use super::enforce::{Denial, check_device_name, check_exec, check_flag, check_fs_exact};
use crate::capability::{Admitted, Flag, Floor, FsOp, GrantLayer, GrantStack, Program};
use crate::path::{LeafMode, LexicalPath, Located, Trace, walk_lenient};
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

    /// Check `exec` for `program`, shown as `shown`.  The [`Admitted`] is what
    /// the launcher demands.
    ///
    /// # Errors
    /// `Err` if the active grant denies the program.
    pub(crate) fn check_exec(
        &mut self,
        shown: &str,
        program: Program,
        args: Vec<String>,
    ) -> Settled<Admitted> {
        let verdict = check_exec(&self.context, self.floor(), shown, program, args);
        self.judged(verdict)
    }

    /// Check an fs read *by name* — a predicate, a listing, a module load.
    /// `path` comes from `Shell::resolve`, so the guard's sole input is
    /// already cwd-anchored; its `..` is read physically.  Anything that opens the
    /// name, for reading or writing, takes [`Self::locate`] instead.
    ///
    /// # Errors
    /// `Err` if, at some layer with an `fs` opinion, `path` falls under a
    /// `deny_paths` entry or outside every read prefix.
    pub fn check_fs_read(&mut self, path: &LexicalPath) -> Settled<()> {
        self.check_named(path, &FsOp::Read)
    }

    /// The floor every fs judgment of this shell reads: its runtime's.
    pub fn floor(&self) -> &Floor {
        self.session.runtime.floor()
    }

    /// Judge `op` on `path` as a name, walked leniently.  A
    /// [discard device](LexicalPath::is_discard) is exempt from both regions,
    /// and a [reserved device name](check_device_name) is refused before
    /// either, asked before the walk, since the question is about the name and
    /// not about what is on the disk under it.
    fn check_named(&mut self, path: &LexicalPath, op: &FsOp) -> Settled<()> {
        if path.is_discard() {
            return Ok(());
        }
        check_device_name(path)?;
        let trace = walk_lenient(path, LeafMode::Follow)
            .map_err(|e| crate::types::Error::io(path.shown(), &e))?;
        let verdict = check_fs_exact(&self.context, self.floor(), &trace, op);
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
        let located = crate::path::walk(path, LeafMode::Follow)
            .map_err(|e| crate::types::Error::io(path.shown(), &e))?;
        if !path.is_discard() {
            let verdict = check_fs_exact(&self.context, self.floor(), located.trace(), op);
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
        leaf: LeafMode,
    ) -> Settled<Option<Located>> {
        check_device_name(path)?;
        let Ok(located) = crate::path::walk(path, leaf) else {
            self.check_named(path, op)?;
            return Ok(None);
        };
        let verdict = check_fs_exact(&self.context, self.floor(), located.trace(), op);
        self.judged(verdict)?;
        Ok(Some(located))
    }

    /// [`locate`](Self::locate) as a predicate: `None` where the walk fails
    /// or the grant refuses, so a scan skips what it may not read instead of
    /// aborting.  One off-limits entry must not blank a whole listing.
    pub fn locate_if_admitted(&mut self, path: &LexicalPath, op: &FsOp) -> Option<Located> {
        let located = crate::path::walk(path, LeafMode::Follow).ok()?;
        self.admits_fs(op, located.trace()).then_some(located)
    }

    /// Whether the live stack admits `op` on an access already walked — no
    /// audit, no refusal: the question a door asks about a *side* read it
    /// may simply forgo, such as a write's before-image.
    pub fn admits_fs(&self, op: &FsOp, trace: &Trace) -> bool {
        self.context.grants.admits_fs(self.floor(), op, trace)
    }

    /// Whether the live stack admits `op` on `path` as a name, walked
    /// leniently: what a listing asks of each entry it may show.  A name that
    /// cannot be walked is not admitted.
    pub fn admits_named(&self, op: &FsOp, path: &LexicalPath) -> bool {
        walk_lenient(path, LeafMode::Follow).is_ok_and(|trace| self.admits_fs(op, &trace))
    }

    /// The OS-renderable projection of the live capability stack; `None` when
    /// no layer restricts enough to need an OS sandbox at all.
    pub fn sandbox_projection(&self) -> Option<SandboxProjection> {
        SandboxProjection::of(&self.context.grants, self.floor(), None)
    }

    /// Run `f` with `layer` pushed for its dynamic extent.  The single gate
    /// into capability-checked code — `grant { … }` blocks and plugin hook /
    /// keybinding / alias dispatch all funnel through here.  The push sits on
    /// top of the caller's stack, so effective authority is always caller ∩
    /// this layer.
    pub fn with_layer<R>(&mut self, layer: GrantLayer, f: impl FnOnce(&mut Self) -> R) -> R {
        self.with_pushed(vec![layer], f)
    }

    /// [`with_layer`](Self::with_layer) for a whole ceiling: every layer of
    /// `stack` is pushed for `f`'s dynamic extent and popped after — never
    /// folded into one layer, since the stack is the meet.
    pub(crate) fn with_layers<R>(
        &mut self,
        stack: GrantStack,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.with_pushed(stack.into_iter().collect(), f)
    }

    fn with_pushed<R>(&mut self, layers: Vec<GrantLayer>, f: impl FnOnce(&mut Self) -> R) -> R {
        let (at, depth) = (self.context.grants.len(), layers.len());
        for layer in layers {
            self.context.grants.push(layer);
        }
        self.audit_deputy_prefixes();
        let r = f(self);
        self.context.grants.remove(at, depth);
        r
    }

    /// Push a layer with no paired pop: it survives to process exit.  Where
    /// `ral --capabilities <file.ral>`'s session-wide ceiling lands, above the
    /// [`GrantLayer::root`] layer [`Shell::root`] installs.  Lexical
    /// attenuation (`grant {}`) wants [`Self::with_layer`].
    pub fn push_session_capabilities(&mut self, layer: GrantLayer) {
        self.context.grants.push(layer);
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
        let prefixes = crate::capability::deputy_prefixes(&self.context.grants, self.floor());
        for prefix in prefixes {
            let fields = BTreeMap::from([("prefix".to_string(), prefix.shown())]);
            self.record_check(None, Check::new(Resource::Deputy, fields));
        }
    }

    /// True when a non-root capabilities layer is active.
    pub(crate) fn has_active_capabilities(&self) -> bool {
        self.context.grants.is_restrictive()
    }
}
