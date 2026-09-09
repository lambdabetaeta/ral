//! Capability-check forwarders: each `Shell::check_*` hands the live dynamic
//! context to the `capability::check_*` decision that folds the whole grant
//! stack, then relays its verdict.  A layer holding no opinion on an effect
//! abstains, so a check passes unless some layer withholds.

use super::Shell;
use crate::capability::FsOp;
use crate::path::walk::Leaf;
use crate::types::{Audit, CallSite, Context, SandboxProjection, Settled};

impl Shell {
    /// Check `editor.read`; `subcmd` names the `_ed-*` builtin in the refusal.
    ///
    /// # Errors
    /// `Err` if a layer of the active stack withholds it.
    pub fn check_editor_read(&self, subcmd: &str) -> Settled<()> {
        crate::capability::check_editor_read(&self.context, subcmd)
    }

    /// Check `editor.write`.
    ///
    /// # Errors
    /// `Err` if a layer of the active stack withholds it.
    pub fn check_editor_write(&self, subcmd: &str) -> Settled<()> {
        crate::capability::check_editor_write(&self.context, subcmd)
    }

    /// Check `editor.tui`.
    ///
    /// # Errors
    /// `Err` if a layer of the active stack withholds it.
    pub fn check_editor_tui(&self) -> Settled<()> {
        crate::capability::check_editor_tui(&self.context)
    }

    /// Check `shell.chdir`, the capability `cd` needs.
    ///
    /// # Errors
    /// `Err` if a layer of the active stack withholds it.
    pub fn check_shell_chdir(&self) -> Settled<()> {
        crate::capability::check_shell_chdir(&self.context)
    }

    /// Every audit-emitting check funnels through here: `context` and
    /// `local.audit` are disjoint fields, and the site is taken by value so it
    /// keeps no borrow on the session's source registry.
    fn audit_call<R>(&mut self, f: impl FnOnce(&Context, &mut Audit, CallSite) -> R) -> R {
        let site = self.call_site();
        f(&self.context, &mut self.local.audit, site)
    }

    /// Check `exec` for a command with one identity set — `policy_names` both
    /// vetoes and admits.  Runtime dispatch, where the resolved and as-invoked
    /// basenames widen the veto set, goes through [`Self::check_exec_call`].
    ///
    /// # Errors
    /// `Err` if the active grant denies the command, or admits only a
    /// subcommand set that `args`'s first element misses.
    pub fn check_exec_args(
        &mut self,
        display_name: &str,
        policy_names: &[&str],
        args: &[String],
    ) -> Settled<()> {
        self.check_exec_call(display_name, policy_names, policy_names, args)
    }

    /// Check `exec` deny-broad, allow-narrow: `deny_names` is the wide set
    /// consulted for vetoes, `policy_names` the narrow one that may admit.
    /// `CommandIdentity::deny_names_from` widens the latter into the former.
    ///
    /// # Errors
    /// `Err` if the active grant denies the command, or admits only a
    /// subcommand set that `args`'s first element misses.
    pub(crate) fn check_exec_call(
        &mut self,
        display_name: &str,
        deny_names: &[&str],
        policy_names: &[&str],
        args: &[String],
    ) -> Settled<()> {
        self.audit_call(|ctx, audit, site| {
            crate::capability::check_exec_args(
                ctx,
                display_name,
                deny_names,
                policy_names,
                args,
                audit,
                site,
            )
        })
    }

    /// Check an fs read *by name* — a predicate, a listing, a module load.
    /// `path` comes from `Shell::resolve`, so the gate's sole input is
    /// already cwd-anchored and `.`/`..`-collapsed.  Anything that opens the
    /// name, for reading or writing, takes [`Self::locate`] instead.
    ///
    /// # Errors
    /// `Err` if, at some layer with an `fs` opinion, `path` falls under a
    /// `deny_paths` entry or outside every read prefix.
    pub fn check_fs_read(&mut self, path: &crate::path::ResolvedPath) -> Settled<()> {
        self.audit_call(|ctx, audit, site| {
            crate::capability::check_fs_op(ctx, path, &FsOp::Read, audit, site)
        })
    }

