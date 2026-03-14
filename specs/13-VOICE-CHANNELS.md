# MELLO Voice Channels Specification

> **Component:** Voice Channels (Multi-Channel Support)  
> **Version:** 0.3  
> **Status:** Planned  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)  
> **Related:** [02-MELLO-CORE.md](./02-MELLO-CORE.md), [04-BACKEND.md](./04-BACKEND.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md)

---

## 1. Overview

Mello currently supports a single implicit voice session per crew. This spec adds support for multiple named voice channels within a crew, allowing members to split into separate conversations.

### Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Capacity cap | 6 per channel | Keeps P2P mesh small; crew can spread across channels |
| Default channel | "General" — always exists, renamable, not deletable | Every crew needs a guaranteed landing spot |
| Channel limit | No hard cap | Practical limit is UI space; no need to gate this |
| Channel CRUD permissions | Creator + configurable | Flexible without overcomplicating beta |
| Streaming | Crew-wide | Stream is visible from any channel; avoids confusion |
| Chat | Crew-wide | Single chat stream; doesn't split per channel |
| Channel ordering | Manual position field | Drag-to-reorder in future; explicit sort for now |

### What Changes

| Layer | Before | After |
|-------|--------|-------|
| Backend `VoiceRoom` | Keyed by `crew_id` | Keyed by `channel_id` |
| Backend storage | None | `voice_channels/{crew_id}` stores channel definitions |
| mello-core `VoiceManager` | Single implicit room | Joins a specific channel by ID |
| mello-core events | `VoiceConnected { crew_id }` | `VoiceConnected { crew_id, channel_id }` |
| Crew state (spec 11) | `voice: { active, members }` | `voice_channels: [{ id, name, members }]` |
| Client UI | Flat member bar | Accordion per channel (see mockup) |

### What Doesn't Change

- P2P mesh topology (full mesh per channel, each channel independent)
- Voice pipeline (libmello C API unchanged)
- Chat (stays crew-wide, single Nakama channel)
- Streaming (crew-wide, `PacketSink` model untouched)
- Stream viewing (not tied to a voice channel)

---

## 2. UI

### 2.1 Crew Members Area (Accordion)

The current flat "crew members bar" is replaced by a vertically stacked accordion of voice channels:

```
┌──────────────────────────────────────────────────────────────────┐
│                                                                  │
│  🎙 General                                      ACTIVE SESSION ●│
│  ACTIVE SESSION                                                  │
│                                                                  │
│  ┌──────┐  ┌──────┐  ┌──────┐  ┌ ─ ─ ─┐                        │
│  │  KT  │  │  AS  │  │  MR  │  │  +   │                        │
│  └──────┘  └──────┘  └──────┘  └ ─ ─ ─┘                        │
│  k0ji_tech  ash_22    m1ra     Invite                           │
│                                                                  │
├──────────────────────────────────────────────────────────────────┤
│  🎙 Strategy                        VX  NV           2 MEMBERS ▾│
├──────────────────────────────────────────────────────────────────┤
│  🎙 AFK                                             0 MEMBERS ▾│
└──────────────────────────────────────────────────────────────────┘
```

**Accordion behaviour:**

| State | Display |
|-------|---------|
| User's current channel | Expanded, shows full member cards with avatars, names, speaking indicators |
| Other channels with members | Collapsed, shows mini avatars inline + member count |
| Empty channels | Collapsed, shows "0 MEMBERS" |

Clicking a collapsed channel header **joins** that channel (leaves current, connects to new). There is no separate "join" button — the accordion header *is* the join action.

### 2.2 Channel Management

Right-clicking a channel header opens a context menu:

```
┌──────────────────────┐
│  Rename Channel      │
│  Delete Channel      │
│  ──────────────────  │
│  Create New Channel  │
└──────────────────────┘
```

"Delete Channel" is hidden for the default channel. Menu items are hidden if the user lacks permission (see §5).

"Create New Channel" is also accessible via a `+` button below the last channel in the accordion.

### 2.3 Joining Flow

When a user clicks a different channel:

1. Leave current voice mesh (tear down peer connections)
2. Call `voice_join(crew_id, channel_id)` RPC
3. Establish new P2P mesh with members in target channel
4. Accordion expands new channel, collapses old

If the target channel is full (6 members), the click does nothing and a toast appears: **"Channel is full (6/6)"**.

---

## 3. Data Model

