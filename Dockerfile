FROM ubuntu:latest AS builder
RUN apt-get update && \
    apt-get install -y cmake gcc curl pkg-config libssl-dev git &&\
    rm -rf  /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && \
    echo 'fn main() { println!("Dummy"); }' > src/main.rs && \
    cargo build --release && \
    rm -rf src
COPY . .
RUN touch src/main.rs &&\
    cargo build --release

FROM ubuntu:latest
WORKDIR /app
RUN apt-get update && \
    apt-get install -y ca-certificates libssl-dev && \
    rm -rf /var/lib/apt/lists/* &&\
    update-ca-certificates --fresh
COPY --from=builder /app/target/release/telegram-images-bot /app/
CMD ["/app/telegram-images-bot"]