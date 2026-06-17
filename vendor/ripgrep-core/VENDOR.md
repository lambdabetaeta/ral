# Vendored ripgrep core

This crate is a vendored copy of ripgrep's top-level binary crate, exposed
as a library so ral can drive a search in-process (the `ripgrep` Cargo
feature) instead of shelling out to an external `rg`.

## Upstream baseline

- Project: <https://github.com/BurntSushi/ripgrep>
- Tag: **`15.1.0`** (encoded in this crate's version `15.1.0+ral.0`; the
  `+ral.0` build-metadata suffix counts ral-local revisions of the same
  upstream tag — bump it to `+ral.1`, … on each re-sync).
- Vendored source: ripgrep's `crates/core/` (its `main.rs` binary crate),
  which depends on the published `grep` / `ignore` crates pinned in
  `Cargo.toml` — those are *not* vendored, only the CLI core is.
- Added to this repo in commit `3233847a` ("adding ripgrep shim").

## Divergence from upstream

The diff is intentionally minimal and contained, so a future security
rebase is a near-mechanical re-apply:

- `crates/core/main.rs` → `src/lib.rs`. The `fn main()` is replaced by two
  library entry points (`lib.rs:42`):
  - `run_cli(rawargs)` — run with explicit argv (argv[0] excluded);
  - `run_env()` — run with the process argv (`std::env::args_os`).
  Both wrap the upstream `run` / `finish` logic unchanged.
- `Cargo.toml`: renamed to `ral-ripgrep-core`, `publish = false`,
  `[lib] path = "src/lib.rs"`, and `[lints.clippy] all = "allow"` so the
  vendored source stays diffable against upstream (the workspace lints are
  not applied here).

No other source file is modified from upstream `crates/core/`.

## Re-syncing to a newer ripgrep

1. Check out the target ripgrep tag upstream.
2. Copy `crates/core/{flags,haystack.rs,logger.rs,messages.rs,search.rs}`
   over `src/` here, replacing them wholesale.
3. Re-apply the `main.rs` → `lib.rs` shim: take upstream `crates/core/main.rs`,
   delete `fn main`, and add back the `run_cli` / `run_env` wrappers above.
4. Refresh the dependency pins in `Cargo.toml` to match the new tag's
   `crates/core/Cargo.toml` (`grep`, `ignore`, `bstr`, …), keeping the
   ral-local `[package]` / `[lib]` / `[lints]` stanzas.
5. Update the tag and bump the `+ral.N` suffix in `version` above and in
   `Cargo.toml`, then `cargo build -p ral --features ripgrep` to confirm
   `run_cli` / `run_env` still link.
