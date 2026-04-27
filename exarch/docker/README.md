# exarch-box

A Docker container for running `exarch` in permissive mode
(`EXARCH_DANGEROUS=1`).  The container is the trust boundary in place
of bubblewrap; the per-call `grant` becomes a no-op.

The default `Dockerfile` fetches a pre-built binary from a GitHub
Release at build time.  No checkout, no `shell-dev`, no rust
toolchain — just `docker compose build`.

## Layout

- `Dockerfile`            — fetches `exarch-linux-${TARGETARCH}` from
                             a GitHub Release, drops it into a hardened
                             debian:bookworm-slim image.
- `compose.yaml`          — read-only rootfs, all caps dropped,
                             `no-new-privileges`, tmpfs for `/tmp` and
                             `$HOME`, pids and memory capped.
- `entrypoint.sh`         — refuses to start unless at least one of
                             `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or
                             `OPENROUTER_API_KEY` is set.
- `Dockerfile.local`,
  `compose.local.yaml`,
  `build.sh`              — local-build variant; see below.

## Build

Default (rolling latest):

    docker compose -f exarch/docker/compose.yaml build

Pin a tag:

    EXARCH_VERSION=v0.1.0 \
        docker compose -f exarch/docker/compose.yaml build

The release artifacts come from `scripts/build-release.ral`, which
publishes `exarch-linux-{x86_64,arm64}` to a rolling `latest`
prerelease alongside the ral binaries.  `dev/deploy-public.ral`
mirrors that release to the public `lambdabetaeta/ral` repo, which is
the default `EXARCH_REPO` (no auth needed).  Override to fetch from
`lambdabetaeta/ral-private` if you have access:

    EXARCH_REPO=lambdabetaeta/ral-private \
    GITHUB_TOKEN=$(gh auth token) \
        docker compose -f exarch/docker/compose.yaml build

For a no-clone, no-build path, pull the prebuilt image instead — see
`exarch/README.md` for the `ghcr.io/lambdabetaeta/exarch-box` one-liner.

## Run

    docker compose -f exarch/docker/compose.yaml run --rm exarch-box

Pass arguments through to `exarch`:

    docker compose -f exarch/docker/compose.yaml run --rm exarch-box \
        --provider openai -p "do the thing"

Override the workspace location (default: `./workspace` next to this
file, gitignored):

    EXARCH_WORKSPACE=~/scratch \
        docker compose -f exarch/docker/compose.yaml run --rm exarch-box

Override the memory cap (default: 4g):

    EXARCH_MEMORY=8g \
        docker compose -f exarch/docker/compose.yaml run --rm exarch-box

## Provider keys

At least one must be in the host environment before `docker compose
run`.  The container inherits whichever are set:

    ANTHROPIC_API_KEY
    OPENAI_API_KEY
    OPENROUTER_API_KEY

The entrypoint refuses to start otherwise, with a clear message.  Set
them in your shell rc, a `.env` next to `compose.yaml`, or pass with
`--env`.

## Local-build variant (posterity)

`Dockerfile.local` + `compose.local.yaml` + `build.sh` keep the
build-from-this-checkout path around.  Use this when iterating on
exarch sources and wanting to test the change inside the
dangerous-mode container — the released image lags behind HEAD until
you tag and publish.

`build.sh` does five things:

1. `cd` to the workspace root.
2. Verify the `shell-dev` container is running.
3. `docker exec shell-dev cargo build --release --locked -p exarch` —
   produces a Linux ELF at `target/release/exarch`.  Building on the
   host (macOS) would produce a Mach-O that the image can't run.
4. `cp target/release/exarch exarch/docker/exarch-linux` — stages
   the binary where `Dockerfile.local` expects it.
5. `docker compose -f exarch/docker/compose.local.yaml build`.

Then:

    exarch/docker/build.sh
    docker compose -f exarch/docker/compose.local.yaml run --rm exarch-box-local

The local image and container are named `exarch-box-local` to avoid
colliding with the released `exarch-box`.