### 3.1 Voice Channel Definition

Stored in Nakama storage: `voice_channels/{crew_id}`

```json
{
  "crew_id": "crew_xyz",
  "channels": [
    {
      "id": "ch_general",
      "name": "General",
      "position": 0,
      "is_default": true,
      "created_by": "user_abc",
      "created_at": "2026-03-08T10:00:00Z"
    },
    {
      "id": "ch_strategy",
      "name": "Strategy",
      "position": 1,
      "is_default": false,
      "created_by": "user_abc",
      "created_at": "2026-03-10T14:30:00Z"
    },
    {
      "id": "ch_afk",
      "name": "AFK",
      "position": 2,
      "is_default": false,
      "created_by": "user_def",
      "created_at": "2026-03-10T15:00:00Z"
    }
  ],
  "updated_at": "2026-03-10T15:00:00Z"
}
```

**Channel ID format:** `ch_` prefix + 8 random alphanumeric characters (e.g. `ch_a3f8b2c1`). The default channel uses `ch_general` as a well-known ID for simplicity.

### 3.2 Voice Room State (In-Memory)

The existing `VoiceRoom` struct changes from `map[crewID]` to `map[channelID]`:

```go
// voice_state.go — updated

type VoiceRoom struct {
    ChannelID string
    CrewID    string
    Members   map[string]*VoiceMemberState // keyed by user_id
}

// Global state
var (
    voiceRooms     map[string]*VoiceRoom   // channelID → room
    voiceUserChannel map[string]string      // userID → channelID (reverse lookup)
    voiceChannelCrew map[string]string      // channelID → crewID (reverse lookup)
)
```

### 3.3 Crew State Update (Spec 11 Amendment)

The `voice` field in `CrewState` changes from a single object to an array:

**Before:**
```json
{
  "voice": {
    "active": true,
    "member_ids": ["user_a", "user_b"],
    "members": [...]
  }
}
```

**After:**
```json
{
  "voice_channels": [
    {
      "id": "ch_general",
      "name": "General",
      "is_default": true,
      "members": [
        { "user_id": "user_a", "username": "k0ji_tech", "speaking": false, "muted": false },
        { "user_id": "user_b", "username": "ash_22", "speaking": false, "muted": false },
        { "user_id": "user_c", "username": "m1ra", "speaking": false, "muted": false }
      ]
    },
    {
      "id": "ch_strategy",
      "name": "Strategy",
      "is_default": false,
      "members": [
        { "user_id": "user_d", "username": "vex_r", "speaking": false, "muted": false },
        { "user_id": "user_e", "username": "nova", "speaking": true, "muted": false }
      ]
    },
    {
      "id": "ch_afk",
      "name": "AFK",
      "is_default": false,
      "members": []
    }
  ]
}
```

The `voice.active` boolean is removed — the UI derives "active" from whether any channel has members.

---

## 4. Backend RPCs

### 4.1 Channel Management RPCs

All channel management RPCs are registered in `main.go` via `initializer.RegisterRpc(...)`.

#### `channel_create`

```go
// Request
type ChannelCreateRequest struct {
    CrewID string `json:"crew_id"`
    Name   string `json:"name"`
}

// Response
type ChannelCreateResponse struct {
    Channel VoiceChannelDef `json:"channel"`
}

// Validation:
// - Caller must have channel_manage permission (see §5)
// - Name must be 1-32 characters, trimmed, no leading/trailing whitespace
// - Name must be unique within the crew (case-insensitive)
// - Channel ID generated server-side: "ch_" + 8 random alphanumeric
// - Position = max existing position + 1
```

#### `channel_rename`

```go
// Request
type ChannelRenameRequest struct {
    CrewID    string `json:"crew_id"`
    ChannelID string `json:"channel_id"`
    Name      string `json:"name"`
}

// Validation:
// - Caller must have channel_manage permission
// - Same name constraints as create
// - Default channel CAN be renamed
```

#### `channel_delete`

```go
// Request
type ChannelDeleteRequest struct {
    CrewID    string `json:"crew_id"`
    ChannelID string `json:"channel_id"`
}

// Validation:
// - Caller must have channel_manage permission
// - Cannot delete the default channel (is_default == true)
// - If channel has members: move them to default channel automatically
//   (server calls voice_leave + voice_join for each, sends notifications)
// - Remaining channels' positions are compacted (no gaps)
```

#### `channel_reorder`

