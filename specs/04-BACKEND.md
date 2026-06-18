# MELLO Backend Specification

> **Component:** Backend Infrastructure (Nakama)  
> **Platform:** Heroic Labs Nakama  
> **Status:** Beta Scope  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

Mello's backend is built on Nakama, an open-source game server. Nakama handles authentication, presence, groups (crews), real-time chat, and P2P signaling. This keeps backend complexity minimal while providing battle-tested infrastructure.

**Key Responsibilities:**
- User authentication (device, email, Discord/Steam/Google/Apple/Twitch OAuth)
- Presence tracking (online/idle/offline)
- Groups (Crews) with membership management
- Real-time chat (persistent, per-crew)
- P2P signaling (ICE candidate exchange)
- TURN relay configuration
- Crew discovery, avatars, invite codes, user search

---

## 2. Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           BACKEND STACK                                 │
│                                                                         │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                         CLIENTS                                   │  │
│  │                                                                   │  │
│  │    Windows │ macOS │ Linux │ iOS │ Android                        │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                │                                        │
│                     WebSocket (WSS) + REST (HTTPS)                      │
│                                │                                        │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                      LOAD BALANCER                                │  │
│  │                     (nginx / Cloudflare)                          │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                │                                        │
│         ┌──────────────────────┼──────────────────────┐                 │
│         │                      │                      │                 │
│         ▼                      ▼                      ▼                 │
│  ┌─────────────┐       ┌─────────────┐       ┌─────────────┐           │
│  │   Nakama    │       │   Nakama    │       │   Nakama    │           │
│  │  Instance 1 │◀─────▶│  Instance 2 │◀─────▶│  Instance 3 │           │
│  └─────────────┘       └─────────────┘       └─────────────┘           │
│         │                      │                      │                 │
│         └──────────────────────┼──────────────────────┘                 │
│                                │                                        │
│                                ▼                                        │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                     COCKROACHDB CLUSTER                           │  │
│  │                    (or PostgreSQL for dev)                        │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│                         P2P INFRASTRUCTURE                              │
│                                                                         │
│  ┌─────────────────────────┐       ┌─────────────────────────────────┐  │
│  │      STUN SERVERS       │       │         TURN SERVERS            │  │
│  │                         │       │                                 │  │
│  │  stun.l.google.com:19302│       │  turn.mello.app:3478            │  │
│  │  stun1.l.google.com:... │       │  (coturn, ~10% of connections)  │  │
│  └─────────────────────────┘       └─────────────────────────────────┘  │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Go Runtime Modules

All custom server logic is written in Go and loaded as Nakama runtime modules. Modules are stateless — state lives in Nakama storage or PostgreSQL.

| Module | Responsibilities |
|--------|-----------------|
| `main.go` | RPC and hook registration |
| `auth.go` | Post-authentication hooks (create default metadata) |
| `crews.go` | `CreateCrewRPC`, `DiscoverCrewsRPC`, `UpdateCrewRPC`, `DeleteCrewRPC`, `ChangeCrewRoleRPC`, `KickCrewMemberRPC`, `GetCrewAvatarRPC`, after-join/leave hooks |
| `streaming.go` | `StartStreamRPC`, `StopStreamRPC`, `UploadThumbnailRPC` |
| `search_users.go` | `SearchUsersRPC` — friends first, then other matches by display name |
| `invite_codes.go` | `GenerateInviteCode`, `JoinByInviteCodeRPC` |
| `signaling.go` | `GetIceServersRPC`, TURN credential generation (HMAC-SHA1, time-limited) |
| `voice_channels.go` | Voice channel CRUD via RPCs |
| `voice_state.go` | Voice state tracking (in-memory `voiceRooms`), join/leave/speaking RPCs, periodic GC via `StartVoiceRoomGC` (30s, prunes members whose presence is offline) |
| `crew_state.go` | Crew state streaming (sidebar, presence) |
| `presence.go` | Presence hooks (status tracking) |
| `push.go` | Push notification helpers |
| `clips.go` | `PostClipRPC`, `CrewTimelineRPC`, `CrewClipsRPC`, `CrewRecapsRPC`, `ClipUploadURLRPC`, `ClipUploadCompleteRPC`, `WeeklyRecapJob`; durable `crew_clips`/`crew_recaps` storage |
| `crew_feed.go` | `CrewFeedRPC` — server-side feed curation (order, role, size, locked card) |
| `stream_sessions_store.go` | Durable `crew_stream_sessions` store (`UpsertStreamSession`, cap) so stream replays outlive the ledger trim |
| `s3.go` | S3/R2 presign client singleton, `GeneratePresignedPUT`, `S3PublicURL` helpers |
| `dev_seed.go` | Development seed data |

