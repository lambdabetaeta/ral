# justfile — a registry of the common dev commands for ral.
#
# This is an entry point, not a place for logic: every recipe is a
# single delegation to a `cargo` invocation or to one of the `.ral`
# scripts under scripts/, where the real orchestration lives.  The
# golden rule (AGENTS.md) applies here too — no shell branching or
# loops belong in this file; if a recipe wants them, it grows into a
# `.ral` script and this file calls that instead.
#
# The `.ral` scripts are launched through `cargo run -p ral`, never a
# bare `ral`: a fresh from-source checkout has no installed `ral` on
# PATH yet, so `just install` and friends must build the interpreter
# from source rather than presuppose it.  This also means every recipe
# exercises the current tree, not a stale installed binary.
#
# Run `just` with no arguments to list recipes.

# Show the recipe list.
default:
    @just --list

# Build the whole workspace, including tests and examples.
build:
    cargo build --workspace --all-targets

# Type-check the workspace without producing binaries — the fast dev loop.
check:
    cargo check --workspace --all-targets

# Cross-check the workspace against the shipping Windows ABI — catches
# `cfg(windows)` drift (unused imports, dead fields) a native check
# can't see.  exarch is excluded: its rustls -> aws-lc-sys dependency
# compiles C against Windows system headers, impossible from a Unix
# host, so exarch's Windows drift is gated by windows-check CI instead.
# synod sits atop exarch and inherits the same wall.
check-windows:
    CC_x86_64_pc_windows_msvc=cc-absent-use-blake3-pure-fallback RUSTFLAGS='-D warnings' cargo check --workspace --exclude exarch --exclude synod --all-targets --target x86_64-pc-windows-msvc

# Run the workspace test suite.  The `ral-core/test-util` feature pulls
# in the sandbox integration test that needs the confinement-token seam.
test:
    cargo test --workspace --features ral-core/test-util

# Format every crate in place.
fmt:
    cargo fmt

# Clippy across the workspace (the Cargo.toml workspace lints fire here, not
# under build).  `[workspace.lints.clippy] disallowed_methods = "deny"` makes
# the fs/process I/O-door denylist (clippy.toml) a hard error here, so a stray
# open/spawn outside a known door breaks the lint — no `-- -D` flag is needed,
# and using one would wrongly override the vendored ral-ripgrep-core's
# deliberate `[lints.clippy] all = "allow"` opt-out.  CI runs this exact
# command (.github/workflows/ci.yml).
lint:
    cargo clippy --workspace --all-targets

# Replay the Linux CI job locally (mirrors .github/workflows/ci.yml).
ci:
    cargo run -p ral --quiet -- scripts/ci-local.ral

# Render the static site into site/.
site:
    uv run scripts/render-site.py

# Build and install ral, exarch, and ral-sh from source.
install:
    cargo run -p ral --quiet -- scripts/install.ral

# Build the release matrix, passing flags through, e.g. `just release --local`.
release *args:
    cargo run -p ral --quiet -- scripts/build-release.ral {{args}}

# Run the current source as `ral`, forwarding arguments, e.g. `just run examples/hello.ral`.
run *args:
    cargo run -p ral --quiet -- {{args}}
