//! Capability-check forwarders.
//!
//! Every `Shell::check_*` is a thin wrapper over a
//! `capability::check_*(&Context, …)` decision: hand the current
//! dynamic context to the function that folds the whole stack, then
//! relay the verdict.  Checks that need the audit trail
//! ([`Self::check_exec_args`], [`Self::check_fs_read`],
//! [`Self::check_fs_write`]) funnel through [`Self::audit_call`], which
//! splits the disjoint borrows on `mobile.context` and `local.audit`
//! and value-types the call site so the trail is free to grow as the
//! check runs.

use super::Shell;
use crate::capability::FsOp;
use crate::types::{Audit, CallSite, Context, SandboxProjection, Settled};

impl Shell {
    /// True when capability checks should emit nodes into the audit
    /// tree.  See
    /// [`Context::should_audit_capabilities`](super::Context::should_audit_capabilities).
    pub fn should_audit_capabilities(&self) -> bool {
        self.mobile
            .context
            .should_audit_capabilities(&self.local.audit)
    }

    /// Check `editor.read` capability — forwards to
    /// [`capability::check_editor_read`](crate::capability::check_editor_read).
    ///
    /// # Errors
    /// Returns `Err` if some layer of the active capabilities stack denies
    /// `editor.read`.
    pub fn check_editor_read(&self, subcmd: &str) -> Settled<()> {
        crate::capability::check_editor_read(&self.mobile.context, subcmd)
    }

    /// Check `editor.write` capability.
    ///
    /// # Errors
    /// Returns `Err` if some layer of the active capabilities stack denies
    /// `editor.write`.
    pub fn check_editor_write(&self, subcmd: &str) -> Settled<()> {
        crate::capability::check_editor_write(&self.mobile.context, subcmd)
    }

    /// Check `editor.tui` capability.
    ///
    /// # Errors
    /// Returns `Err` if some layer of the active capabilities stack denies
    /// `editor.tui`.
    pub fn check_editor_tui(&self) -> Settled<()> {
        crate::capability::check_editor_tui(&self.mobile.context)
    }

    /// Check `shell.chdir` capability.
    ///
    /// # Errors
    /// Returns `Err` if some layer of the active capabilities stack denies
    /// `shell.chdir`.
    pub fn check_shell_chdir(&self) -> Settled<()> {
        crate::capability::check_shell_chdir(&self.mobile.context)
    }

    /// Snapshot the audit site, split out the disjoint context + audit
    /// borrows (`mobile.context` and `local.audit` sit in different
    /// sub-trees of `Shell`), and hand them to `f`.  The site is
    /// value-typed so it does not re-borrow `context.location` once the
    /// context is live.  Every audit-emitting check funnels through here.
    fn audit_call<R>(&mut self, f: impl FnOnce(&Context, &mut Audit, CallSite) -> R) -> R {
        let site = self.run.loc.audit_site();
        f(&self.mobile.context, &mut self.local.audit, site)
    }

    /// Validate an `exec` capability check against the active stack
    /// and emit an audit node if auditing is on.
    ///
    /// Convenience entry for a command with a single identity set: the
    /// `policy_names` act as both the veto and the admission identities.
    /// The runtime dispatch path, where a head's resolved/as-invoked
    /// basenames widen the veto surface, goes through
    /// [`Self::check_exec_call`].
    ///
    /// # Errors
    /// Returns `Err` if the active grant denies the command outright, or
    /// admits only a subcommand set that `args`'s first element is not in
    /// (or that `args` is empty against).
    pub fn check_exec_args(
        &mut self,
        display_name: &str,
        policy_names: &[&str],
        args: &[String],
    ) -> Settled<()> {
        self.check_exec_call(display_name, policy_names, policy_names, args)
    }

    /// Validate an `exec` capability check with distinct veto and
    /// admission identity sets — deny-broad, allow-narrow.
    ///
    /// `deny_names` is the broad identity set consulted for vetoes;
    /// `policy_names` is the narrow set that may admit.  See
    /// [`CommandIdentity::deny_names_from`](crate::runtime::command::CommandIdentity::deny_names_from).
    ///
    /// # Errors
    /// Returns `Err` if the active grant denies the command outright, or
    /// admits only a subcommand set that `args`'s first element is not in
    /// (or that `args` is empty against).
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

    /// Validate an fs read against the active capability stack.  Takes a
    /// [`ResolvedPath`](crate::path::ResolvedPath) the caller minted via
    /// [`Shell::resolve`](super::Shell::resolve), so the gate's sole
    /// input is already cwd-anchored and `.`/`..`-collapsed.
    ///
    /// # Errors
    /// Returns `Err` if, at some layer with an `fs` opinion, `path` falls
    /// under a `deny_paths` entry or outside every read prefix.
    pub fn check_fs_read(&mut self, path: &crate::path::ResolvedPath) -> Settled<()> {
        self.audit_call(|ctx, audit, site| {
            crate::capability::check_fs_op(ctx, path, &FsOp::Read, audit, site)
        })
    }

    /// Validate an fs write against the active capability stack.
    ///
    /// # Errors
    /// Returns `Err` if, at some layer with an `fs` opinion, `path` falls
    /// under a `deny_paths` entry or outside every write prefix.
    pub fn check_fs_write(&mut self, path: &crate::path::ResolvedPath) -> Settled<()> {
        self.audit_call(|ctx, audit, site| {
            crate::capability::check_fs_op(ctx, path, &FsOp::Write, audit, site)
        })
    }

    /// Compute the OS-renderable projection of the current
    /// capabilities stack.  See
    /// [`capability::sandbox_projection`](crate::capability::sandbox_projection).
    pub fn sandbox_projection(&self) -> Option<SandboxProjection> {
        crate::capability::sandbox_projection(&self.mobile.context)
    }

    /// The guest process jail installed on this session, if any — `None`
    /// everywhere but a real Linux guest.  Thin forwarder, same shape as
    /// [`Self::sandbox_projection`].
    pub fn guest_jail(&self) -> Option<std::sync::Arc<crate::process::jail::GuestJail>> {
        self.session.guest_jail.clone()
    }
}
