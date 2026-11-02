# The container `just box` runs recipes in: a Mac's only way to compile or run
# `cfg(target_os = "linux")` code.  Cargo targets musl, as every Linux release
# ships; glibc builds only build scripts and proc macros.
#
# Unpinned: rust-toolchain.toml tracks `stable`, so a pin here would disagree.
FROM rust:bookworm

# musl-tools: the C half of the musl target.  clang: assembles psm for the
# other arch without a cross toolchain (`just check`).  bubblewrap: the sandbox
# itself; without it envelope tests skip, which reads like a pass, unless
# RAL_REQUIRE_SANDBOX is set.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        cmake \
        musl-tools \
        clang \
        git \
        curl \
        unzip \
        ca-certificates \
        bubblewrap \
 && rm -rf /var/lib/apt/lists/*

ARG JUST_VERSION=1.58.0
RUN triple="$(uname -m)-unknown-linux-musl" \
 && mkdir /.cargo \
 && printf '[build]\ntarget = "%s"\n' "$triple" > /.cargo/config.toml \
 && curl -fsSL "https://github.com/casey/just/releases/download/${JUST_VERSION}/just-${JUST_VERSION}-${triple}.tar.gz" \
    | tar -xz -C /usr/local/bin just

ARG DENO_VERSION=2.5.6
RUN curl -fsSL "https://github.com/denoland/deno/releases/download/v${DENO_VERSION}/deno-$(uname -m)-unknown-linux-gnu.zip" \
        -o /tmp/deno.zip \
 && unzip /tmp/deno.zip -d /usr/local/bin \
 && rm /tmp/deno.zip

# Non-root, so files written through the bind mount keep the host owner's UID.
ARG USER=dev
ARG UID=1000
ARG GID=1000
RUN groupadd --gid ${GID} ${USER} \
 && useradd  --uid ${UID} --gid ${GID} --create-home --shell /bin/bash ${USER} \
 && install -d -o ${USER} -g ${USER} /home/${USER}/.cargo /workspace

# Named volumes at run time, keeping the registry and Linux artefacts off the
# bind mount.
ENV CARGO_HOME=/home/${USER}/.cargo
ENV CARGO_TARGET_DIR=/workspace/.target-linux

# Debian's arm64 musl-gcc links glibc's libgcc, whose outline atomics want
# `__getauxval`; jemalloc's configure then finds no atomics.
ENV CFLAGS_aarch64_unknown_linux_musl=-mno-outline-atomics

USER ${USER}
WORKDIR /workspace
CMD ["/bin/bash"]
