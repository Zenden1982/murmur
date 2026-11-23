# Бэкенд Murmur. Сборка из корня репозитория:
#   docker build -f crates/backend/Dockerfile -t murmur/backend .
# (или через deploy/docker-compose.yml, профиль backend). Прокси для сборки — --build-arg HTTP(S)_PROXY.

ARG RUST_VERSION=1.100

FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /src
# Сначала только манифесты — слой с зависимостями кэшируется
COPY Cargo.toml Cargo.lock ./
# В образе только бэкенд: workspace сужаем до proto + backend, [patch] (vendor/ для клиента) убираем.
# Поэтому без --locked: cargo лишь выбрасывает из Cargo.lock ненужные пакеты, версии остальных — из него же.
RUN sed -i -e 's|^members = .*|members = ["crates/proto", "crates/backend"]|' -e '/^\[patch.crates-io\]/,$d' Cargo.toml
COPY crates/proto/Cargo.toml crates/proto/Cargo.toml
COPY crates/backend/Cargo.toml crates/backend/Cargo.toml
RUN mkdir -p crates/proto/src crates/backend/src \
 && echo "" > crates/proto/src/lib.rs \
 && echo "fn main() {}" > crates/backend/src/main.rs \
 && cargo build --release -p backend \
 && rm -rf crates/*/src target/release/deps/backend* target/release/deps/libbackend* target/release/deps/*proto*
COPY crates crates
RUN touch crates/proto/src/lib.rs crates/backend/src/main.rs crates/backend/src/lib.rs \
 && cargo build --release -p backend

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home /data app \
 && mkdir /data && chown app /data
COPY --from=build /src/target/release/backend /usr/local/bin/backend
USER app
WORKDIR /data
ENV DATABASE_URL=sqlite:///data/app.db \
    UPLOADS_DIR=/data/uploads \
    RELEASES_DIR=/data/releases \
    BIND_ADDR=0.0.0.0:8080 \
    RUST_LOG=info
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["backend"]
CMD ["serve"]
