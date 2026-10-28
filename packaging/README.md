# Release legal files

Original workspace code is MIT OR Apache-2.0. The modified Codex portions
identified in the root `NOTICE` remain Apache-2.0-only: the workspace's dual
licence does not offer an MIT alternative for them. `NOTICE` retains the
upstream Codex attribution, including its Ratatui attribution.

## Binary delivery

- `scripts/package-release.sh` packages Unix binaries as `.tar.gz` and Windows
  exarch as `.zip`. Every archive contains `LICENSE-APACHE`, `LICENSE-MIT`, and
  `NOTICE`; exarch archives also contain `LICENSE-EDTUI`, copied verbatim from
  `vendor/edtui/LICENSE.md` (MIT, Copyright 2023 Philipp Reiter).
- Both the GitHub Actions matrix and `scripts/build-release.ral` use that
  packager. Checksums cover the archives. Do not publish the loose binaries.
- Unix download installers retain legal files in
  `${XDG_DATA_HOME:-$HOME/.local/share}/{ral,exarch}`. The Windows exarch
  installer retains them alongside `exarch.exe`.
- The ral MSI installs both licence texts and `NOTICE` beside `ral.exe`.
  Winget installs that MSI and needs no separate legal-file payload.
- Source install scripts retain legal files in `$CARGO_HOME/share/{ral,exarch}`.
- Synod bundles retain the four files under their `licenses` resources. The
  guest initramfs also carries them under `/usr/share/doc/exarch`; guest-root
  assembly copies these four files into the new root before `switch_root`
  reclaims the initramfs, and refuses to continue if a copy fails.

These additions cover the locally adapted Codex code and vendored edtui;
they are not a generated inventory of transitive dependency licences.

## Debian metadata

`ral/Cargo.toml` owns the only existing `package.metadata.deb` configuration.
Its `assets` include these entries (paths are relative to that crate):

```toml
{ source = "../LICENSE-APACHE", dest = "/usr/share/doc/ral/LICENSE-APACHE", mode = "644" },
{ source = "../LICENSE-MIT", dest = "/usr/share/doc/ral/LICENSE-MIT", mode = "644" },
{ source = "../NOTICE", dest = "/usr/share/doc/ral/NOTICE", mode = "644" },
```

Its `license-file` points to `../LICENSE-APACHE`; the installed licence texts
and `NOTICE` document the dual-licensed original code and Apache-only
adaptations. Exarch has no Debian package metadata. If one is added, include the same three files
under `/usr/share/doc/exarch/` plus `../vendor/edtui/LICENSE.md` installed there
as `LICENSE-EDTUI`. Cargo package licence declarations must likewise not
claim an MIT alternative for the Codex-derived portions.
