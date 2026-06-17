# Vendored genai

This crate is a vendored copy of the `genai` multi-provider LLM client, patched
so the client it builds for model-list requests validates against the bundled
Mozilla webpki roots rather than the operating system's trust store — matching
exarch's outbound trust policy in `exarch/src/tls.rs`.

exarch already injects a webpki-bound client into genai for the chat and
streaming transport (`Client::builder().with_reqwest(...)`). The model-listing
path escapes that injection: genai's `Adapter::all_model_names(kind, endpoint,
auth)` takes no `WebClient`, so each adapter builds its own
`WebClient::default()`. Upstream's default builds a reqwest `rustls` client,
which defaults to `rustls-platform-verifier`; on a host with no system trust
store (e.g. a container image shipping no `ca-certificates` bundle) it loads
zero roots and aborts the client build — a panic during startup provider
auto-discovery, with no seam to inject our client. The defect is unchanged
through the latest upstream release (`0.7.0-beta.3`).

## Upstream baseline

- Project: <https://github.com/jeremychone/rust-genai>
- Version: **`0.6.5`** (encoded in this crate's version `0.6.5+ral.0`; the
  `+ral.0` build-metadata suffix counts ral-local revisions of the same
  upstream release — bump it to `+ral.1`, … on each re-sync). Build metadata is
  ignored in semver matching, so exarch's `genai = "0.6.5"` still resolves here.
- Consumed via `[patch.crates-io]` in the workspace root `Cargo.toml`; this
  crate is `exclude`d from the workspace, so the workspace lints do not apply
  (hence no `[lints]` stanza, unlike `vendor/ripgrep-core`).
- The vendored tree is trimmed to the library build surface: the upstream
  examples, integration tests and their fixtures, CI, docs, and publish
  artifacts are removed, along with the now-dangling `[[example]]`/`[[test]]`
  target blocks in `Cargo.toml`. Only `src/`, `Cargo.toml`, `README.md`, and the
  licences remain.
- Added to this repo in commit `eae4ab6d`, trimmed in `cda43b8c`.

## Divergence from upstream

The diff is intentionally minimal and contained, so a future re-sync is a
near-mechanical re-apply:

- `src/webc/web_client.rs` — `impl Default for WebClient` calls
  `.use_preconfigured_tls(webpki_tls_config())` as its first builder step, and a
  new `webpki_tls_config()` builds a `rustls::ClientConfig` from
  `webpki_roots::TLS_SERVER_ROOTS` with the aws-lc-rs provider. The provider and
  rustls version match the ones reqwest is built with, so reqwest's
  `use_preconfigured_tls` downcast succeeds at build time rather than silently
  falling back to the platform verifier.
- `Cargo.toml` — added the `rustls` (`0.23`, `default-features = false`,
  features `std`/`tls12`/`aws-lc-rs`) and `webpki-roots` (`1`) dependencies the
  helper needs, and set the version to `0.6.5+ral.0`.

No other source file is modified from upstream `src/`.

## Re-syncing to a newer genai

1. Confirm the patch is still needed: check that upstream's
   `Adapter::all_model_names` still takes no `WebClient` and that
   `WebClient::default()` still builds an unconfigured reqwest client. If
   upstream threads the configured client into the list path, drop this fork and
   inject via `Client::builder().with_reqwest(...)` alone.
2. Download the target genai release (`cargo` registry or crates.io) and copy
   its `src/` over `src/` here, replacing it wholesale.
3. Re-apply the `web_client.rs` patch: add `.use_preconfigured_tls(
   webpki_tls_config())` to the `Default` impl and the `webpki_tls_config()`
   helper above it.
4. In the normalized `Cargo.toml`, re-add the `rustls` and `webpki-roots`
   dependencies, remove the non-lib files and their `[[example]]`/`[[test]]`
   target blocks, and keep `README.md` and the licences.
5. Set the version to `<release>+ral.N` (bump N), then
   `cargo build -p exarch && cargo tree -p exarch -d` to confirm the patch links
   and the graph still unifies on a single `rustls 0.23`.
