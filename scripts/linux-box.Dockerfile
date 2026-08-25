# The Linux box `just linux-ci` runs in — the only place a macOS host can
# compile or run `#[cfg(target_os = "linux")]` code at all, since
# core/src/sandbox.rs gates `mod linux` out of a native build.
#
# Deliberately lean: the cargo toolchain and bubblewrap, nothing else.  The
# site toolchain stays on the host — which is why scripts/ci.sh runs
# render-site.py natively in both of its modes.
#
# Unpinned base on purpose.  rust-toolchain.toml tracks `stable` and CI
# installs `stable`, so a pin here would be the one place claiming a
# version the other two do not.
FROM rust:bookworm

# build-essential and pkg-config cover the crates pulling in cc-rs or
# *-sys; libssl-dev the handful preferring system OpenSSL to rustls.
#
# bubblewrap is the Linux sandbox backend itself.  Absent, every test that
# spawns an envelope *skips* — which reads exactly like a pass, and is how
# a `deny` rendering that could not even launch went unnoticed.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        cmake \
        git \
        curl \
        ca-certificates \
        bubblewrap \
 && rm -rf /var/lib/apt/lists/*

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

USER ${USER}
WORKDIR /workspace
CMD ["/bin/bash"]
