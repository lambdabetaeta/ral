# A grant never reaches its own key

**Nothing the agent runs can read the provider keys exarch uses to call its
model.** A profile decides what the agent may touch on the user's machine, but
no profile has to remember to exclude these keys; the guards below sit beneath
every profile. The few places they stop are listed at the end.

A key can be reached in two places: on disk, and inside the process holding it.
Each place has its own guard. Neither guard depends on a profile remembering it.

## On disk: denied by composition

Most profiles (`reasonable`, `read-only`, `edit-only`) read `` `xdg-config . `` and
`` `xdg-state . `` whole, on purpose. Tools break without their configuration, and the
profiles accept that risk by name, denying `gh`, `op` and `gcloud` by hand. Our
own credentials sit inside that region:

- `$XDG_STATE_HOME/<app>/keys.json`, the keychain fallback, which is the normal
  path on any host with no Secret Service (`provider/keychain.rs`);
- `$XDG_STATE_HOME/exarch/oauth.json`, a ChatGPT login's tokens
  (`provider/oauth/`).

Unguarded, a grant that leaves the network open could exfiltrate them with `cat`. So
`provider::credential_files` names every such file for every product on the
engine (`bootstrap::APPS`). `policy::for_invocation` then denies them **after**
composition, as its own layer, on the same footing as a `--restrict` file's own
path ([[map/exarch/policy|policy]]). No profile can forget the deny, no
`--extend` can widen past it, and every spawned child's layer can only
narrow beneath it ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]).

Synod's files are named by the engine (`bootstrap::SYNOD`), not by synod. The
deny and the directory therefore cannot drift apart. Synod's own agent runs in a
guest that mounts no host directory, but exarch started directly on the same
machine would otherwise read synod's keys.

The deny names **files, not directories**. `$XDG_CONFIG_HOME/exarch/` also
holds the operator's `AGENTS.md` and the global skill roots, which the agent is
meant to read ([[design/exarch-config-dir|exarch-config-dir]]).

## In the process: blanked, then sealed

A key handed over in the launch environment is read once, at single-threaded
startup, by `CredentialStore::resolve_and_scrub` ([[map/exarch|exarch]]
§Credentials). Removing the variable from the environment is not enough, for
two reasons:

- **The bytes outlive the variable.** `unsetenv` drops libc's pointer, but
  `/proc/<pid>/environ` reads the memory the kernel laid down at `execve`.
  exarch evaluates a tool call in its own process, so `< /proc/self/environ`
  names exarch itself, and one redirect used to print every key. Each value is
  now overwritten in place before it is unset.
- **The store holds the key.** The resolved keys live in exarch's heap. Under
  `ptrace_scope=0`, any same-uid child could read `/proc/<exarch-pid>/mem`. So
  the process makes itself non-dumpable first (`PR_SET_DUMPABLE`, Linux). This
  hands `/proc/<pid>/{environ,mem,maps}` to root and refuses the session, its
  children and every other same-uid process alike. `execve` restores
  dumpability, so children are otherwise unaffected.

On macOS there is no `/proc`, but `sysctl(KERN_PROCARGS2)` (what `ps eww`
reads) shows a same-uid process the same launch bytes, so blanking closes it
too; the heap is already guarded, since `task_for_pid` is refused to ordinary
processes.

This guard holds under every grant, `dangerous` included, because it does not
depend on a policy.

## Where it stops

- **`dangerous` has no `fs` policy to carry the file deny.** It attenuates
  nothing by contract, and installing a policy only to hold the deny would
  confine a session that asked not to be. The credential layer is pushed
  wherever some fs restricts: name a `--restrict` file, or extend `dangerous`
  with an fs exclusion, and the deny comes with it; a positive-only fs
  restricts nothing and gets none.
- **The vault is reachable from an unconfined session.** The macOS profile
  admits no `securityd`, and the Linux envelope binds no session bus unless a
  grant reads `/run/user`. Outside those, the credential manager answers any
  process of the user's ([[map/exarch/provider|provider]]).
- **Root inside a user namespace defeats the seal.** A non-dumpable process's
  `/proc` files belong to its namespace's root, which is exarch itself if it
  runs as that root. The bytes are still blanked, but the heap is open.
- **Session logs are readable on purpose.** Asking an agent to read logs is
  common, so `` `xdg-state . `` stays open. A key reaches a log only if the agent could
  read it first, and the two guards above close that; anything a user pastes
  into a session can be read by later sessions.
- **Windows has no seal.** Any process of the same user can open exarch for
  `PROCESS_VM_READ` and read its environment block and heap; a process DACL
  cannot stop its own owner. A confined child cannot: it runs in an
  AppContainer, which a process's default DACL does not admit. An unconfined
  session can.
