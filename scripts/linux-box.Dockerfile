# The Linux box `just linux-ci` runs in — the only place a macOS host can
# compile or run `#[cfg(target_os = "linux")]` code at all, since
# core/src/sandbox.rs gates `mod linux` out of a native build.
#
# Deliberately lean: the cargo toolchain, bubblewrap, and `just` — which is
# here because scripts/ci.sh runs its steps as justfile recipes, so the image
# needs the same command a developer types.  The site toolchain stays on the
# host, which is why scripts/ci.sh runs `just site` natively in both modes.
#
# Cargo targets musl, as every Linux artefact ships (the releases, synod's
# guest); glibc builds only the build scripts and proc macros.
#
# Unpinned base on purpose.  rust-toolchain.toml tracks `stable` and CI
# installs `stable`, so a pin here would be the one place claiming a
# version the other two do not.
FROM rust:bookworm

# build-essential and pkg-config cover the crates pulling in cc-rs or
# *-sys; libssl-dev the handful preferring system OpenSSL to rustls.
# musl-tools gives them `musl-gcc`, the C half of the musl target.
#
# bubblewrap is the Linux sandbox backend itself.  Absent, every test that
# spawns an envelope *skips* — which reads exactly like a pass, and is how
# a `deny` rendering that could not even launch went unnoticed.
#
# ripgrep is `just lint`'s em-dash check.  Absent, `! rg` reads "not found" as
# "no match" and passes.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        cmake \
        musl-tools \
        git \
        curl \
        ca-certificates \
        bubblewrap \
        ripgrep \
 && rm -rf /var/lib/apt/lists/*

# Pinned, unlike the base: the toolchain follows rust-toolchain.toml's `stable`,
# but nothing else here states a `just` version, so this is the only claim and
# cannot contradict one.  Built for the host's own architecture — a macOS
# checkout builds this image arm64, GitHub's runner amd64.
ARG JUST_VERSION=1.58.0
RUN triple="$(uname -m)-unknown-linux-musl" \
 && mkdir /.cargo \
 && printf '[build]\ntarget = "%s"\n' "$triple" > /.cargo/config.toml \
 && curl -fsSL "https://github.com/casey/just/releases/download/${JUST_VERSION}/just-${JUST_VERSION}-${triple}.tar.gz" \
    | tar -xz -C /usr/local/bin just

# A non-root user, so anything written through the bind-mounted source tree
# lands owned by the same UID that owns it outside.
ARG USER=dev
ARG UID=1000
ARG GID=1000
RUN groupadd --gid ${GID} ${USER} \
 && useradd  --uid ${UID} --gid ${GID} --create-home --shell /bin/bash ${USER} \
 && install -d -o ${USER} -g ${USER} /home/${USER}/.cargo /workspace

# Both paths are named volumes at run time (see scripts/ci.sh), so
# the registry and the Linux artefacts stay inside docker's own storage instead
# of crossing the bind mount into the host's target/.
ENV CARGO_HOME=/home/${USER}/.cargo
ENV CARGO_TARGET_DIR=/workspace/.target-linux

# Debian's arm64 `musl-gcc` links glibc's libgcc, whose outline-atomics helper
# wants glibc's `__getauxval`: jemalloc's configure then finds no atomics.
# Inline ones need no helper; read only when the target is aarch64 musl.
ENV CFLAGS_aarch64_unknown_linux_musl=-mno-outline-atomics

USER ${USER}
WORKDIR /workspace
CMD ["/bin/bash"]
