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

# Run the workspace test suite.  The `ral-core/test-util` feature pulls
# in the sandbox integration test that needs the confinement-token seam.
test:
    cargo test --workspace --features ral-core/test-util

# Format every crate in place.
fmt:
    cargo fmt

# Clippy across the workspace (the Cargo.toml workspace lints fire here, not under build).
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
