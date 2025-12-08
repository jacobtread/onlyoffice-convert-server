#  Builder part
FROM rust:1.91.0-slim-bookworm AS builder

WORKDIR /app

# Dependency precachng
COPY Cargo.toml .
COPY Cargo.lock .
COPY client/Cargo.toml ./client/Cargo.toml
RUN mkdir src && echo "fn main() {}" >src/main.rs
RUN mkdir client/src && echo "fn main() {}" >client/src/main.rs
RUN cargo build --release

COPY src src
COPY client/src client/src
RUN touch src/main.rs

RUN cargo build --release

# ----------------------------------------
# Runner part
# ----------------------------------------
FROM jacobtread/onlyoffice-x2t-docker-base AS runner

WORKDIR /app

# Copy the built binary
COPY --from=builder /app/target/release/onlyoffice-convert-server ./

ENV X2T_PATH=/var/www/onlyoffice/documentserver/server/FileConverter/bin
ENV SERVER_ADDRESS=0.0.0.0:3000

EXPOSE 3000

CMD ["/app/onlyoffice-convert-server"]
