# Mello

> Hang out with your crew. Jump into anyone's stream.

A lightweight crew-based social platform with Parsec-tier streaming. Discord-killer vibes, <25MB install.

## Quick Start

```bash
# Start backend
cd backend && docker compose up -d

# Run client
cd client && cargo run
```

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  Slint UI (Rust) → mello-core (Rust) → libmello (C++)  │
└─────────────────────────────────────────────────────┘
                          ↓
              Nakama (Auth, Chat, Signaling)
                          ↓
                    P2P (Voice, Stream)
```

## License

Apache 2.0