    /// The fs door: walk `path` to the object it names, symlink-free, and
    /// authorise *that object* for `op`.  The [`Located`] handed back
    /// performs every operation relative to the object's directory handle,
    /// so what was judged is what gets opened — a dangling link is judged at
    /// its target, and a component swapped after the judgment is never
    /// followed.  Every write in ral comes through here.
    ///
    /// # Errors
    /// The walk's I/O error, phrased with the name as written; or the
    /// grant's refusal.
    pub fn locate(
        &mut self,
        path: &crate::path::ResolvedPath,
        op: &FsOp,
    ) -> Settled<crate::path::Located> {
        let located = crate::path::walk::walk(path, Leaf::Resolve).map_err(|e| {
            let name = path.display();
            let msg = match e.kind() {
                std::io::ErrorKind::NotFound => format!("{name}: no such file or directory"),
                std::io::ErrorKind::PermissionDenied => format!("{name}: permission denied"),
                _ => format!("{name}: {e}"),
            };
            crate::types::Break::Error(crate::types::Error::new(msg, 1))
        })?;
        if !path.is_discard() {
            self.audit_call(|ctx, audit, site| {
                crate::capability::check_fs_exact(ctx, located.real(), op, audit, site)
            })?;
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
    /// The grant's refusal, on the object or on the name.
    pub(crate) fn locate_existing(
        &mut self,
        path: &crate::path::ResolvedPath,
        op: &FsOp,
        leaf: Leaf,
    ) -> Settled<Option<crate::path::Located>> {
        let Ok(located) = crate::path::walk::walk(path, leaf) else {
            self.audit_call(|ctx, audit, site| {
                crate::capability::check_fs_op(ctx, path, op, audit, site)
            })?;
            return Ok(None);
        };
        self.audit_call(|ctx, audit, site| {
            crate::capability::check_fs_exact(ctx, located.real(), op, audit, site)
        })?;
        Ok(Some(located))
    }

    /// [`locate`](Self::locate) as a predicate: `None` where the walk fails
    /// or the grant refuses, so a scan skips what it may not read instead of
    /// aborting.  One off-limits entry must not blank a whole listing.
    pub fn locate_if_admitted(
        &mut self,
        path: &crate::path::ResolvedPath,
        op: &FsOp,
    ) -> Option<crate::path::Located> {
        let located = crate::path::walk::walk(path, Leaf::Resolve).ok()?;
        self.admits_fs_exact(op, located.real()).then_some(located)
    }

    /// Whether the live stack admits `op` on a path already located — no
    /// audit, no refusal: the question a door asks about a *side* read it
    /// may simply forgo, such as a write card's before-image.
    pub fn admits_fs_exact(&self, op: &FsOp, real: &std::path::Path) -> bool {
        self.context
            .grants
            .admits_fs_exact(op, &self.context.resolver(), real)
    }

    /// The OS-renderable projection of the live capability stack; `None` when
    /// no layer restricts enough to need an OS sandbox at all.
    pub fn sandbox_projection(&self) -> Option<SandboxProjection> {
        let ctx = &self.context;
        let path_env = ctx.env_overrides().get("PATH").map_or("", String::as_str);
        crate::capability::sandbox_projection(&ctx.grants, &ctx.resolver(), path_env)
    }

    /// Whether the live stack permits birthing a process this session stops
    /// owning.  Read at the `detach` call, so an enclosing `grant` frame binds.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn permits_detach(&self) -> bool {
        self.context.grants.permits_detach()
    }

    /// The guest process jail installed on this session — `None` anywhere but
    /// a real Linux guest.
    pub(crate) fn guest_jail(&self) -> Option<std::sync::Arc<crate::process::jail::GuestJail>> {
        self.session.guest_jail.clone()
    }
}
