# Official Verilator 5.034 Linux amd64 image; runtime headers are included.
FROM verilator/verilator:v5.034@sha256:3bc71260e7bbc88928ffa5c053db48824c43e64e6665887baf4ac48c5307e289 AS verilator

# Copy only the installed tool and runtime, omitting source-build dependencies.
RUN find /usr/local -name '*_dbg' -delete

FROM ubuntu:24.04@sha256:f610ab94648195aa356059f5b41d6085c9d4d903c072430cdd1af7bdb646106b
COPY --from=verilator /usr/local/ /usr/local/

LABEL org.opencontainers.image.source="https://github.com/arch-hdl-lang/harc-com"
LABEL org.opencontainers.image.description="HARC CI: Verilator 5.034, Rust 1.94.0, Clang, Z3 and ccache"

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --no-install-recommends -y \
        clang libz3-dev curl pkg-config ccache make g++ perl python3 git \
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
