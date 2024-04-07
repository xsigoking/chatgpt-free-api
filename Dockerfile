FROM --platform=linux/amd64 messense/rust-musl-cross:x86_64-musl AS amd64
COPY . .
RUN cargo install --path . --root /

FROM --platform=linux/amd64 messense/rust-musl-cross:aarch64-musl AS arm64
COPY . .
RUN cargo install --path . --root /

FROM ${TARGETARCH} AS builder

FROM scratch
COPY --from=builder /bin/chatgpt-free-api /bin/chatgpt-free-api
STOPSIGNAL SIGINT
ENTRYPOINT ["/bin/chatgpt-free-api"]