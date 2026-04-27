# exarch-box

A Docker container for running `exarch` in permissive mode
(`EXARCH_DANGEROUS=1`).  The container is the trust boundary in place
of bubblewrap; the per-call `grant` becomes a no-op.

The `Dockerfile` fetches a pre-built binary from a GitHub Release at
build time.  No checkout, no `shell-dev`, no rust toolchain.

## Layout

- `Dockerfile`     — fetches `exarch-linux-${TARGETARCH}` from a GitHub
                      Release, drops it into a hardened
                      `debian:bookworm-slim` image.
- `compose.yaml`   — read-only rootfs, all caps dropped,
                      `no-new-privileges`, tmpfs for `/tmp` and `$HOME`,
                      pids and memory capped.
- `entrypoint.sh`  — refuses to start unless at least one provider key
                      is set; forwards `EXARCH_PROVIDER`/`EXARCH_MODEL`
                      to `exarch` as `--provider`/`--model`.

## Build

Default (rolling latest):

    docker compose -f exarch/docker/compose.yaml build

Pin a tag:

    EXARCH_VERSION=v0.1.0 \
        docker compose -f exarch/docker/compose.yaml build

Bake a provider and model into the image:

    EXARCH_PROVIDER=openai EXARCH_MODEL=gpt-4o \
        docker compose -f exarch/docker/compose.yaml build

The release artifacts come from `scripts/build-release.ral`, which
publishes `exarch-linux-{x86_64,arm64}` to a rolling `latest`
prerelease alongside the ral binaries.  `dev/deploy-public.ral` mirrors
that release to `lambdabetaeta/ral`, which is the default `EXARCH_REPO`
(no auth needed).  Override to fetch from `lambdabetaeta/ral-private`:

    EXARCH_REPO=lambdabetaeta/ral-private \
    GITHUB_TOKEN=$(gh auth token) \
        docker compose -f exarch/docker/compose.yaml build

For a no-clone, no-build path, pull the prebuilt image — see
`exarch/README.md` for the `ghcr.io/lambdabetaeta/exarch-box` one-liner.

## Run

    docker compose -f exarch/docker/compose.yaml run --rm exarch-box

A REPL prompt (`▸`) opens.  Type a task and press Enter; EOF or `/quit`
exits.

## Passing flags

Any argument after the service name is forwarded verbatim to `exarch`.
Seed with a prompt string or a file instead of opening the REPL:

    docker compose -f exarch/docker/compose.yaml run --rm exarch-box \
        --prompt "describe this repo"

    docker compose -f exarch/docker/compose.yaml run --rm exarch-box \
        --file task.md

Same with the `docker run` one-liner:

    docker run --rm -it -e ANTHROPIC_API_KEY -v "$PWD:/work" \
        ghcr.io/lambdabetaeta/exarch-box --prompt "describe this repo"

To choose a provider or model, set `EXARCH_PROVIDER` and `EXARCH_MODEL` in
the host environment — do **not** pass them as CLI flags.  The entrypoint
prepends them and `clap` rejects duplicate flags:

    EXARCH_PROVIDER=openai EXARCH_MODEL=gpt-4o \
        docker compose -f exarch/docker/compose.yaml run --rm exarch-box \
        --prompt "describe this repo"

## Cleanup

`--rm` removes the container automatically on exit; the image stays.  To
remove it:

    docker rmi exarch-box                          # if built locally
    docker rmi ghcr.io/lambdabetaeta/exarch-box    # if pulled from the registry

Remove the image and wipe the build cache in one step:

    docker compose -f exarch/docker/compose.yaml down --rmi all
    docker builder prune

## Provider keys

At least one must be in the host environment before `docker compose
run`.  The container inherits whichever are set:

    ANTHROPIC_API_KEY
    OPENAI_API_KEY
    OPENROUTER_API_KEY

The entrypoint refuses to start otherwise, with a clear message.  Set
them in your shell rc, a `.env` next to `compose.yaml`, or pass with
`--env`.

## Provider and model

Set `EXARCH_PROVIDER` and `EXARCH_MODEL` in the host environment to
forward them as `--provider` and `--model` at startup:

    EXARCH_PROVIDER=openai EXARCH_MODEL=gpt-4o \
        docker compose -f exarch/docker/compose.yaml run --rm exarch-box

Alternatively, bake them into the image at build time (see Build above);
the env var at runtime takes precedence over what was baked in.

Valid values for `EXARCH_PROVIDER` are whatever `exarch --help` lists
under `--provider` (`anthropic`, `openai`, `openrouter`).  `EXARCH_MODEL`
is passed verbatim as a string.

Do not set `EXARCH_PROVIDER`/`EXARCH_MODEL` and also pass `--provider`/
`--model` as extra args to `docker compose run` — `clap` rejects
duplicate flags.

## Other overrides

Override the workspace location (default: `./workspace`, gitignored):

    EXARCH_WORKSPACE=~/scratch \
        docker compose -f exarch/docker/compose.yaml run --rm exarch-box

Override the memory cap (default: 4g):

    EXARCH_MEMORY=8g \
        docker compose -f exarch/docker/compose.yaml run --rm exarch-box
