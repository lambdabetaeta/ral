---
status: active
generated_at_commit: 99226d37
---

# A grant does not hand out its own key

**The credentials that authorise a turn are the authority a
[[design/grant|grant]] is made of, never something it confers — so exarch's own
credential files are denied by composition, beneath every profile and outside
every widening.** A capability profile decides what the agent may reach on the
user's machine; the key that pays for the agent's next request is not on that
map at all, and a profile is the wrong place to remember so.

## The hole this closes

`reasonable`, `read-only` and `edit-only` read `xdg:config` and `xdg:state`
whole, deliberately: tools break without their own configuration, and the
profiles accept that read as an exfiltration risk they can name (`gh`, `op` and
`gcloud` are denied by hand). Our own files sat in exactly that reach —

- `$XDG_CONFIG_HOME/<app>/keys.json`, the keychain fallback, which is the
  normal path on any host with no Secret Service to hold a key
  (`provider/keychain.rs`);
- `$XDG_STATE_HOME/exarch/oauth.json`, a signed-in ChatGPT account's tokens
  (`provider/oauth/`).

— so with `net: true` the exfiltration was `cat`. The env channel was already
shut: `provider::credential` sweeps a key out of the process before any session
runs, eagerly, so what the environment carried cannot be read back
([[map/exarch|exarch]] §Accounts).

## Decision

- `provider::credential_files` names every file on this computer holding one of
  our credentials, over `bootstrap::APPS` — the products on this engine, whose
  directories differ only by name.
- `policy::for_invocation` denies them after composition, on the same footing
  as each `--restrict` file's own path: last, where no profile can forget them
  and no `--extend-base` can widen them back, deny sets unioning under meet and
  join alike ([[map/core/capabilities|capabilities]]).
- The carve-out lands wherever an `fs` policy exists — every base but a bare
  `dangerous`, which attenuates nothing by contract. Installing a policy there
  to hold these denies would confine every session that asked not to be, against
  an agent already free to read the same bytes a hundred other ways. Name a
  `--restrict` file under `dangerous` and the session attenuates; then the
  carve-out lands with it.
- Synod is named by the engine (`bootstrap::SYNOD`) rather than by synod, so
  the deny and the directory cannot drift into a hole. Synod's own agent never
  needed it — its shell runs in the guest, where no host directory is mounted —
  but exarch launched directly on that same machine would otherwise read
  synod's keys.

## Consequence

Only the credential *files* are denied, not the directories holding them:
`$XDG_CONFIG_HOME/exarch/` also holds the operator's `AGENTS.md` and the global
skill roots, which the agent is meant to read and which
[[map/exarch/builtins|`skill`]] filters through the live grant. The trust
argument of [[design/exarch-config-dir|exarch-config-dir]] is unchanged and now
true in both directions: the config home is unwritable by structure, and what
in it must stay unread is unread by veto.
