# Self-hosting

The client and the backend are open source, and the SFU is source available. If you run them yourself, your data stays on your hardware.

## Backend

`backend/docker-compose.yml` starts PostgreSQL 15, Nakama 3.21.0 with the m3llo Go modules, and an S3-compatible store (RustFS) for clips, snapshots, chat media and diagnostics.

```bash
cd backend && docker compose up -d
```

The Nakama console is at `http://localhost:7351`. The login is in `backend/nakama/data/local.yml`.

## SFU

Voice and streams run through the SFU. Run [mello-sfu](https://github.com/mollohq/mello-sfu) (its `SELF-HOSTING.md` covers the SFU side), then point Nakama at it. `backend/docker-compose.yml` passes these variables to Nakama:

- `SFU_REGIONS`: your SFUs as `region=endpoint`, comma-separated. One SFU: `SFU_REGIONS=eu-west=wss://sfu.example.com:8443/ws`. Each region name must match the `SFU_REGION` of that SFU. With several regions, the client measures each one and joins the nearest.
- `SFU_JWT_PRIVATE_KEY`: the RS256 private key Nakama uses to sign short-lived SFU tokens. The SFU gets the public key.
- Optional: `SFU_DEFAULT_REGION` (else the first region) and `SFU_HEALTH_PORT` (else 8080, the SFU's `/health` port).

Nothing is built in. Without a key or a region, joining a voice channel and starting a stream fail with `SFU_UNAVAILABLE`, and the Nakama start log says why. `backend/.env.example` lists all SFU variables.

## Client
Build from source, see [`specs/05-GETTING-STARTED.md`](../specs/05-GETTING-STARTED.md). To point the client at your backend, set `NAKAMA_HOST`, `NAKAMA_PORT`, `NAKAMA_SSL`, `NAKAMA_SERVER_KEY` and `NAKAMA_HTTP_KEY`. The server key must match the one your Nakama uses. See `mello-core/src/config.rs`.

## Sign-in
A social sign-in provider turns on only when its client ID is set in the backend environment. The IDs are listed in `backend/.env.example`. Without them, sign-in is email and password.

## Before you expose it

The compose file is a development stack. Change these first:

- The development keys: `NAKAMA_HTTP_KEY` and the socket server key in `backend/nakama/data/local.yml`.
- The console login in `backend/nakama/data/local.yml`.
- The S3 credentials `minioadmin` / `minioadmin` in the compose file.
- The PostgreSQL password `localdev`.
- The published ports for PostgreSQL (5432) and the S3 store (9000, 9001).
