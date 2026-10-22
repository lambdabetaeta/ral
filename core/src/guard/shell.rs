//! The guard's `Shell` face: the live dynamic context handed to the
//! decisions in [`super::enforce`], their refusals recorded at one door, and
//! the scope guards that push and pop grant layers.  A layer holding no
//! opinion on an effect abstains, so a check passes unless some layer
//! withholds.

use super::enforce::{
    Denial, check_device_name, check_exec, check_flag, check_fs_exact, fs_denial,
};
use crate::capability::{Admitted, Flag, Floor, FsOp, GrantLayer, GrantStack, Program};
use crate::path::walk::Kind;
use crate::path::{
    Dir, Judged, LeafMode, LexicalPath, Located, Op, Trace, walk, walk_dir, walk_lenient,
};
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

    /// The floor every fs judgment of this shell reads: its runtime's.
    pub fn floor(&self) -> &Floor {
        self.session.runtime.floor()
    }

    /// Judge `path` as a name, walked leniently, for `O`: what a probe asks
    /// where the walk found nothing, so a denied name that does not exist
    /// reads as denied, not as absent.  A [discard device](LexicalPath::is_discard)
    /// is exempt from both regions, and a [reserved device name](check_device_name)
    /// is refused before either, asked before the walk, since the question is
    /// about the name and not about what is on the disk under it.
    fn check_named<O: Op>(&mut self, path: &LexicalPath) -> Settled<()> {
        if path.is_discard() {
            return Ok(());
        }
        check_device_name(path)?;
        let trace = walk_lenient(path, LeafMode::Follow)
            .map_err(|e| crate::types::Error::io(path.shown(), &e))?;
        let verdict = check_fs_exact(&self.context, self.floor(), &trace, &O::FS);
        self.judged(verdict)
    }

    /// An object a door already holds, judged for `O` by the live stack: the
    /// absent leaf a name ral invents is, before anything makes it.
    ///
    /// # Errors
    /// The grant's refusal.
    pub fn judge<O: Op>(&mut self, located: Located) -> Settled<Located<Judged<O>>> {
        let real = located.real().clone();
        let verdict = (self.context.grants)
            .judge::<O>(self.floor(), located)
            .map_err(|refused| fs_denial(&self.context, refused, &O::FS, &real));
        self.judged(verdict)
    }

    /// The fs door: walk `path` to the object it names, symlink-free, and
    /// authorise *that object* for `O`.  The handle handed back performs
    /// every operation relative to the object's directory handle, so what was
    /// judged is what gets opened — a dangling link is judged at its target,
    /// and a component swapped after the judgment is never followed.  Every
    /// write in ral comes through here.
    ///
    /// # Errors
    /// The walk's I/O error, phrased with the name as written; the grant's
    /// refusal; or the refusal of a reserved device name.
    pub fn locate<O: Op>(&mut self, path: &LexicalPath) -> Settled<Located<Judged<O>>> {
        check_device_name(path)?;
        let located =
            walk(path, LeafMode::Follow).map_err(|e| crate::types::Error::io(path.shown(), &e))?;
        self.judge(located)
    }

    /// [`locate`](Self::locate) for a directory entered: `walk_dir`, then the
    /// directory judged.
    ///
    /// # Errors
    /// The walk's I/O error, phrased with the name as written; the grant's
    /// refusal; or the refusal of a reserved device name.
    pub fn locate_dir<O: Op>(&mut self, path: &LexicalPath) -> Settled<Dir<Judged<O>>> {
        check_device_name(path)?;
        let dir = walk_dir(path).map_err(|e| crate::types::Error::io(path.shown(), &e))?;
        let real = dir.real().clone();
        let verdict = (self.context.grants)
            .judge_dir::<O>(self.floor(), dir)
            .map_err(|refused| fs_denial(&self.context, refused, &O::FS, &real));
        self.judged(verdict)
    }

    /// [`locate`](Self::locate) for a probe, which must tell *absent* from
    /// *refused*: `Ok(None)` where nothing is at the name, so `exists` and
    /// its siblings answer `false` rather than raising, while a grant refusal
    /// is still an `Err`.
    ///
    /// The refusal is judged on what the walk found, the absent leaf
    /// included, and on the name where the walk found nothing — otherwise a
    /// denied path that does not exist would leak the difference by reading
    /// as merely absent.
    ///
    /// # Errors
    /// The grant's refusal, on the object or on the name; or the refusal of a
    /// reserved device name.
    pub fn locate_existing<O: Op>(
        &mut self,
        path: &LexicalPath,
        leaf: LeafMode,
    ) -> Settled<Option<Located<Judged<O>>>> {
        check_device_name(path)?;
        let Ok(located) = walk(path, leaf) else {
            self.check_named::<O>(path)?;
            return Ok(None);
        };
        let held = self.judge::<O>(located)?;
        Ok(held.stat().is_some().then_some(held))
    }

    /// The first of `candidates` that is a regular file the live stack admits
    /// for `O`, held.  Each walked candidate is judged before its kind is
    /// asked, present or absent, so which one is refused never says which
    /// exists; a denied candidate is passed over, as `execvp` does, and the
    /// first denial is the error where none is admitted; `Ok(None)` where none
    /// is refused and none is an admitted file.
    ///
    /// # Errors
    /// The first refused candidate, where none was admitted; or the refusal
    /// of a reserved device name.
    pub fn locate_first<O: Op>(
        &mut self,
        candidates: impl IntoIterator<Item = LexicalPath>,
    ) -> Settled<Option<Located<Judged<O>>>> {
        let mut first = None;
        for cand in candidates {
            check_device_name(&cand)?;
            let Ok(located) = walk(&cand, LeafMode::Follow) else {
                continue;
            };
            let real = located.real().clone();
            match (self.context.grants).judge::<O>(self.floor(), located) {
                Ok(found) if found.trace().kind() == Some(Kind::File) => return Ok(Some(found)),
                Ok(_) => {}
                Err(refused) => {
                    first.get_or_insert_with(|| fs_denial(&self.context, refused, &O::FS, &real));
                }
            }
        }
        first.map_or(Ok(None), |denial| Err(self.refused(*denial)))
    }

    /// [`locate`](Self::locate) as a predicate: `None` where the walk fails
    /// or the grant refuses, so a scan skips what it may not read instead of
    /// aborting.  One off-limits entry must not blank a whole listing.
    pub fn locate_if_admitted<O: Op>(&mut self, path: &LexicalPath) -> Option<Located<Judged<O>>> {
        let located = walk(path, LeafMode::Follow).ok()?;
        (self.context.grants).judge::<O>(self.floor(), located).ok()
    }

    /// Whether the live stack admits `op` on an access already walked — no
    /// audit, no refusal: the question a door asks about a *side* read it
    /// may simply forgo, such as a write's before-image.
    pub fn admits_fs(&self, op: &FsOp, trace: &Trace) -> bool {
        self.context.grants.admits_fs(self.floor(), op, trace)
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
