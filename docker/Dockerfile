# Builds one workspace binary: docker build -f docker/Dockerfile --build-arg SERVICE=shepherd-storage .
ARG RUST_IMAGE=lukemathwalker/cargo-chef:latest-rust-trixie

FROM ${RUST_IMAGE} AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG SERVICE
ARG TARGETARCH
ARG PDFIUM_RELEASE=chromium/8066
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang && rm -rf /var/lib/apt/lists/*
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p ${SERVICE}
COPY . .
RUN cargo build --release -p ${SERVICE} \
 && mkdir -p /out/config /out/lib /out/forms \
 && cp target/release/${SERVICE} /out/service \
 && if [ -d ${SERVICE}/config ]; then cp -r ${SERVICE}/config/. /out/config/; fi
# Form services need pdfium (rendering) and the form templates.
RUN case "${SERVICE}" in shepherd-intake|shepherd-workbench) \
      arch=$([ "${TARGETARCH}" = "arm64" ] && echo arm64 || echo x64); \
      curl -fsSL "https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_RELEASE}/pdfium-linux-${arch}.tgz" \
        | tar xz -C /tmp && cp /tmp/lib/libpdfium.so /out/lib/ \
      && cp -r forms/templates forms/uscis /out/forms/ ;; \
    esac

FROM gcr.io/distroless/cc-debian13:nonroot
WORKDIR /app
COPY --from=builder /out/service /app/service
COPY --from=builder /out/config /app/config
COPY --from=builder /out/lib /app/lib
COPY --from=builder /out/forms /app/forms
USER nonroot
ENTRYPOINT ["/app/service"]
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=3 CMD ["/app/service", "healthcheck"]
