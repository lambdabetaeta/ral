//! The shell's authority boundary: its live grant stack handed to
//! [`crate::authority::guard`], refusals recorded at one door, and scope
//! guards that push and pop grant layers. A layer holding no opinion on an
//! effect abstains, so a check passes unless some layer withholds.

use crate::authority::guard::{
    Denial, Refusal, check_device_name, check_exec, check_flag, check_fs_exact, fs_denial,
};
use crate::authority::policy::{Admitted, Baseline, Flag, FsOp, GrantLayer, Program};
use crate::authority::sandbox::SandboxProjection;
use crate::path::walk::{Kind, is_loop, loop_trace};
use crate::path::{
    Dir, Judged, LeafMode, LexicalPath, Located, Op, Trace, walk, walk_dir, walk_lenient,
};
use crate::shell::runtime::{Break, Check, Error, Mooring, Resource, Settled, Shell};
use std::collections::BTreeMap;
use std::io;

impl From<Refusal> for Error {
    fn from(Refusal { message, hint }: Refusal) -> Self {
        Self {
            hint: hint.map(Into::into),
            ..Self::new(message)
        }
    }
}

impl Shell {
    /// Check a boolean capability: `Err` if a layer of the active stack
    /// withholds `flag`, naming `who` in the refusal.
    ///
    /// # Errors
    /// `Err` if a layer of the active stack withholds it.
    pub fn check(&self, flag: Flag, who: &str) -> Settled<()> {
        check_flag(&self.context.grants, flag, who).map_err(|refusal| Error::from(refusal).into())
    }

    /// Record `check` and mint the `Break` that refuses.  Its callers hold no
    /// `Mooring`, so the denial reaches the trail alone.
    fn refused(&mut self, Denial { check, error }: Denial) -> Break {
        self.record_check(None, check);
        Error::from(error).into()
    }

    fn judged<T>(&mut self, verdict: Result<T, Denial>) -> Settled<T> {
        verdict.map_err(|denial| self.refused(denial))
    }

    /// Judge `program`, shown as `shown`; a refusal is recorded against
    /// `mooring`.  The [`Admitted`] is what the launcher demands.
    ///
    /// # Errors
    /// `Err` if the active grant denies the program.
    pub(crate) fn check_exec(
        &mut self,
        shown: &str,
        program: Program,
        mooring: &Mooring,
    ) -> Settled<Admitted> {
        let verdict = check_exec(&self.context.grants, self.baseline(), shown, program);
        verdict.map_err(|Denial { check, error }| {
            self.record_check(Some(mooring), check);
            Error::from(error).into()
        })
    }

    /// The baseline every fs judgment of this shell reads: its runtime's.
    pub fn baseline(&self) -> &Baseline {
        self.session.runtime.baseline()
    }

    /// Judge `path` as a name, walked leniently, for `O`: what a door asks
    /// where its walk found nothing or failed, so a denied name that does not
    /// exist reads as denied, not as absent.  A [discard device](LexicalPath::is_discard)
    /// is exempt from both regions, and a [reserved device name](check_device_name)
    /// is refused before either, asked before the walk, since the question is
    /// about the name and not about what is on the disk under it.
    fn check_named<O: Op>(&mut self, path: &LexicalPath) -> Settled<()> {
        if path.is_discard() {
            return Ok(());
        }
        check_device_name(path).map_err(Error::from)?;
        let trace = walk_lenient(path, LeafMode::Follow)
            .map_err(|e| crate::shell::runtime::Error::io(path.shown(), &e))?;
        let verdict = check_fs_exact(&self.context.grants, self.baseline(), &trace, &O::FS);
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
            .judge::<O>(self.baseline(), located)
            .map_err(|refused| fs_denial(&self.context.grants, refused, &O::FS, &real));
        self.judged(verdict)
    }

    /// Judge disclosure at the failed link, or at the leniently resolved name.
    fn walk_failure<O: Op>(&mut self, path: &LexicalPath, error: &io::Error) -> Settled<Error> {
        if let Some(trace) = loop_trace(error) {
            let verdict = check_fs_exact(&self.context.grants, self.baseline(), trace, &O::FS);
            self.judged(verdict)?;
        } else {
            self.check_named::<O>(path)?;
        }
        Ok(Error::io(path.shown(), error))
    }

    /// A walk's failure is disclosed only after its diagnostic is judged.
    fn named_on_failure<O: Op, T>(
        &mut self,
        path: &LexicalPath,
        walked: io::Result<T>,
    ) -> Settled<T> {
        walked.or_else(|e| Err(self.walk_failure::<O>(path, &e)?.into()))
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
        check_device_name(path).map_err(Error::from)?;
        let located = self.named_on_failure::<O, _>(path, walk(path, LeafMode::Follow))?;
        self.judge(located)
    }

