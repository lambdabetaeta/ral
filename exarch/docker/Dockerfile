# Hardened runtime image for exarch in permissive mode.  The binary is
# fetched at build time from a GitHub Release — no local checkout, no
# rust toolchain.  Pin a specific tag with --build-arg
# EXARCH_VERSION=v… ; the default `latest` tracks the rolling
# prerelease that build-release.ral publishes.
#
#   docker compose -f compose.yaml build
#   docker compose -f compose.yaml run --rm exarch-box
#
# For private repos, pass a token at build time:
#   docker compose -f compose.yaml build \
#       --build-arg GITHUB_TOKEN=$(gh auth token)
#
# Bake a default provider/model into the image:
#   EXARCH_PROVIDER=openai EXARCH_MODEL=gpt-4o \
#       docker compose -f compose.yaml build
# Both can be overridden at runtime via env vars.

FROM debian:bookworm-slim AS download
ARG EXARCH_VERSION=latest
ARG EXARCH_REPO=lambdabetaeta/ral
ARG GITHUB_TOKEN=
ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# TARGETARCH is `amd64` or `arm64`; release artifacts use `x86_64` /
# `arm64`.  Fail loudly on anything else rather than fetching garbage.
RUN set -eu; \
    case "$TARGETARCH" in \
        amd64) ARCH=x86_64 ;; \
        arm64) ARCH=arm64  ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    URL="https://github.com/${EXARCH_REPO}/releases/download/${EXARCH_VERSION}/exarch-linux-${ARCH}"; \
    AUTH=""; \
    [ -n "$GITHUB_TOKEN" ] && AUTH="-H Authorization: token $GITHUB_TOKEN"; \
    curl -fsSL $AUTH "$URL" -o /tmp/exarch; \
    chmod +x /tmp/exarch; \
    file /tmp/exarch || true

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    git \
    curl \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

RUN git config --system --add safe.directory '*'

RUN useradd --create-home --shell /bin/bash --uid 1000 exarch

COPY --from=download /tmp/exarch /usr/local/bin/exarch
COPY entrypoint.sh /usr/local/bin/exarch-entrypoint
RUN chmod +x /usr/local/bin/exarch-entrypoint

ENV EXARCH_DANGEROUS=1

ARG EXARCH_PROVIDER=
ARG EXARCH_MODEL=
ENV EXARCH_PROVIDER=${EXARCH_PROVIDER}
ENV EXARCH_MODEL=${EXARCH_MODEL}

USER exarch
WORKDIR /work
ENTRYPOINT ["/usr/local/bin/exarch-entrypoint"]
