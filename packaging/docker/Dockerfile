# syntax=docker/dockerfile:1
# Track 3 submission image: one static linux/amd64 fastsim binary, on PATH as both verbs
# (`simulate` and `simulate-batch`; the binary dispatches on argv[0]). No ENTRYPOINT.
#
# Built by sub/tools/build.py from a whitelist of source and public PGO inputs.
# Do not build it from the workspace root: sub/.env must never be in a context.
#
# Build stage: the exact image the Linux exactness checks ran in (rust:1-slim on 2026-09-25 is
# this digest: Rust 1.98.1, Debian 13, glibc 2.41). glibc is linked statically, so exp/log in the
# binary are glibc 2.41's, the libm NumPy used for the reference traces (python:3.11-slim is
# Debian 13 too). x86-64-v3 enables AVX2/BMI/LZCNT on the published Xeon Platinum 8570 hosts.
# It is an explicit, reproducible ISA target, independent of the machine doing the build. Pass
# --build-arg TARGET_CPU=x86-64 for an older CPU; that build also gets its own PGO training.
# LLVM lays out each function's blocks by the profile's ext-TSP order (fewer taken branches and
# cache lines on the hot path; outputs are the same, layout only). The binary is a static
# executable at a fixed address rather than a static PIE: no relocating itself at start, and
# absolute jump tables and addresses in the code (2-3% less time on the lab; outputs the same).
# build.py passes --platform linux/amd64 for every stage and refuses any other result.
FROM rust:1.98.1-slim-trixie@sha256:f47a8de237dcbb0b0ce1099901e60a89728e3d51f24e664b40e947171538ade7 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tools/pgo_build.py ./tools/pgo_build.py
COPY training ./training
ARG PGO=1
ARG TARGET_CPU=x86-64-v3
ENV CARGO_TERM_COLOR=never \
    CARGO_PROFILE_RELEASE_STRIP=symbols \
    RUSTFLAGS="-C target-feature=+crt-static -C relocation-model=static -C target-cpu=${TARGET_CPU} -C llvm-args=-enable-ext-tsp-block-placement"
RUN if [ "$PGO" = 1 ]; then \
      apt-get update && apt-get install -y --no-install-recommends --no-upgrade python3 \
      && rustup component add llvm-tools-preview \
      && python3 tools/pgo_build.py --units /src/training --target x86_64-unknown-linux-gnu \
           --work-dir /pgo --out /out/usr/local/bin/simulate; \
    else \
      cargo build --release --locked --bin fastsim --target x86_64-unknown-linux-gnu \
      && install -D -m 0755 target/x86_64-unknown-linux-gnu/release/fastsim /out/usr/local/bin/simulate; \
    fi \
 && ln -s simulate /out/usr/local/bin/simulate-batch \
 && mkdir -p /out/etc /out/tmp && chmod 1777 /out/tmp \
 && if readelf -l /out/usr/local/bin/simulate | grep -q INTERP; then echo "not static" >&2; exit 1; fi \
 && if readelf -d /out/usr/local/bin/simulate | grep -q NEEDED; then echo "has shared deps" >&2; exit 1; fi

# Runtime root: the build stage's binary in /bin, an empty /etc and /tmp, and nothing else. The
# stage above stays byte for byte as it was, so a cached build reuses its binary.
#
# Every Track 3 unit runs with a GPU. On each start the NVIDIA hook or CDI spec mounts the driver
# files into the root, adds links, and runs ldconfig there, so the less the root holds, the less
# that costs (on x86 with a Tesla T10: 162 ms on Debian 13 slim, 124 ms on an empty root). Where
# those files go matters too: when a directory of the image gets a new entry during the start,
# overlayfs copies it up and, from Linux 6.10.14/6.11.11 on, fsyncs it, inside the timed window.
# The injection writes into /usr (bin, lib, share), so the image has no /usr at all, and it writes
# /etc (ld.so.cache, ld.so.conf.d), which build.py copies up before the start: it sets the
# image's WorkingDir to /etc/fastsim in the config only, and Docker creates that directory when it
# creates the container, outside the Final's StartedAt-FinishedAt window. A Dockerfile WORKDIR
# cannot do this, because it creates the directory in the image. /etc stays for toolkits that do
# not create it for ld.so.cache.
FROM build AS layout
RUN install -D -m 0755 /out/usr/local/bin/simulate /rootfs/bin/simulate \
 && ln -s simulate /rootfs/bin/simulate-batch \
 && mkdir -p /rootfs/etc /rootfs/tmp && chmod 1777 /rootfs/tmp

FROM scratch
ARG VERSION=dev
COPY --from=layout /rootfs/ /
LABEL qfbench2.interface_version="2.0" \
      org.opencontainers.image.title="fastsim" \
      org.opencontainers.image.description="Agenthon 2026 Track 3 market simulator (ABIDES-exact, Rust)" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.licenses="Apache-2.0"
# The platform runs as uid 65534 with a read-only root and a noexec /tmp; nothing is written
# outside /output, but HOME points somewhere writable anyway. The verbs resolve on PATH (/bin).
# The simulator computes nothing on the GPU, so the image asks the NVIDIA hook for the `utility`
# driver files only (5 libraries mounted instead of 20; the device is granted as before). Only
# a listed capability may be named here: an unknown one such as `none` fails the container's
# start, and an empty value means the default set. The official launchers pass
# `-e NVIDIA_DRIVER_CAPABILITIES=compute`, which takes precedence over this value, and Docker 29.3
# and later inject `--gpus` through CDI when a spec exists, which mounts every driver file anyway.
# Both boards time the whole container, so a run writes its outputs while it simulates, card or
# no card. That is also the binary's own default since release 1.23 (`streams` in
# src/bin/fastsim.rs); the variable stays as the explicit setting.
ENV PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    HOME=/tmp \
    NVIDIA_DRIVER_CAPABILITIES=utility \
    FASTSIM_STREAM=1
USER 65534:65534
