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

- `main.rs` → `lib.rs`: `fn main` removed; `run_cli` / `run_env` / `finish`
  added; `ExitCode` replaced by `u8` throughout.
- `flags/parse.rs`: `parse_from` / `parse_low_from` thread explicit argv.
- `flags/mod.rs`: re-exports `parse_from`.
- `Cargo.toml`: renamed to `ral-ripgrep-core`, `publish = false`,
  `[lib] path = "src/lib.rs"`, and `[lints.clippy] all = "allow"` so the
  vendored source stays diffable against upstream (the workspace lints are
  not applied here).

`vendor/rustfmt.toml` turns formatting off beneath `vendor/`, so everything
else is upstream's bytes and the shim edits are hand-written in its
79-column style.

## Re-syncing to a newer ripgrep

1. Check out the target ripgrep tag upstream.
2. Copy every file under `crates/core/` except `main.rs` and `README.md`
   over `src/` here, wholesale (the data files included).
3. Re-apply all three shims (`main.rs` → `lib.rs`, `flags/parse.rs`,
   `flags/mod.rs`) by hand onto upstream's text.
4. Refresh the dependency pins in `Cargo.toml` to match the new tag's
   `crates/core/Cargo.toml` (`grep`, `ignore`, `bstr`, …), keeping the
   ral-local `[package]` / `[lib]` / `[lints]` stanzas.
5. Update the tag and bump the `+ral.N` suffix in `version` above and in
   `Cargo.toml`, then `cargo build -p ral --features ripgrep` to confirm
   `run_cli` / `run_env` still link.