---

## 4. Custom RPCs

| RPC name | Auth | Description |
|----------|------|-------------|
| `create_crew` | Yes | Creates a Nakama group, stores avatar in storage, generates invite code, sends Nakama notifications to invited users. Returns crew ID + invite code. |
| `discover_crews` | No (`http_key`) | Paginated list of public (open) crews. Accepts `cursor`, returns `crews` + `nextCursor`. Used during onboarding without auth. |
| `get_crew_avatar` | No (`http_key`) | Reads crew avatar base64 from storage by crew ID. Returns `{"data":"<base64>"}`. Works without auth for onboarding. |
| `search_users` | Yes | Searches users by display name prefix. Returns friends first (via `nk.FriendsList`), then non-friends matching the query (via SQL `LIKE` on `users` table). Limit 100 friends, 20 results. |
| `join_by_invite_code` | Yes | Looks up invite code in storage, joins the associated crew. Returns crew ID + name. |
| `update_crew` | Yes | Updates crew name, description, avatar, open/closed status, and invite policy. Requires admin role (state ≤ 1). |
| `delete_crew` | Yes | Deletes a crew. Requires owner role (state 0). |
| `change_crew_role` | Yes | Promotes/demotes a member between admin (1) and member (2). Requires owner role (state 0). |
| `kick_crew_member` | Yes | Removes a member from the crew. Admins can kick members; owner can kick anyone except self. |
| `get_ice_servers` | Yes | Returns STUN server URLs + TURN server URLs with time-limited HMAC credentials (24h TTL). |
| `start_stream` | Yes | Announces stream start to crew members via crew state stream. |
| `stop_stream` | Yes | Announces stream end. |
| `upload_thumbnail` | Yes | Stores stream thumbnail (base64) in Nakama storage. |
| `post_clip` | Yes | Stores clip metadata in the durable per-crew `crew_clips` document (capped, outside the 7-day ledger trim). Pushes notification to crew. |
| `crew_feed` | Yes | Server-curated primary feed: merges ledger + durable clips/recaps, returns `this_week` + `memory` sections with per-entry `role`/`size` and a server-emitted `locked` card for non-premium users. |
| `crew_timeline` | Yes | Raw paginated merge (ledger + recent clips + latest recap) for deep scroll. Cursor-based. |
| `crew_clips` | Yes | Paginated access to durable clips. |
| `crew_recaps` | Yes | Paginated access to durable recaps. |
| `clip_upload_url` | Yes | Returns a presigned PUT URL for direct S3/R2 upload and the public `media_url`. |
| `clip_upload_complete` | Yes | Updates the clip's `media_url` after successful upload. |

Every RPC validates its input and returns typed Nakama errors (`UNAUTHENTICATED`, `INVALID_ARGUMENT`, `NOT_FOUND`, `PERMISSION_DENIED`, `INTERNAL`).

---

## 5. Data Models

### User (Nakama built-in + metadata)

| Field | Source | Description |
|-------|--------|-------------|
| `id` | Nakama | UUID |
| `username` | Nakama | Unique handle |
| `display_name` | Nakama | Shown in UI |
| `avatar_url` | Nakama | User avatar URL |
| `metadata` | Nakama JSON | `{ tag, created_at }` |

### Crew (Nakama Group + metadata)

| Field | Source | Description |
|-------|--------|-------------|
| `id` | Nakama group | UUID |
| `name` | Nakama group | Display name |
| `description` | Nakama group | Crew description |
| `open` | Nakama group | Joinable without invite |
| `max_count` | Nakama group | Max members (default: 6) |
| `metadata` | Nakama JSON | `{ max_members, invite_only, created_by, stream_enabled, invite_policy }` |

### Presence Status (via Nakama presence)

```json
{ "status": "online|idle|dnd|offline", "streaming_to": "<crew_id>", "watching_id": "<host_user_id>" }
```

### Signaling Messages (via Nakama channel messages)