```go
// Request
type ChannelReorderRequest struct {
    CrewID     string   `json:"crew_id"`
    ChannelIDs []string `json:"channel_ids"` // ordered list, all channel IDs must be present
}

// Validation:
// - Caller must have channel_manage permission
// - channel_ids must contain exactly all existing channel IDs (no adds, no removes)
// - Server writes new position values based on array index
```

### 4.2 Voice RPCs (Updated)

#### `voice_join` (Updated)

```go
// Before
type VoiceJoinRequest struct {
    CrewID string `json:"crew_id"`
}

// After
type VoiceJoinRequest struct {
    CrewID    string `json:"crew_id"`
    ChannelID string `json:"channel_id"`
}

// Behaviour:
// 1. If user is already in a channel in this crew → leave it first (implicit move)
// 2. If user is in a channel in a DIFFERENT crew → leave that too (one channel at a time globally)
// 3. Check channel capacity (max 6) → reject with CHANNEL_FULL if at limit
// 4. Add user to VoiceRoom for channel_id
// 5. Update user presence: activity = { type: "in_voice", crew_id, channel_id }
// 6. Update crew state cache
// 7. Push voice_update to all crew subscribers
```

#### `voice_leave` (Updated)

```go
// Before
type VoiceLeaveRequest struct {
    CrewID string `json:"crew_id"`
}

// After — no change needed (server looks up current channel from voiceUserChannel map)
type VoiceLeaveRequest struct {
    CrewID string `json:"crew_id"`
}
```

### 4.3 Crew Creation Hook (Updated)

When a crew is created (`create_crew` RPC), the server now also initialises the default voice channel:

```go
func afterCreateCrew(crewID, creatorID string) {
    channels := VoiceChannelList{
        CrewID: crewID,
        Channels: []VoiceChannelDef{
            {
                ID:        "ch_general",
                Name:      "General",
                Position:  0,
                IsDefault: true,
                CreatedBy: creatorID,
                CreatedAt: time.Now().UTC(),
            },
        },
    }
    // Write to Nakama storage: voice_channels/{crew_id}
    nk.StorageWrite(ctx, []*runtime.StorageWrite{{
        Collection: "voice_channels",
        Key:        crewID,
        UserID:     "", // system-owned
        Value:      marshal(channels),
        PermissionRead: 2,  // public read
        PermissionWrite: 0, // server-only write
    }})
}
```

### 4.4 Push Notifications (Spec 11 Amendment)

Voice updates now include channel context:

```json
// voice_update notification — sent to all crew subscribers
{
  "type": "voice_update",
  "crew_id": "crew_xyz",
  "channel_id": "ch_general",
  "channel_name": "General",
  "members": [
    { "user_id": "user_a", "username": "k0ji_tech", "speaking": false, "muted": false }
  ]
}
```

Channel CRUD events are pushed as priority crew events:

```json
// channel_created
{
  "type": "crew_event",
  "crew_id": "crew_xyz",
  "event": "channel_created",
  "data": {
    "channel": { "id": "ch_strategy", "name": "Strategy", "position": 1, "is_default": false }
  }
}

// channel_renamed
{
  "type": "crew_event",
  "crew_id": "crew_xyz",
  "event": "channel_renamed",
  "data": { "channel_id": "ch_general", "name": "Lounge" }
}

// channel_deleted
{
  "type": "crew_event",
  "crew_id": "crew_xyz",
  "event": "channel_deleted",
  "data": { "channel_id": "ch_strategy", "moved_members_to": "ch_general" }
}
```

---

## 5. Permissions

### 5.1 Channel Permission

A single new permission controls all channel management (create, rename, delete, reorder):

```go
// crews.go — updated CrewMetadata

type CrewMetadata struct {
    MaxMembers     int    `json:"max_members"`
    InviteOnly     bool   `json:"invite_only"`
    CreatedBy      string `json:"created_by"`
    
    // New: who can manage voice channels
    // "creator" = only crew creator (default)
    // "admin"   = crew creator + anyone with admin role
    // "member"  = any crew member
    ChannelManage  string `json:"channel_manage"`
}
```

Default for new crews: `"creator"`.

### 5.2 Permission Check

