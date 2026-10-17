# syntax=docker/dockerfile:1.7
#
# Two images from one source: the gateway, and the gateway with the difficulty
# classifier and its artefact already inside it.
#
# For an air-gapped deployment one artefact beats two — an operator moving a model
# file alongside an image has two things to transfer, checksum and keep in step.
#
#   docker build --target plain -t intelli-prism:latest .
#   docker build --target classifier -t intelli-prism:embed8 \
#     --build-context model=/path/to/difficulty-onnx-embed8 \
#     --build-context runtime=/path/to/onnxruntime/lib \
#     --build-arg MODEL_BUILD=embed8 --build-arg MODEL_VERSION=1 .
#
# The model is passed in rather than fetched, and nothing here holds a credential.
# Once the weights have a public url the same stage fetches them with the gateway's
# own `model fetch`, which verifies on the way in exactly as the copy below does.

ARG RUST=1-slim-trixie
ARG RUNTIME=debian:trixie-slim

# ——— one dependency build both images share ———
FROM rust:${RUST} AS builder
# ca-certificates for crates.io; the rest is what ring, onig and the sqlite driver
# want from a C toolchain. No version pins: the base image's own are what it tested.
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install --no-install-recommends -y build-essential pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# A mirror for anyone who needs one; crates.io direct when the argument is empty.
ARG CARGO_MIRROR=""
RUN if [ -n "${CARGO_MIRROR}" ]; then \
      mkdir -p "${CARGO_HOME}" && printf \
        '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "%s"\n' \
        "${CARGO_MIRROR}" > "${CARGO_HOME}/config.toml"; \
    fi
COPY . .

FROM builder AS build-plain
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --bin intelli-prism \
    && cp target/release/intelli-prism /intelli-prism

FROM builder AS build-classifier
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --bin intelli-prism --features classifier \
    && cp target/release/intelli-prism /intelli-prism

# ——— the gateway, and nothing it does not need ———
FROM ${RUNTIME} AS plain
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 prism
COPY --from=build-plain /intelli-prism /usr/local/bin/intelli-prism
USER prism
EXPOSE 8080
ENTRYPOINT ["intelli-prism"]
LABEL org.opencontainers.image.title="intelli-prism" \
      org.opencontainers.image.description="An enterprise AI proxy and gateway" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# ——— the same gateway, with a classifier it can actually run ———
FROM ${RUNTIME} AS classifier
# Which build is inside, because the quantization carries its own calibration and two
# images must be tellable apart without being started.
ARG MODEL_BUILD
ARG MODEL_VERSION
ARG ORT_VERSION=1.28.0
ENV IP_MODEL_DIR=/opt/intelli-prism/difficulty
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 prism
COPY --from=build-classifier /intelli-prism /usr/local/bin/intelli-prism
# The inference runtime is loaded by name at open, never linked, so the image carries
# the shared object and says where it is. This is what the classifier image costs over
# the plain one beyond the weights themselves.
# `runtime` and `model` are build contexts, not stages: the library and the weights
# are passed in rather than fetched, which is what lets this build run air-gapped and
# hold no credential.
# hadolint ignore=DL3022
COPY --from=runtime libonnxruntime.so.${ORT_VERSION} /usr/local/lib/
ENV ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so.${ORT_VERSION}
# Owned as it lands: a `chown -R` afterwards rewrites every byte of the weights into
# a second layer, which costs the image the size of the model twice over.
# hadolint ignore=DL3022
COPY --from=model --chown=prism . ${IP_MODEL_DIR}/
# Verified while the image is assembled, so bad weights fail the build rather than
# ship — the same check the gateway makes at startup, by the same code. Read once:
# each run hashes every byte of the weights, and the labels are checked against what
# it said rather than against what the build was told.
RUN set -eu; \
    said="$(intelli-prism model verify --at "${IP_MODEL_DIR}")"; \
    printf '%s\n' "$said"; \
    case "$said" in \
      *"build        ${MODEL_BUILD}"*) ;; \
      *) echo "this image is labelled ${MODEL_BUILD}; the artefact is not" >&2; exit 1 ;; \
    esac; \
    case "$said" in \
      *"model        ${MODEL_VERSION}"*) ;; \
      *) echo "this image is labelled model ${MODEL_VERSION}; the artefact is not" >&2; exit 1 ;; \
    esac
USER prism
EXPOSE 8080
ENTRYPOINT ["intelli-prism"]
LABEL org.opencontainers.image.title="intelli-prism" \
      org.opencontainers.image.description="An enterprise AI proxy and gateway, with the difficulty classifier" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      ai.intelli-prism.model.build="${MODEL_BUILD}" \
      ai.intelli-prism.model.version="${MODEL_VERSION}" \
      ai.intelli-prism.onnxruntime.version="${ORT_VERSION}"
