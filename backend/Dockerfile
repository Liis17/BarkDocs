# Общий Dockerfile для всех сервисов backend.
# Какой сервис собрать — задаётся build-arg SERVICE (имя crate, например barkdocs-auth).

FROM rust:1-slim-trixie AS builder
ARG SERVICE
WORKDIR /app
COPY . .
RUN cargo build --release -p ${SERVICE}

# Дистрибутив тот же, что у builder: иначе бинарник не найдёт нужную версию glibc.
FROM debian:trixie-slim
ARG SERVICE
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/${SERVICE} /usr/local/bin/app
USER 65534
CMD ["app"]