```go
func canManageChannels(ctx context.Context, nk runtime.NakamaModule, userID, crewID string) bool {
    // 1. Load crew metadata
    meta := loadCrewMetadata(crewID)
    
    switch meta.ChannelManage {
    case "member":
        return true
    case "admin":
        // Check if user is crew creator or has admin state in group
        return isCrewCreator(meta, userID) || isGroupAdmin(nk, crewID, userID)
    default: // "creator"
        return isCrewCreator(meta, userID)
    }
}
```

### 5.3 Settings UI

The crew settings screen (accessible to crew creator) gains a new option:

```
Voice Channel Management
  ○ Only me (creator)
  ○ Admins
  ○ Everyone
```

This maps to the `channel_manage` metadata field. Exposed via an `update_crew_settings` RPC.

---

## 6. mello-core Changes

### 6.1 New Types

```rust
// mello-core/src/voice/channel.rs (new file)

pub type ChannelId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceChannel {
    pub id: ChannelId,
    pub name: String,
    pub position: u32,
    pub is_default: bool,
    pub created_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceChannelState {
    pub id: ChannelId,
    pub name: String,
    pub is_default: bool,
    pub members: Vec<VoiceMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceMember {
    pub user_id: String,
    pub username: String,
    pub speaking: bool,
    pub muted: bool,
    pub deafened: bool,
}
```

### 6.2 Updated Events

```rust
// mello-core/src/events.rs — updated variants

Event::VoiceConnected { crew_id: CrewId, channel_id: ChannelId },
Event::VoiceDisconnected { crew_id: CrewId, channel_id: ChannelId },
Event::VoiceActivity { channel_id: ChannelId, member_id: MemberId, speaking: bool },
Event::VoiceMoved { crew_id: CrewId, from_channel: ChannelId, to_channel: ChannelId },

// Channel CRUD events
Event::ChannelCreated { crew_id: CrewId, channel: VoiceChannel },
Event::ChannelRenamed { crew_id: CrewId, channel_id: ChannelId, name: String },
Event::ChannelDeleted { crew_id: CrewId, channel_id: ChannelId },
Event::ChannelReordered { crew_id: CrewId, channel_ids: Vec<ChannelId> },

// Updated crew state
Event::CrewStateLoaded { state: CrewState },  // CrewState now contains voice_channels
```

### 6.3 Updated Commands

```rust
// mello-core/src/command.rs — new variants

Command::VoiceJoin { crew_id: CrewId, channel_id: ChannelId },
Command::VoiceLeave { crew_id: CrewId },  // unchanged — server resolves channel

Command::ChannelCreate { crew_id: CrewId, name: String },
Command::ChannelRename { crew_id: CrewId, channel_id: ChannelId, name: String },
Command::ChannelDelete { crew_id: CrewId, channel_id: ChannelId },
Command::ChannelReorder { crew_id: CrewId, channel_ids: Vec<ChannelId> },
```

### 6.4 VoiceManager Update

The `VoiceManager` in `mello-core/src/voice/manager.rs` currently tracks a single `connected_crew: Option<CrewId>`. This becomes:

```rust
pub struct VoiceManager {
    libmello: *mut MelloContext,
    event_tx: mpsc::Sender<Event>,

    // Updated: track both crew and channel
    connected_crew: Option<CrewId>,
    connected_channel: Option<ChannelId>,

    // Peers — unchanged, still keyed by member user_id
    peers: HashMap<MemberId, PeerHandle>,
}

impl VoiceManager {
    pub async fn join_channel(
        &mut self,
        nakama: &NakamaClient,
        crew_id: &str,
        channel_id: &str,
    ) -> Result<(), Error> {
        // If already in a channel, leave first
        if self.connected_channel.is_some() {
            self.leave_current(nakama).await?;
        }

        // RPC: voice_join with channel_id
        let members = nakama.rpc("voice_join", &VoiceJoinRequest {
            crew_id: crew_id.to_string(),
            channel_id: channel_id.to_string(),
        }).await?;

        self.connected_crew = Some(crew_id.to_string());
        self.connected_channel = Some(channel_id.to_string());

        // Establish P2P connections with channel members (same mesh logic as before)
        for member in &members {
            self.connect_peer(nakama, &member.user_id).await?;
        }

        self.event_tx.send(Event::VoiceConnected {
            crew_id: crew_id.to_string(),
            channel_id: channel_id.to_string(),
        }).await.ok();

        Ok(())
    }
}
```

### 6.5 Updated Client State

