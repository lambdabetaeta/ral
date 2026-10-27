---
status: active
generated_at_commit: 6281520c58de152afc0b1ed99b94ade98c21551e
generated_at_date: 2026-10-27
verified_at_commit: 6281520c58de152afc0b1ed99b94ade98c21551e
verified_at_date: 2026-10-27
anchors: [adopt_posix_profile, adopts_login_env, EnvReport, indirect, ChildLimits]
---

# The login environment is the POSIX profiles

A Unix interactive login adopts the environment `/bin/sh -l` builds from
`/etc/profile` and `~/.profile`, then sources the rc. ral has no profile of
its own: `/etc/ral/profile` and `~/.ral_profile` would split environment setup
between ral and the POSIX file other programs already use. ral-specific
configuration belongs in `$XDG_CONFIG_HOME/ral/rc` or `~/.ralrc`.

Adoption runs a helper rather than replacing ral. A broken, exiting, or
missing profile helper must never leave a user without a shell: any failure
warns and preserves the environment ral started with. New and changed
variables are set, removed variables unset; `SHLVL`, `PWD`, `OLDPWD`, `_`
remain ral's bookkeeping. `--norc`/`--noprofile` skips adoption and the rc;
batch runs and Windows adopt nothing.

The helper execs this same binary as `--ral-env-report` by
`reexec::indirect`'s canonical path, bypassing a `ral-sh` symlink. Reporter
and collector share one NUL-delimited format in `host::login_env`. The report
is an unnamed file on fd 3, read after the helper exits, so a background job
inheriting that descriptor cannot hold ral waiting for EOF. ral ignores
SIGINT and SIGQUIT while waiting and restores its dispositions afterward;
the helper resets both to their defaults. Adoption precedes every thread.

This does not read `~/.zprofile` or `~/.bashrc`. A `umask` or resource limit
set in `~/.profile` belongs only to the helper: use the rc's `umask:` and
`limits:`. The mask governs ral's creates and its children; limits govern
children only and a grant's limits still win. The rc's `env:` sets environment
entries only, with lexical values under `bindings:`. An rc's `exit N` ends
the session with N. The POSIX bridge remains unchanged: its ral branch
reaches the same adoption in `run`.