```json
{ "type": "offer|answer|ice", "from": "<user_id>", "to": "<user_id>", "session_id": "...", "sdp": "...", "candidate": "...", "sdp_mid": "...", "sdp_mline_index": 0 }
```

---

## 6. Storage Patterns

Nakama's key-value storage is used for data that doesn't fit built-in models:

| Collection | Key | Owner | Value | Used by |
|-----------|-----|-------|-------|---------|
| `crew_avatars` | `{crew_id}` | System user (`""`) | `{"data":"<base64 JPEG>"}` | `create_crew`, `get_crew_avatar` |
| `invite_codes` | `{code}` | System user (`""`) | `{"crew_id":"...","crew_name":"...","created_by":"..."}` | `GenerateInviteCode`, `join_by_invite_code` |
| `stream_thumbnails` | `{crew_id}` | Streaming user | `{"data":"<base64 JPEG>"}` | `upload_thumbnail` |
| `crew_events` | `{crew_id}` | System user (`""`) | 7-day rolling JSON event ledger (sessions, joins, chat, moments) | `crew_timeline`, `crew_feed`, `post_moment` |
| `crew_clips` | `{crew_id}` | System user (`""`) | Durable capped list of clip metadata (outside ledger trim) | `post_clip`, `crew_clips`, `crew_feed`, `clip_upload_complete` |
| `crew_recaps` | `{crew_id}` | System user (`""`) | Durable list of weekly recaps | `WeeklyRecapJob`, `crew_recaps`, `crew_feed` |
| `crew_stream_sessions` | `{crew_id}` | System user (`""`) | Durable capped list of stream replays (outside ledger trim) | `stop_stream`, snapshot backfill, `crew_feed` |

**Cloud object storage (S3/R2):** Clip media files (MP4/AAC) are stored in an S3-compatible bucket (`mello-clips`). Clients upload directly via presigned PUT URLs — no data passes through Nakama. See [CLIPS.md §6](./features/CLIPS.md) for the full flow.

Storage writes use `PermissionRead: 2` (public read) and `PermissionWrite: 0` (server-only write) for system-owned data. The owner for crew avatars and invite codes is the empty string (system user) so that any client can read them.

---

## 7. Client-Server Communication

### Authentication Flow (Discord OAuth example)

```
┌────────┐                    ┌────────┐                    ┌────────┐
│ Client │                    │ Nakama │                    │Discord │
└───┬────┘                    └───┬────┘                    └───┬────┘
    │                             │                             │
    │ 1. Open Discord OAuth URL   │                             │
    │────────────────────────────▶│                             │
    │                             │                             │
    │                             │ 2. Redirect to Discord      │
    │◀────────────────────────────│────────────────────────────▶│
    │                             │                             │
    │ 3. User authorizes          │                             │
    │──────────────────────────────────────────────────────────▶│
    │                             │                             │
    │ 4. Callback with code       │                             │
    │◀──────────────────────────────────────────────────────────│
    │                             │                             │
    │ 5. AuthenticateCustom       │                             │
    │   (provider: "discord",     │                             │
    │    token: code)             │                             │
    │────────────────────────────▶│                             │
    │                             │                             │
    │                             │ 6. Exchange code for token  │
    │                             │────────────────────────────▶│
    │                             │                             │
    │                             │ 7. Get user info            │
    │                             │◀────────────────────────────│
    │                             │                             │
    │ 8. Session token            │                             │
    │◀────────────────────────────│                             │
    │                             │                             │
```

### Real-Time Communication

```
┌────────┐                              ┌────────┐
│ Client │                              │ Nakama │
└───┬────┘                              └───┬────┘
    │                                       │
    │ 1. Connect WebSocket                  │
    │──────────────────────────────────────▶│
    │                                       │
    │ 2. Join crew channel                  │
    │   channel_join("crew.{crew_id}")      │
    │──────────────────────────────────────▶│
    │                                       │
    │ 3. Receive presence updates           │
    │◀──────────────────────────────────────│
    │                                       │
    │ 4. Send chat message                  │
    │   channel_message_send(...)           │
    │──────────────────────────────────────▶│
    │                                       │
    │ 5. Receive chat messages (broadcast)  │
    │◀──────────────────────────────────────│
    │                                       │
    │ 6. Send signaling message             │
    │   channel_message_send(signal)        │
    │──────────────────────────────────────▶│
    │                                       │
    │ 7. Receive signaling (to specific)    │
    │◀──────────────────────────────────────│
    │                                       │
```