```rust
// client/src/app.rs — updated CrewState fields for Slint

pub struct CrewViewState {
    // ... existing fields ...

    // Updated: array of channel states instead of single voice state
    voice_channels: Vec<VoiceChannelState>,
    current_channel_id: Option<ChannelId>,
}
```

### 6.6 Channel Load on Crew Select

When a user selects a crew, `set_active_crew` RPC already returns `CrewState`. The response now includes `voice_channels` (the full list of channel definitions + current members). No additional RPC needed.

---

## 7. User Presence Update (Spec 11 Amendment)

The user presence activity object gains a `channel_id` field:

```json
{
  "user_id": "user_abc",
  "status": "online",
  "activity": {
    "type": "in_voice",
    "crew_id": "crew_xyz",
    "channel_id": "ch_general",
    "channel_name": "General"
  }
}
```

This allows the sidebar to show "user_abc is in General @ Neon Syndicate" without needing to load full crew state.

---

## 8. Error Codes

| Code | Name | Description |
|------|------|-------------|
| `CHANNEL_FULL` | Channel is full | 6/6 members in target channel |
| `CHANNEL_NOT_FOUND` | Channel not found | Invalid channel_id for this crew |
| `CANNOT_DELETE_DEFAULT` | Cannot delete default channel | Attempted to delete is_default channel |
| `CHANNEL_NAME_TAKEN` | Name already exists | Duplicate channel name within crew |
| `CHANNEL_NAME_INVALID` | Invalid channel name | Empty, too long (>32 chars), or invalid characters |
| `NO_CHANNEL_PERMISSION` | Permission denied | User lacks channel_manage permission |

---

## 9. Testing Checklist

### Channel CRUD
- [ ] New crew has "General" default channel
- [ ] Create channel → appears in all members' accordions
- [ ] Rename channel → reflected for all members
- [ ] Rename default channel → works
- [ ] Delete default channel → rejected
- [ ] Delete channel with members → members moved to default, notified
- [ ] Reorder channels → order persists and syncs to all members
- [ ] Duplicate channel name → rejected
- [ ] Channel name validation (empty, >32 chars) → rejected

### Joining / Leaving
- [ ] Click collapsed channel → joins that channel, leaves current
- [ ] Join full channel (6/6) → rejected with toast
- [ ] Leave crew → voice leave is triggered automatically
- [ ] Disconnect → server cleans up voice state for that channel

### Permissions
- [ ] Creator can always manage channels
- [ ] Setting "admin" allows group admins to manage
- [ ] Setting "member" allows everyone to manage
- [ ] Non-permitted users don't see management UI options

### P2P Mesh
- [ ] Each channel has independent peer connections
- [ ] Moving between channels tears down old connections, builds new
- [ ] Speaking indicator works per-channel
- [ ] User in channel A does not hear users in channel B

### Crew State / Sidebar
- [ ] Active crew shows all channels with members
- [ ] Sidebar shows summary voice info across channels
- [ ] Channel CRUD events pushed immediately to all subscribers
- [ ] Voice join/leave updates pushed with channel_id context

### Streaming
- [ ] Stream is visible regardless of which channel the viewer is in
- [ ] Stream notification is crew-wide, not channel-scoped
- [ ] Streamer can be in any channel while streaming

---

## 10. Files to Create / Modify

### New Files

```
backend/nakama/data/modules/
  voice_channels.go              # Channel CRUD RPCs, storage helpers

mello-core/src/voice/
  channel.rs                     # VoiceChannel, VoiceChannelState types

client/ui/panels/
  voice_channels.slint           # Accordion component
```

### Modified Files

```
backend/nakama/data/modules/
  main.go                        # Register new RPCs
  voice_state.go                 # VoiceRoom keyed by channel_id, updated join/leave
  crew_state.go                  # voice_channels in CrewState aggregate
  push.go                        # Channel context in voice push notifications
  crews.go                       # channel_manage permission in CrewMetadata, default channel on create

mello-core/src/
  voice/mod.rs                   # Re-export channel types
  voice/manager.rs               # connected_channel, join_channel(), leave_channel()
  events.rs                      # New event variants
  command.rs                     # New command variants
  client.rs                      # Updated handle_select_crew, handle_voice_join

client/src/
  app.rs                         # voice_channels state, current_channel_id

client/ui/panels/
  stream_view.slint              # Replace flat member bar with voice_channels accordion
```

---

*This spec covers voice channels. For the streaming pipeline, see [12-STREAMING.md](./12-STREAMING.md).*
