ARG RUST_BUILD_IMAGE=rust:1.92-bookworm
FROM ${RUST_BUILD_IMAGE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
ARG SOURCE_COMMIT=unversioned
ENV SOURCE_COMMIT=${SOURCE_COMMIT}
RUN cargo build --locked --release --bin poly-us -j 1

FROM gcr.io/distroless/cc-debian12:nonroot
ARG SOURCE_COMMIT=unversioned
LABEL org.opencontainers.image.revision=${SOURCE_COMMIT}
COPY --from=build /src/target/release/poly-us /poly-us
WORKDIR /app
ENTRYPOINT ["/poly-us"]
CMD ["collect", "--config", "/app/collector.toml"]
