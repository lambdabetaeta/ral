---
status: accepted
---

# synod keeps its own accounts, in the computer's credential manager

**A key reaches exarch through the environment, because exarch is started
from a shell by someone who has one. Synod is double-clicked, inherits the
desktop's environment, and faces someone with no `.zshrc` to export from — so
it asks for keys in a window and keeps them where the computer already keeps
passwords. The two products now have two credential stories, and what they
share is the library, not the policy: exarch's own paths are untouched, and
every piece synod needed was added beside the machinery it generalises.**

## Context

Until now synod's `session::prepare` called `exarch::config::load()` and
`CredentialStore::resolve_and_scrub`, which is exactly exarch's startup: read
`$XDG_CONFIG_HOME/exarch/config.ral`, sweep the conventional key variables,
scrub them from the environment. For a developer running synod out of a
terminal this works. For the person synod is *for* it does not work at all —
a double-clicked application inherits the desktop session's environment, and
there is no step in which a key could have got into it. Synod could reach
providers only if someone had first made it a shell program.

Two smaller frictions rode along. Synod read exarch's config file, so a
declaration made for one product silently governed the other. And the sweep
is one-shot by construction: `resolve_and_scrub` mutates the environment and
so may only run while the process is single-threaded, which means a key
arriving mid-session had nowhere to go.

## Decision

- **The credential manager first, the environment as fallback.**
  `synod::accounts::prepare` runs the environment sweep exactly as before —
  it is still the step that must happen single-threaded — and then lays the
  vault over the top, so a key typed into the window outranks a stale
  variable in the launching environment. A developer's terminal keeps
  working with no ceremony; a key that was never typed into the window is
  never silently written into the vault either.

- **One door to the vault, in the library.**
  `exarch::provider::keychain` reaches the macOS Keychain, the Windows
  Credential Manager, or a Linux desktop's Secret Service through one
  `keyring` entry named `(app, provider-label)` — so synod's Anthropic key
  and exarch's are two entries, exactly as their config directories are two
  directories. **Exarch calls none of it.** The module is shared machinery
  the second product drives, not a change to the first.

- **A computer with no credential manager is told about, not lied to.**
  `Entry::store_status()` is asked first; where there is none the fallback is
  an owner-only file beside the app's own configuration, and `Keychain::vault`
  answers *where secrets on this computer actually land* in a sentence the
  window prints verbatim. A headless box gets an honest sentence rather than
  an implied protection that is not there. The file is born owner-private
  through `provider::secret_file::write_private` — the `ChatGPT` token
  store's existing care, extracted so the two callers share one
  implementation rather than two copies of a Windows DACL.

- **Endpoints are synod's own file, and hold no secrets.**
  `$XDG_CONFIG_HOME/synod/providers.ral` is read and written by the same
  decoder as exarch's config (`config::load_declared` / `save_declared`, the
  old `load()` body over a caller-named path). It carries addresses and
  protocols, never keys, so it needs no special permissions. A name or
  address carrying a quote is **asked about, not escaped**: these are things
  a person typed into a window, and a service whose name contains a quote is
  a typo to raise, not a string to encode.

- **Provenance is recorded at the binding.** `CredentialStore` remembers
  which of its two doors each key came through (`admit_key` sets it,
  `forget`/`retire` clear it, `was_admitted` reads it). The window needs this
  to know whether a key is its to take back — and deriving it instead by
  asking the vault, entry by entry, would cost a round trip per provider
  every time the accounts list was drawn, with an unlock prompt apiece on a
  locked keychain. The store is the only thing that knows; it should say.

## Consequences

- Synod gains five commands (`shell/keys.rs`), each returning the fresh list
  and ending in the same `models-refreshed` event a sign-in ends in, so a key
  typed into the window is usable on the very next conversation with no
  restart. This is `add_oauth`'s bargain extended to API keys, and it is only
  available because `admit_key` sidesteps the un-repeatable sweep.

- A key that came from the environment cannot be removed from the window, and
  the accounts screen says so, names the variable, and offers the one remedy
  there is: type another. Withdrawing a *declared endpoint* removes the
  service; forgetting a *key* leaves the service known and unbound. These are
  `retire` and `forget`, and the distinction is the user's, not an
  implementation detail.

- exarch is unchanged in behaviour: it still reads env keys and its own
  `config.ral`, and calls neither `keychain` nor the four new store mutators.
  The risk accepted is the ordinary one of a shared library — a change made
  for synod's sake now sits in exarch's crate, and must keep earning its
  place there rather than drifting into exarch's paths.

- `keyring 4.1` is a new dependency (+188 lines of `Cargo.lock`), reaching
  three platform APIs behind one type.

## Not settled here

The real Keychain, Credential Manager, and Secret Service round trips are
untested: no GUI runs in the development sandbox. The first run on a Mac or a
Windows box is the evidence, and until then the fallback arm is the only one
exercised.