### P2P Signaling Flow

```
┌─────────┐              ┌────────┐              ┌─────────┐
│  Peer A │              │ Nakama │              │  Peer B │
└────┬────┘              └───┬────┘              └────┬────┘
     │                       │                       │
     │ 1. Create offer       │                       │
     │ (local SDP)           │                       │
     │                       │                       │
     │ 2. Send offer via     │                       │
     │    channel message    │                       │
     │──────────────────────▶│                       │
     │                       │                       │
     │                       │ 3. Forward to Peer B  │
     │                       │──────────────────────▶│
     │                       │                       │
     │                       │                       │ 4. Create answer
     │                       │                       │    (local SDP)
     │                       │                       │
     │                       │ 5. Send answer        │
     │                       │◀──────────────────────│
     │                       │                       │
     │ 6. Forward to Peer A  │                       │
     │◀──────────────────────│                       │
     │                       │                       │
     │ 7. Exchange ICE candidates (both directions) │
     │◀─────────────────────▶│◀─────────────────────▶│
     │                       │                       │
     │═══════════════════════════════════════════════│
     │              P2P Connection Established       │
     │═══════════════════════════════════════════════│
     │                       │                       │
```

---

## 8. Invite Code System

Invite codes are 8-character alphanumeric strings (format: `XXXX-XXXX`), generated server-side when a crew is created. They're stored in Nakama storage with the system user as owner.

**Flow:**
1. Creator creates crew → `create_crew` RPC generates code → returns code to client
2. Creator shares code (copy-to-clipboard in the new-crew modal)
3. Recipient enters code → `join_by_invite_code` RPC looks up code → joins the crew
4. Invited users (selected during crew creation) receive Nakama notifications with crew details

---

## 9. Scaling Considerations

### Beta (Up to 10,000 users)

```
1x Nakama (4 vCPU, 8GB RAM)
1x PostgreSQL (2 vCPU, 4GB RAM)
1x TURN (2 vCPU, 4GB RAM, 1Gbps)

Estimated cost: ~$150-300/mo
```

### Growth (100,000+ users)

```
┌─────────────────────┐    ┌─────────────────────┐
│     US-EAST         │    │     EU-WEST         │
│  3x Nakama (HA)     │    │  3x Nakama (HA)     │
│  CockroachDB node   │◀──▶│  CockroachDB node   │
│  2x TURN            │    │  2x TURN            │
└─────────────────────┘    └─────────────────────┘

Estimated cost: ~$2,000-5,000/mo
```

---

## 10. Security

| Aspect | Implementation |
|--------|----------------|
| Transport | WSS (TLS 1.3) |
| Authentication | Nakama JWT tokens |
| Session | 24h expiry, refresh tokens (7-day) |
| TURN credentials | Time-limited HMAC-SHA1 (24h TTL) |
| Rate limiting | Nakama built-in + nginx |
| DDoS protection | Cloudflare |
| Storage permissions | System-owned data: public read, server-only write |

---

## 11. Monitoring

| Metric | Source | Alert Threshold |
|--------|--------|-----------------|
| WebSocket connections | Nakama | >90% capacity |
| Message throughput | Nakama | >10k/s per instance |
| P2P success rate | Application | <85% |
| TURN relay usage | Coturn | >30% of connections |
| TURN bandwidth | Coturn | >80% capacity |
| Database connections | PostgreSQL | >80% pool |
| API latency (p99) | Nakama | >500ms |

---

## 12. API Reference

