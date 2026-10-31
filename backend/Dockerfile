# Общий Dockerfile для всех сервисов backend.
# Какой сервис собрать — задаётся build-arg SERVICE (имя crate, например barkdocs-auth).

FROM rust:1-slim AS builder
ARG SERVICE
WORKDIR /app
COPY . .
RUN cargo build --release -p ${SERVICE}

FROM debian:bookworm-slim
ARG SERVICE
COPY --from=builder /app/target/release/${SERVICE} /usr/local/bin/app
CMD ["app"]
