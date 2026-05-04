FROM rust:1.88-bookworm

ENV PATH="/usr/local/cargo/bin:${PATH}"

RUN apt-get update && apt-get install -y \
    build-essential \
    bubblewrap \
    ripgrep \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work

CMD ["bash"]
