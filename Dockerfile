FROM rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922 AS build
RUN rustup toolchain install 1.98.1 --profile minimal
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml .env.example ./
COPY crates ./crates
COPY migrations ./migrations
COPY locales ./locales
RUN cargo build --release --locked --bin zapovit

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && groupadd -g 10001 zapovit && useradd -u 10001 -g zapovit -M -s /usr/sbin/nologin zapovit \
    && mkdir -p /var/lib/zapovit/journal && chown -R zapovit:zapovit /var/lib/zapovit
COPY --from=build /build/target/release/zapovit /usr/local/bin/zapovit
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/zapovit"]
CMD ["serve"]

FROM build AS fixture-build
RUN cargo build --release --locked -p adapters --example fake_telegram

FROM runtime AS integration-fixture
COPY --from=fixture-build /build/target/release/examples/fake_telegram /usr/local/bin/fake-telegram
ENTRYPOINT ["/usr/local/bin/fake-telegram"]

FROM runtime AS release
