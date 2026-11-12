//! The shell's authority boundary: its live grant stack handed to
//! [`crate::authority::guard`], refusals recorded at one door, and scope
//! guards that push and pop grant layers. A layer holding no opinion on an
//! effect abstains, so a check passes unless some layer withholds.

use crate::authority::guard::{Denial, check_device_name, check_flag, fs_denial};
use crate::authority::policy::{Baseline, Flag, FsOp, GrantLayer, LocateError};
use crate::authority::sandbox::SandboxProjection;
use crate::fact::{IoOperation, IoSubject, Reason};
use crate::path::is_loop;
use crate::path::{Dir, Judged, LeafMode, LexicalPath, Located, Op, Trace, walk};
use crate::shell::runtime::{Break, Check, Error, Mooring, Resource, Settled, Shell};
use std::collections::BTreeMap;
use std::io;

impl Shell {
    /// Check a boolean capability: `Err` if a layer of the active stack
    /// withholds `flag`, naming `who` in the refusal.
    ///
    /// # Errors
    /// `Err` if a layer of the active stack withholds it.
    pub fn check(&self, flag: Flag, who: &str) -> Settled<()> {
        check_flag(&self.context.grants, flag, who)
            .map_err(|denial| Error::new(Reason::Denied(Box::new(denial))).into())
    }

    /// Record `check` and mint the `Break` that refuses.  Its callers hold no
    /// `Mooring`, so the denial reaches the trail alone.
    fn refused(&mut self, denial: Denial) -> Break {
        if let Some(check) = denial.audit_check() {
            self.record_check(None, check);
        }
        Error::new(Reason::Denied(Box::new(denial))).into()
    }

    /// Record an exec `denial` against `mooring` and mint the `Break` that refuses.
    pub(crate) fn exec_refused(&mut self, denial: Denial, mooring: &Mooring) -> Break {
        if let Some(check) = denial.audit_check() {
            self.record_check(Some(mooring), check);
        }
        Error::new(Reason::Denied(Box::new(denial))).into()
    }

    /// The baseline every fs judgment of this shell reads: its runtime's.
    pub fn baseline(&self) -> &Baseline {
        self.session.runtime.baseline()
    }

    /// An object a door already holds, judged for `O` by the live stack: the
    /// absent leaf a name ral invents is, before anything makes it.
    ///
    /// # Errors
    /// The grant's refusal.
    pub fn judge<O: Op>(&mut self, located: Located) -> Settled<Located<Judged<O>>> {
        let path = self.anchor(located.real().as_path());
        self.judge_named::<O>(located, &path)
    }

    fn judge_named<O: Op>(
        &mut self,
        located: Located,
        path: &LexicalPath,
    ) -> Settled<Located<Judged<O>>> {
        let verdict = (self.context.grants)
            .judge::<O, _>(self.baseline(), located)
            .map_err(|refused| fs_denial(&self.context.grants, refused, O::FS, path));
        verdict.map_err(|denial| self.refused(denial))
    }

    /// The stack's verdict on `path` raised: a refusal worded and recorded; a
    /// walk failure phrased with the name as written.
    fn raised<O: Op, T>(
        &mut self,
        path: &LexicalPath,
        verdict: Result<T, LocateError>,
    ) -> Settled<T> {
        match verdict {
            Ok(found) => Ok(found),
            Err(LocateError::Io(e)) => Err(Self::walk_failure(path, &e)),
            Err(LocateError::Refused(refused)) => {
                let denial = fs_denial(&self.context.grants, refused, O::FS, path);
                Err(self.refused(denial))
            }
        }
    }

    fn walk_failure(path: &LexicalPath, e: &io::Error) -> Break {
        Error::io(
            IoOperation::Walk,
            IoSubject::Path(crate::text::quote_native(path.as_path().as_os_str())),
            e,
        )
        .into()
    }

    /// The fs door: walk `path` to the object it names, symlink-free, and
    /// authorise *that object* for `O`.  The handle handed back performs
    /// every operation relative to the object's directory handle, so what was
    /// judged is what gets opened — a dangling link is judged at its target,
    /// and a component swapped after the judgment is never followed.  Every
    /// write in ral comes through here.
    ///
    /// # Errors
    /// The grant's refusal, of the name where the walk fails; else the walk's
    /// I/O error, phrased with the name as written; or the refusal of a
    /// reserved device name.
    pub fn locate<O: Op>(&mut self, path: &LexicalPath) -> Settled<Located<Judged<O>>> {
        check_device_name(path).map_err(|denial| Error::new(Reason::Denied(Box::new(denial))))?;
        let verdict = (self.context.grants).locate::<O, Located>(self.baseline(), path);
        self.raised::<O, _>(path, verdict)
    }

