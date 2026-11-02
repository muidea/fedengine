# Build with parent directory as context: docker build -f fedengine/docker/Dockerfile .
FROM rust:1.94.0-bookworm AS build
WORKDIR /build
COPY modorch-rs/Cargo.toml modorch-rs/Cargo.lock /build/modorch-rs/
COPY modorch-rs/crates /build/modorch-rs/crates
COPY modorch-rs/examples /build/modorch-rs/examples
COPY fedengine/Cargo.toml fedengine/Cargo.lock fedengine/rust-toolchain.toml /build/fedengine/
COPY fedengine/.cargo /build/fedengine/.cargo
COPY fedengine/src /build/fedengine/src
COPY fedengine/crates /build/fedengine/crates
WORKDIR /build/fedengine
RUN cargo build --release --locked
RUN mkdir -p /package && cp target/release/fedengine /package/ && find target/duckdb-download -name libduckdb.so -exec cp {} /package/ \;

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libstdc++6 libgomp1 && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 --create-home fedengine
COPY --from=build /package/fedengine /usr/local/bin/fedengine
COPY --from=build /package/libduckdb.so /usr/local/lib/libduckdb.so
RUN ldconfig && mkdir -p /var/lib/fedengine/tmp /etc/fedengine && chown -R fedengine:fedengine /var/lib/fedengine
COPY fedengine/config/application.toml /etc/fedengine/application.toml
USER fedengine
WORKDIR /var/lib/fedengine
EXPOSE 15432
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/fedengine"]
CMD ["--config", "/etc/fedengine/application.toml"]
