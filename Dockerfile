# Build and runtime image for the uRouter Gateway.
#
# The runtime stage is distroless: the Gateway needs no shell, package manager or
# writable filesystem, and its TLS comes from rustls rather than system OpenSSL.
# Only glibc and the CA bundle are required, which is what `cc-debian12` provides.
#
# The build stage is deliberately a single plain `cargo build` rather than a
# manifest-stub dependency-cache layer. The stub trick is easy to get subtly
# wrong across a 16-crate workspace and it buys nothing in CI, where the cache is
# cold anyway. Use `--mount=type=cache` with BuildKit if local rebuild time
# matters.

FROM rust:1.97-bookworm AS builder
WORKDIR /src
COPY . .
RUN set -eux; \
    cargo build --release -p urouter-gateway; \
    strip target/release/urouter-gateway

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
WORKDIR /app
COPY --from=builder /src/target/release/urouter-gateway /usr/local/bin/urouter-gateway
# The checked-in Catalog and Route are the default control plane. Mount over
# /app/catalog and /app/gateway to deploy a reviewed revision instead.
COPY --from=builder /src/catalog /app/catalog
COPY --from=builder /src/gateway /app/gateway

EXPOSE 8787
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/urouter-gateway"]
# 0.0.0.0 rather than the CLI's 127.0.0.1 default: a loopback bind is unreachable
# from outside the container.
CMD ["--bind", "0.0.0.0:8787"]
