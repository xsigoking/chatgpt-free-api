FROM alpine as builder
ARG REPO VER TARGETPLATFORM
RUN if [ "$TARGETPLATFORM" = "linux/amd64" ]; then \ 
        TARGET="x86_64-unknown-linux-musl"; \
    elif [  "$TARGETPLATFORM" = "linux/arm64" ]; then \
        TARGET="aarch64-unknown-linux-musl"; \
    elif [  "$TARGETPLATFORM" = "linux/386" ]; then \
        TARGET="i686-unknown-linux-musl"; \
    elif [  "$TARGETPLATFORM" = "linux/arm/v7" ]; then \
        TARGET="armv7-unknown-linux-musleabihf"; \
    fi && \
    wget https://github.com/${REPO}/releases/download/${VER}/chatgpt-free-api-${VER}-${TARGET}.tar.gz && \
    tar -xf chatgpt-free-api-${VER}-${TARGET}.tar.gz && \
    mv chatgpt-free-api /bin/

FROM scratch
COPY --from=builder /bin/chatgpt-free-api /bin/chatgpt-free-api
STOPSIGNAL SIGINT
ENTRYPOINT ["/bin/chatgpt-free-api"]