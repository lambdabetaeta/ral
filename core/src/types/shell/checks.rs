//! Capability-check forwarders: each `Shell::check_*` hands the live dynamic
//! context to the `capability::check_*` decision that folds the whole grant
//! stack, then relays its verdict.  A layer holding no opinion on an effect
//! abstains, so a check passes unless some layer withholds.

use super::Shell;
use crate::capability::FsOp;
use crate::types::{Audit, CallSite, Context, SandboxProjection, Settled};

impl Shell {
    /// True when capability checks should emit an observation: a live trail
    /// and an `audit: true` grants layer must both call for it.
    pub fn should_audit_capabilities(&self) -> bool {
        self.context
            .should_audit_capabilities(&self.local.audit)
    }

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
    pub fn check_exec_call(
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

    /// Check an fs read.  `path` comes from `Shell::resolve`, so the gate's
    /// sole input is already cwd-anchored and `.`/`..`-collapsed.
    ///
    /// # Errors
    /// `Err` if, at some layer with an `fs` opinion, `path` falls under a
    /// `deny_paths` entry or outside every read prefix.
    pub fn check_fs_read(&mut self, path: &crate::path::ResolvedPath) -> Settled<()> {
        self.audit_call(|ctx, audit, site| {
            crate::capability::check_fs_op(ctx, path, &FsOp::Read, audit, site)
        })
    }

    /// Check an fs write.
    ///
    /// # Errors
    /// `Err` if, at some layer with an `fs` opinion, `path` falls under a
    /// `deny_paths` entry or outside every write prefix.
    pub fn check_fs_write(&mut self, path: &crate::path::ResolvedPath) -> Settled<()> {
        self.audit_call(|ctx, audit, site| {
            crate::capability::check_fs_op(ctx, path, &FsOp::Write, audit, site)
        })
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
    pub fn permits_detach(&self) -> bool {
        self.context.grants.permits_detach()
    }

    /// The guest process jail installed on this session — `None` anywhere but
    /// a real Linux guest.
    pub fn guest_jail(&self) -> Option<std::sync::Arc<crate::process::jail::GuestJail>> {
        self.session.guest_jail.clone()
    }
}
