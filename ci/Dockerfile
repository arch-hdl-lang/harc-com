# Official Verilator 5.034 Linux amd64 image; runtime headers are included.
FROM verilator/verilator:v5.034@sha256:3bc71260e7bbc88928ffa5c053db48824c43e64e6665887baf4ac48c5307e289 AS verilator

# Copy only the installed tool and runtime, omitting source-build dependencies.
RUN find /usr/local -name '*_dbg' -delete

FROM ubuntu:24.04@sha256:f610ab94648195aa356059f5b41d6085c9d4d903c072430cdd1af7bdb646106b AS base
COPY --from=verilator /usr/local/ /usr/local/

LABEL org.opencontainers.image.source="https://github.com/arch-hdl-lang/harc-com"
LABEL org.opencontainers.image.description="HARC CI: Verilator 5.034, Rust 1.94.0, Clang, Z3 and ccache"

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --no-install-recommends -y \
        clang libz3-dev curl pkg-config ccache make g++ perl python3 git xz-utils zstd \
        ca-certificates libgoogle-perftools4 libtcmalloc-minimal4 libfl2 zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
ENV PATH="/opt/cargo/bin:${PATH}"
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && sh /tmp/rustup-init.sh -y --profile minimal --default-toolchain 1.94.0 \
    && rm /tmp/rustup-init.sh \
    && rustc --version && cargo --version \
    && verilator --version && clang++ --version && ccache --version

# GitHub Actions runs shell steps, rather than the upstream Verilator wrapper.
ENTRYPOINT []
CMD ["/bin/bash"]

# Opt-in variant with the Lean 4 toolchain, used by arch-com's `lean proofs`
# workflow to replay compiler proof certificates. Kept as a separate target
# (and tag) because the toolchain adds ~2.8 GB uncompressed / ~0.8 GB
# compressed that no HARC job needs. Both downloads come from GitHub releases
# and are pinned by checksum; the toolchain is registered with elan under the
# exact name that arch-com's `lean-toolchain` file requests, so `lake` never
# downloads anything at job time. Bump LEAN_VERSION together with
# arch-com/proofs/*/lean-toolchain.
FROM base AS lean
ARG ELAN_VERSION=4.2.4
ARG ELAN_SHA256=42b94d4244e8353142c456ec0e4ca6528fd898a6c604d4059f494e706e431f63
ARG LEAN_VERSION=4.30.0
ARG LEAN_SHA256=4dad74141c2c119ca1aa626656be83b8e14238afba97271fd7bf1eb3f081b319
LABEL org.opencontainers.image.description="HARC CI + Lean ${LEAN_VERSION}: Verilator 5.034, Rust 1.94.0, Clang, Z3, ccache, elan"
ENV ELAN_HOME=/opt/elan
ENV PATH="/opt/elan/bin:${PATH}"
RUN curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/elan.tar.gz \
        "https://github.com/leanprover/elan/releases/download/v${ELAN_VERSION}/elan-x86_64-unknown-linux-gnu.tar.gz" \
    && echo "${ELAN_SHA256}  /tmp/elan.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/elan.tar.gz -C /tmp \
    && /tmp/elan-init -y --no-modify-path --default-toolchain none \
    && curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/lean.tar.zst \
        "https://github.com/leanprover/lean4/releases/download/v${LEAN_VERSION}/lean-${LEAN_VERSION}-linux.tar.zst" \
    && echo "${LEAN_SHA256}  /tmp/lean.tar.zst" | sha256sum -c - \
    && mkdir -p /opt/lean \
    && tar --zstd -xf /tmp/lean.tar.zst -C /opt/lean \
    && elan toolchain link "leanprover/lean4:v${LEAN_VERSION}" "/opt/lean/lean-${LEAN_VERSION}-linux" \
    && elan default "leanprover/lean4:v${LEAN_VERSION}" \
    && rm -f /tmp/elan.tar.gz /tmp/elan-init /tmp/lean.tar.zst \
    && lean --version && lake --version
