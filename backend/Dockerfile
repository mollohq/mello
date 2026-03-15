FROM heroiclabs/nakama-pluginbuilder:3.21.0 AS builder

WORKDIR /backend
COPY nakama/data/modules/ .
RUN go mod tidy && go build -buildmode=plugin -trimpath -o ./backend.so .

FROM heroiclabs/nakama:3.21.0

COPY --from=builder /backend/backend.so /nakama/data/modules/
COPY nakama/data/nakama.yml /nakama/data/nakama.yml

EXPOSE 7350

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:7350/healthcheck || exit 1

ENTRYPOINT ["/bin/sh", "-c", "\
    /nakama/nakama migrate up \
        --database.address=${DATABASE_URL} && \
    exec /nakama/nakama \
        --config /nakama/data/nakama.yml \
        --database.address=${DATABASE_URL} \
        --socket.server_key=${NAKAMA_SERVER_KEY:-defaultkey} \
        --runtime.http_key=${NAKAMA_HTTP_KEY:-defaulthttpkey} \
        --console.username=${NAKAMA_CONSOLE_USERNAME:-admin} \
        --console.password=${NAKAMA_CONSOLE_PASSWORD:-admin} \
        --console.signing_key=${NAKAMA_CONSOLE_SIGNING_KEY:-defaultsigningkey} \
        --session.encryption_key=${NAKAMA_SESSION_ENCRYPTION_KEY:-defaultencryptionkey} \
        --session.refresh_encryption_key=${NAKAMA_SESSION_REFRESH_ENCRYPTION_KEY:-defaultrefreshkey} \
        --console.port=7351 \
        --socket.port=${PORT:-7350} \
        --social.apple.bundle_id=${APPLE_CLIENT_ID:-} \
        --social.steam.publisher_key=${STEAM_PUBLISHER_KEY:-} \
        --social.steam.app_id=${STEAM_APP_ID:-0} \
    "]
