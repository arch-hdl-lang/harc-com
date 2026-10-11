# Prebuilt HARC CI toolchain

The CI jobs use `ghcr.io/arch-hdl-lang/harc-ci` pinned by registry digest in
`.github/workflows/ci.yml`. The image contains Verilator 5.034 (including runtime
headers), Rust 1.94.0 with the minimal profile, Ubuntu 24.04 Clang, Z3 development
files, and ccache. Both base images are pinned by digest in `ci/Dockerfile`.

The final stage copies only Verilator's installed files from its official image;
its source-build dependencies and debug executables are excluded. The initial
validated image has 454.7 MiB of compressed layers. It contains toolchain files
only: Docker build context is `ci/`, never the HARC compiler or private fork code.

## Updating the image

1. Edit `ci/Dockerfile` and/or `ci/smoke.sh`.
2. Run the **Build CI image** workflow in public `arch-hdl-lang/harc-com`. It runs
   automatically for changes under `ci/` on main, and can be dispatched manually.
   The PR branch is included for bootstrapping this image.
3. The workflow verifies tool versions, compiles and runs a timed Verilator
   simulation with FST support and Clang, and compiles/links/runs a Z3 API program.
4. After publication, inspect the digest and compressed size from the
   `ci-image-digest` artifact or the workflow summary.
5. Update all five container image pins in `.github/workflows/ci.yml` to that
   digest. Image publication never silently updates CI consumers.

CI currently authenticates to GHCR with its repository `GITHUB_TOKEN` and
read-only package permissions. GHCR publishes new packages privately by default.
The organization must allow public package visibility and the `harc-ci` package
must be public to let forks consume it without cross-repository package grants.
Only the toolchain image should be made public; private source code remains
outside the image build context.

Cargo caches use `/opt/cargo` and a separate `cargo-ci-v1` namespace to avoid
restoring host-runner build products into the container toolchain. ccache caches
remain separated by backend and fixture shard.