    /// [`locate`](Self::locate) for a directory entered.
    ///
    /// # Errors
    /// As [`locate`](Self::locate).
    pub fn locate_dir<O: Op>(&mut self, path: &LexicalPath) -> Settled<Dir<Judged<O>>> {
        check_device_name(path).map_err(|denial| Error::new(Reason::Denied(Box::new(denial))))?;
        let verdict = (self.context.grants).locate::<O, Dir>(self.baseline(), path);
        self.raised::<O, _>(path, verdict)
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
        check_device_name(path).map_err(|denial| Error::new(Reason::Denied(Box::new(denial))))?;
        let located = match walk(path, leaf) {
            Ok(located) => located,
            Err(e) => {
                let verdict = (self.context.grants).judge_failure(self.baseline(), O::FS, path, &e);
                self.raised::<O, _>(path, verdict)?;
                return if is_loop(&e) {
                    Err(Self::walk_failure(path, &e))
                } else {
                    Ok(None)
                };
            }
        };
        let held = self.judge_named::<O>(located, path)?;
        Ok(held.stat().is_some().then_some(held))
    }

    /// [`locate`](Self::locate) as a predicate: `None` where the walk fails
    /// or the grant refuses, so a scan skips what it may not read instead of
    /// aborting.  One off-limits entry must not blank a whole listing.
    pub fn locate_if_admitted<O: Op>(&mut self, path: &LexicalPath) -> Option<Located<Judged<O>>> {
        let located = walk(path, LeafMode::Follow).ok()?;
        (self.context.grants)
            .judge::<O, _>(self.baseline(), located)
            .ok()
    }

    /// Whether the live stack admits `op` on an access already walked — no
    /// audit, no refusal: the question a door asks about a *side* read it
    /// may simply forgo, such as a write's before-image.
    pub fn admits_fs(&self, op: FsOp, trace: &Trace) -> bool {
        self.context.grants.admits_fs(self.baseline(), op, trace)
    }

    /// The OS-renderable projection of the live capability stack; `None` when
    /// no layer restricts enough to need an OS sandbox at all.
    pub fn sandbox_projection(&self) -> Option<SandboxProjection> {
        SandboxProjection::of(&self.context.grants, self.baseline(), None)
    }

    /// Run `f` with `layer` pushed for its dynamic extent.  The single gate
    /// into capability-checked code — `grant { … }` blocks and plugin hook /
    /// keybinding / handler dispatch all funnel through here.  The push sits on
    /// top of the caller's stack, so effective authority is always caller ∩
    /// this layer.
    pub fn with_layer<R>(&mut self, layer: GrantLayer, f: impl FnOnce(&mut Self) -> R) -> R {
        self.with_pushed(vec![layer], f)
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
    /// just pushed.  [`crate::authority::policy::deputy_prefixes`] takes the stack
    /// and only reports, never denies.  No-op unless a trail is open; a
    /// `Flagged` check has no rail branch, and the scope guards hold no
    /// `Mooring`.
    pub(crate) fn audit_deputy_prefixes(&mut self) {
        if !self.local.audit.active() {
            return;
        }
        let prefixes =
            crate::authority::policy::deputy_prefixes(&self.context.grants, self.baseline());
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

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    /// A discard spelling that walks elsewhere is judged where it walks.
    #[test]
    fn a_spelling_folding_to_the_discard_device_is_judged_where_it_walks() {
        use crate::authority::policy::{Capabilities, FsPolicy};
        use crate::path::{FrozenPath, Reading};
        use crate::test_helper::layer;

        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let depth = root.components().count() - 1;
        let deep: std::path::PathBuf = std::iter::repeat_n("a", depth + 1).collect();
        std::fs::create_dir_all(root.join("work").join(&deep)).unwrap();
        std::os::unix::fs::symlink(root.join("work").join(&deep), root.join("work/l")).unwrap();
        std::fs::create_dir(root.join("dev")).unwrap();
        std::fs::write(root.join("secret"), "key").unwrap();
        std::os::unix::fs::symlink(root.join("secret"), root.join("dev/null")).unwrap();
        let ups: std::path::PathBuf = std::iter::repeat_n("..", depth + 2).collect();
        let spelling = root.join("work/l").join(ups).join("dev/null");
        let rp = LexicalPath::absolute(&spelling).unwrap();
        assert!(rp.is_discard(), "{}", spelling.display());
        let frozen = |p: &std::path::Path| FrozenPath::from_surface(crate::path::utf8(p).unwrap());
        let layer = layer(Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![frozen(&root)],
                deny_paths: vec![frozen(&root.join("secret"))],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        });
        let mut shell = crate::test_helper::core_shell();
        let refused = shell.with_layer(layer, |sh| sh.locate::<Reading>(&rp).err());
        let refused = refused.expect("the walk lands on the denied secret");
        assert!(refused.to_string().contains("denied"), "{refused}");
    }
}