### REST Endpoints (via Nakama)

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| POST | `/v2/account/authenticate/device` | No | Device auth (onboarding) |
| POST | `/v2/account/authenticate/email` | No | Email login |
| POST | `/v2/account/authenticate/custom` | No | OAuth (Discord, Steam, etc.) |
| GET | `/v2/account` | Yes | Get current user |
| GET | `/v2/user/group` | Yes | List user's crews |
| POST | `/v2/group/{id}/join` | Yes | Join crew |
| POST | `/v2/group/{id}/leave` | Yes | Leave crew |
| POST | `/v2/rpc/create_crew` | Yes | Create crew + avatar + invite code |
| POST | `/v2/rpc/update_crew` | Yes | Update crew name, description, avatar, privacy, invite policy |
| POST | `/v2/rpc/delete_crew` | Yes | Delete crew (owner only) |
| POST | `/v2/rpc/change_crew_role` | Yes | Promote/demote crew member (owner only) |
| POST | `/v2/rpc/kick_crew_member` | Yes | Kick member from crew |
| POST | `/v2/rpc/discover_crews` | No (`http_key`) | Paginated public crew list |
| POST | `/v2/rpc/get_crew_avatar` | No (`http_key`) | Fetch crew avatar base64 |
| POST | `/v2/rpc/search_users` | Yes | Search users by display name |
| POST | `/v2/rpc/join_by_invite_code` | Yes | Join crew via invite code |
| POST | `/v2/rpc/get_ice_servers` | Yes | Get STUN/TURN credentials |
| POST | `/v2/rpc/start_stream` | Yes | Announce stream start |
| POST | `/v2/rpc/stop_stream` | Yes | Announce stream end |
| POST | `/v2/rpc/upload_thumbnail` | Yes | Upload stream thumbnail |
| POST | `/v2/rpc/post_clip` | Yes | Store clip metadata in durable `crew_clips` |
| POST | `/v2/rpc/crew_feed` | Yes | Server-curated crew feed (`this_week` + `memory`) |
| POST | `/v2/rpc/crew_timeline` | Yes | Raw paginated merge for deep scroll |
| POST | `/v2/rpc/clip_upload_url` | Yes | Get presigned PUT URL for S3/R2 upload |
| POST | `/v2/rpc/clip_upload_complete` | Yes | Confirm upload, set media_url |

### WebSocket Messages

| Type | Direction | Description |
|------|-----------|-------------|
| `channel_join` | Client→Server | Join crew channel |
| `channel_leave` | Client→Server | Leave crew channel |
| `channel_message_send` | Client→Server | Send chat/signal message |
| `channel_message` | Server→Client | Receive chat/signal message |
| `presence_event` | Server→Client | Member join/leave notification |
| `status_update` | Client→Server | Update presence status |
| `notifications` | Server→Client | Invite notifications, system alerts |

---

## Voice State Correctness (v0.3)

Backend changes that make the Nakama-authoritative voice roster correct and durable (see authority model in [02-MELLO-CORE.md](./02-MELLO-CORE.md), client behaviour in [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md)).

- **Idempotent same-channel `voice_join`.** Re-joining the channel a user is already in is a no-op for membership: it skips the `voiceLeaveInternal` remove-then-add and the `voice_left`/`voice_joined` churn, preserves `JoinedAt`, and just re-issues the SFU token / returns the snapshot. Prevents roster flicker on reconnect.
- **Atomic join + capacity check.** The three voice maps (`voiceRooms` / `voiceUserChannel` / `voiceChannelCrew`) are mutated in a single critical section, closing the capacity TOCTOU.
- **Voice cleanup on kick / leave-crew.** Kick and `AfterLeaveCrew` paths now evict the user from any voice room (previously left ghosts).
- **Sequenced + coalesced `voice_update`.** Pushes carry a per-crew monotonic `seq` and are debounced to avoid VAD push storms.
- **Tighter GC with grace + consecutive detections.** `voiceRoomGC` now combines: offline grace (`30s`), stale-online threshold (`5m`), and `2` consecutive stale detections before pruning. This reduces false-prune churn during transient reconnects while still removing ghosts.
- **SFU reconcile oracle with miss hysteresis.** For SFU crews, Nakama pulls the SFU admin session API (`/admin/api/session/{id}`, `SFU_ADMIN_PASSWORD`) to correct membership drift, then re-pushes a sequenced update. Pruning requires `2` consecutive SFU misses and respects a `45s` post-join grace; unknown SFU session lookups do not prune (treat as P2P/transient).
- **`dev_fault` RPC (dev/test only).** Alongside `dev_seed_state`, injects drift for testing: force a ghost voice member, force `voice_leave`, drop the next push. Used by the reconcile/resync tests.
- **Diagnostic upload URL RPC.** Issues a short-lived presigned PUT URL so production clients can upload a captured diagnostic log bundle (see [15-DEBUG-TELEMETRY.md](./15-DEBUG-TELEMETRY.md)).

---

*This spec defines the backend. For development setup, see [05-GETTING-STARTED.md](./05-GETTING-STARTED.md).*
