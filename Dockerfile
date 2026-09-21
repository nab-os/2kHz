# syntax=docker/dockerfile:1.7
#
# The server half of 2kHz.
#
#   --target server     the Rust API alone: browsing, sync, embed, stream URLs,
#                       and the Rust crawler. 266MB. Starting a *Python* stage
#                       fails with "could not start uv", by construction.
#   --target pipeline   the above plus uv, ffmpeg and the analysis stack.
#                       1.6GB with layout alone, several GB with extract, and
#                       x86_64 only, the essentia-tensorflow pin ships one
#                       wheel, cp312 manylinux x86_64.
#
# Build from the repo root; the context needs app/, server/ and pipeline/.
#
#   docker build --target server   -t two-khz-server .
#   docker build --target pipeline -t two-khz-server:pipeline .
#
# /app is not arbitrary. `two_khz::qobuz::repo_root()` is CARGO_MANIFEST_DIR
# resolved at *compile* time, and the Python side resolves REPO_ROOT from its
# own __file__, so the build path and the run path have to be the same one.

ARG RUST_VERSION=1.96.0
ARG DEBIAN_SUITE=trixie
ARG UV_VERSION=0.9

# --------------------------------------------------------------------- build

# Its own stage rather than `COPY --from=ghcr.io/...:${UV_VERSION}` directly:
# a global ARG is in scope for FROM, but not inside a later stage.
FROM ghcr.io/astral-sh/uv:${UV_VERSION} AS uv

FROM rust:${RUST_VERSION}-${DEBIAN_SUITE} AS builder

# rusqlite is `bundled` and tokenizers brings onig, so both want a C compiler.
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY app ./app
COPY server ./server
# `db.rs` embeds it with include_str!, so it is a build input, not data.
COPY schema.sql ./schema.sql

# No GTK, no webkit: the server takes `two-khz-app` with default-features off
# and only the `local` feature, so nothing here wants a window.
#
# `ort` fetches an onnxruntime build during this step, so the build needs the
# network. Whatever it leaves behind gets carried into the runtime image; the
# cache mount means the target directory is gone by the time the layer lands,
# hence the copy into /out inside the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/server/target \
    set -eu; \
    cargo build --release --locked --manifest-path server/Cargo.toml; \
    mkdir -p /out; \
    cp server/target/release/two-khz-server /out/; \
    find server/target/release -name 'libonnxruntime*.so*' -exec cp {} /out/ \;

# -------------------------------------------------------------------- server

FROM debian:${DEBIAN_SUITE}-slim AS server

# libstdc++ for onnxruntime, curl for the healthcheck.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl libstdc++6 \
 && rm -rf /var/lib/apt/lists/*

# Not --system: that expects a uid below SYS_UID_MAX, and a high fixed uid is
# what keeps a bind-mounted /data chownable to something predictable.
RUN useradd --uid 10001 --user-group --create-home --home-dir /home/twokhz \
        --shell /usr/sbin/nologin twokhz

COPY --from=builder /out/ /opt/two-khz/
RUN set -eu; \
    mv /opt/two-khz/two-khz-server /usr/local/bin/two-khz-server; \
    find /opt/two-khz -name 'libonnxruntime*.so*' -exec mv {} /usr/local/lib/ \; ; \
    ldconfig; \
    rm -rf /opt/two-khz

# The corpus, the space and the device tokens all live in /data, it is the
# only thing here worth a backup. /cache is the audio excerpt scratch, which
# the analyse stage evicts as it goes.
# Docker does not read HOME from /etc/passwd, and `uv_path()` looks under it.
ENV HOME=/home/twokhz \
    TWO_KHZ_DATA_DIR=/data \
    TWO_KHZ_MODEL_DIR=/data/models \
    TWO_KHZ_CACHE_DIR=/cache/audio \
    HF_HOME=/data/.cache/huggingface

# Python's MODEL_DIR is REPO_ROOT/data/models with no environment override,
# so the symlink is what keeps one copy of the weights rather than two.
RUN set -eu; \
    mkdir -p /data/models /cache/audio /app; \
    ln -s /data /app/data; \
    chown -R twokhz:twokhz /data /cache /app

WORKDIR /app
USER twokhz
EXPOSE 7700

HEALTHCHECK --interval=30s --timeout=3s --start-period=15s --retries=3 \
    CMD curl -fsS http://127.0.0.1:7700/api/health || exit 1

# Subcommands pass straight through: `docker run … pair --name phone`.
ENTRYPOINT ["two-khz-server"]
CMD ["serve", "--bind", "0.0.0.0:7700"]

# ------------------------------------------------------------------ pipeline

FROM server AS pipeline
USER root

# ffmpeg does the range-requested excerpt pulls in two_khz/audio.py.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg \
 && rm -rf /var/lib/apt/lists/*

# `stages.rs` spawns `uv run two-khz <stage>` from /app/pipeline, falling back
# to PATH when ~/.local/bin/uv is absent.
COPY --from=uv /uv /usr/local/bin/uv

# The environment lives outside /app/pipeline so that bind-mounting the
# pipeline source over it for development does not hide the venv.
ENV UV_PROJECT_ENVIRONMENT=/opt/venv \
    UV_PYTHON_INSTALL_DIR=/opt/python \
    UV_CACHE_DIR=/tmp/uv-cache

COPY pipeline /app/pipeline
# Rust embeds the schema at compile time; Python reads REPO_ROOT/schema.sql at
# run time, so this target needs the file itself.
COPY schema.sql /app/schema.sql

# `extract` is the expensive one: torch plus essentia-tensorflow, and the
# reason this target is multi-GB. Drop it with
#   --build-arg PIPELINE_EXTRAS="--extra layout"
# if this machine only ever runs build-space and layout.
ARG PIPELINE_EXTRAS="--extra extract --extra layout"
RUN --mount=type=cache,target=/tmp/uv-cache \
    set -eu; \
    uv sync --project /app/pipeline --frozen --no-dev ${PIPELINE_EXTRAS}; \
    chown -R twokhz:twokhz /opt/venv /opt/python /app/pipeline

# Everything is installed already; without this, `uv run` would try to re-sync
# on each stage start and fail whenever the index is unreachable.
ENV UV_NO_SYNC=1

USER twokhz