    /// [`locate`](Self::locate) for a directory entered: `walk_dir`, then the
    /// directory judged.
    ///
    /// # Errors
    /// As [`locate`](Self::locate).
    pub fn locate_dir<O: Op>(&mut self, path: &LexicalPath) -> Settled<Dir<Judged<O>>> {
        check_device_name(path).map_err(Error::from)?;
        let dir = self.named_on_failure::<O, _>(path, walk_dir(path))?;
        let real = dir.real().clone();
        let verdict = (self.context.grants)
            .judge_dir::<O>(self.baseline(), dir)
            .map_err(|refused| fs_denial(&self.context.grants, refused, &O::FS, &real));
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
        check_device_name(path).map_err(Error::from)?;
        let located = match walk(path, leaf) {
            Ok(located) => located,
            Err(e) => {
                let error = self.walk_failure::<O>(path, &e)?;
                return if is_loop(&e) {
                    Err(error.into())
                } else {
                    Ok(None)
                };
            }
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
            check_device_name(&cand).map_err(Error::from)?;
            let Ok(located) = walk(&cand, LeafMode::Follow) else {
                continue;
            };
            let real = located.real().clone();
            match (self.context.grants).judge::<O>(self.baseline(), located) {
                Ok(found) if found.trace().kind() == Some(Kind::File) => return Ok(Some(found)),
                Ok(_) => {}
                Err(refused) => {
                    first.get_or_insert_with(|| {
                        fs_denial(&self.context.grants, refused, &O::FS, &real)
                    });
                }
            }
        }
        first.map_or(Ok(None), |denial| Err(self.refused(denial)))
    }

    /// [`locate`](Self::locate) as a predicate: `None` where the walk fails
    /// or the grant refuses, so a scan skips what it may not read instead of
    /// aborting.  One off-limits entry must not blank a whole listing.
    pub fn locate_if_admitted<O: Op>(&mut self, path: &LexicalPath) -> Option<Located<Judged<O>>> {
        let located = walk(path, LeafMode::Follow).ok()?;
        (self.context.grants)
            .judge::<O>(self.baseline(), located)
            .ok()
    }

    /// Whether the live stack admits `op` on an access already walked — no
    /// audit, no refusal: the question a door asks about a *side* read it
    /// may simply forgo, such as a write's before-image.
    pub fn admits_fs(&self, op: &FsOp, trace: &Trace) -> bool {
        self.context.grants.admits_fs(self.baseline(), op, trace)
    }

    /// The OS-renderable projection of the live capability stack; `None` when
    /// no layer restricts enough to need an OS sandbox at all.
    pub fn sandbox_projection(&self) -> Option<SandboxProjection> {
        SandboxProjection::of(&self.context.grants, self.baseline(), None)
    }

    /// Run `f` with `layer` pushed for its dynamic extent.  The single gate
    /// into capability-checked code — `grant { … }` blocks and plugin hook /
    /// keybinding / alias dispatch all funnel through here.  The push sits on
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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn fs_and_flag_refusals_keep_their_diagnostics_and_error_status() {
        use crate::authority::policy::{Capabilities, FsPolicy, ShellVerb};
        use crate::path::Reading;
        use crate::test_helper::{layer, trace};

        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("secret");
        std::fs::write(&file, "key").unwrap();
        let path = LexicalPath::absolute(&file).unwrap();
        let restricted = layer(Capabilities {
            fs: Some(FsPolicy::default()),
            shell: Some([].into()),
            ..Capabilities::default()
        });
        let mut shell = crate::test_helper::core_shell();
        shell.with_layer(restricted, |shell| {
            let Break::Error(fs) = shell.locate::<Reading>(&path).err().expect("reading is denied")
            else {
                panic!("a grant refusal is an error, never an escape");
            };
            assert_eq!(
                fs.message,
                format!(
                    "fs read denied by grant: no allowance for reading {}",
                    crate::path::shown(trace(&file).real().as_path())
                )
            );
            assert_eq!(fs.hint.as_deref(), Some("add it to the grant's fs `read` or `write` list"));
            assert_eq!(fs.code(), 1);

            let Break::Error(flag) = shell.check(Flag::Shell(ShellVerb::Detach), "detach").unwrap_err()
            else {
                panic!("a grant refusal is an error, never an escape");
            };
            assert_eq!(flag.message, "detach: an active grant withholds it, so nothing here may outlive this session. Would `spawn` do, since it ends when the session does?");
            assert!(flag.hint.is_none());
            assert_eq!(flag.code(), 1);
        });
    }

    /// A discard spelling that walks elsewhere is judged where it walks.
    #[cfg(unix)]
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
