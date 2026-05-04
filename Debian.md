# Debian package release process

The package is built with [`cargo-deb`](https://github.com/kornelski/cargo-deb).
Metadata lives in `[package.metadata.deb]` in `ral/Cargo.toml`.
Maintainer scripts are in `packaging/debian/`.

The `.deb` installs:
- `/usr/bin/ral` — the ral shell
- `/usr/bin/ral-sh` — the POSIX-bridge login-shell dispatcher
- `/usr/share/doc/ral/` — SPEC.md and TUTORIAL.md

`postinst` appends `/usr/bin/ral-sh` to `/etc/shells` on install.
`postrm` removes it on uninstall/purge.

## Prerequisites

Install `cargo-deb` once (on any machine with Rust):

```
cargo install cargo-deb
```

For cross-compilation to `amd64` from macOS, use the Docker container:

```
docker exec shell-dev cargo install cargo-deb
```

## Building the package

Build release binaries first, then generate the `.deb`:

```
docker exec shell-dev cargo build --release -p ral -p ral-sh
docker exec shell-dev cargo deb -p ral --no-build
```

The `.deb` is written to `target/debian/ral_<version>_amd64.deb`.

`--no-build` skips an extra cargo build since we just built above.

## Releasing

1. Tag and push (same as for Homebrew):

   ```
   git tag v0.X.Y
   git push all v0.X.Y
   ```

2. Build the `.deb` as above.

3. Attach `target/debian/ral_0.X.Y_amd64.deb` to the GitHub release.

## Installing on Debian

```
wget https://github.com/lambdabetaeta/ral/releases/download/v0.X.Y/ral_0.X.Y_amd64.deb
sudo dpkg -i ral_0.X.Y_amd64.deb
```

## Setting ral-sh as your login shell

The `postinst` script registers `/usr/bin/ral-sh` in `/etc/shells` automatically.
Then:

```
chsh -s /usr/bin/ral-sh
```

## Uninstalling

```
sudo dpkg -r ral
```

`postrm` removes `/usr/bin/ral-sh` from `/etc/shells`.
