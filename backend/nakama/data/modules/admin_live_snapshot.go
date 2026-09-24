package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"sort"
	"sync"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// liveSnapshotTTL bounds how often the (moderately expensive) snapshot
// rebuilds. The admin Live view polls, so a short cache keeps repeated
// polls cheap without visible staleness.
const liveSnapshotTTL = 15 * time.Second

// LiveVoiceRoom is one non-empty voice room.
type LiveVoiceRoom struct {
	ChannelID   string              `json:"channel_id"`
	CrewID      string              `json:"crew_id"`
	CrewName    string              `json:"crew_name"`
	ChannelName string              `json:"channel_name"`
	Members     []*VoiceMemberState `json:"members"`
}

// LiveStream is one active stream record.
type LiveStream struct {
	StreamerID       string `json:"streamer_id"`
	StreamerUsername string `json:"streamer_username"`
	StreamID         string `json:"stream_id"`
	CrewID           string `json:"crew_id"`
	CrewName         string `json:"crew_name"`
	Title            string `json:"title"`
	StartedAt        string `json:"started_at"`
	ThumbnailURL     string `json:"thumbnail_url,omitempty"`
	ViewerCount      int    `json:"viewer_count"`
}

// LiveSnapshot is the admin_live_snapshot response contract.
type LiveSnapshot struct {
	VoiceRooms []LiveVoiceRoom `json:"voice_rooms"`
	Streams    []LiveStream    `json:"streams"`
}

var (
	liveSnapshotMu   sync.Mutex
	liveSnapshotJSON string
	liveSnapshotAt   time.Time
)

// AdminLiveSnapshotRPC returns live voice rooms and active streams for the
// internal admin dashboard. It is server-to-server only (called with
// http_key, never a client session).
func AdminLiveSnapshotRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	// Reject client-session calls; only http_key (no user in context) is allowed.
	if uid, _ := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string); uid != "" {
		return "", runtime.NewError("admin_live_snapshot is server-to-server only", 7) // PERMISSION_DENIED
	}

	liveSnapshotMu.Lock()
	defer liveSnapshotMu.Unlock()

	if liveSnapshotJSON != "" && time.Since(liveSnapshotAt) < liveSnapshotTTL {
		return liveSnapshotJSON, nil
	}

	snap, err := collectLiveSnapshot(ctx, nk)
	if err != nil {
		logger.Error("admin_live_snapshot: collect failed: %v", err)
		// Serve stale cache rather than blanking the panel if we ever have one.
		if liveSnapshotJSON != "" {
			return liveSnapshotJSON, nil
		}
		return "", runtime.NewError("failed to collect live snapshot", 13) // INTERNAL
	}

	out, err := json.Marshal(snap)
	if err != nil {
		return "", runtime.NewError("failed to encode live snapshot", 13)
	}

	liveSnapshotJSON = string(out)
	liveSnapshotAt = time.Now()
	return liveSnapshotJSON, nil
}

func collectLiveSnapshot(ctx context.Context, nk runtime.NakamaModule) (*LiveSnapshot, error) {
	snap := &LiveSnapshot{VoiceRooms: []LiveVoiceRoom{}, Streams: []LiveStream{}}

	rooms := snapshotActiveVoiceRooms()
	applyChannelNames(ctx, nk, rooms)
	snap.VoiceRooms = rooms

	streams, err := collectLiveStreams(ctx, nk)
	if err != nil {
		return nil, err
	}
	snap.Streams = streams

	applyCrewNames(ctx, nk, snap)

	return snap, nil
}

// snapshotActiveVoiceRooms copies every non-empty room out of the in-memory
// map. Empty rooms never appear. Pure over package state so tests can seed
// voiceRooms directly.
func snapshotActiveVoiceRooms() []LiveVoiceRoom {
	voiceRoomsMu.RLock()
	defer voiceRoomsMu.RUnlock()

	rooms := make([]LiveVoiceRoom, 0, len(voiceRooms))
	for _, room := range voiceRooms {
		if len(room.Members) == 0 {
			continue
		}
		members := make([]*VoiceMemberState, 0, len(room.Members))
		for _, m := range room.Members {
			copy := *m
			members = append(members, &copy)
		}
		sort.Slice(members, func(i, j int) bool { return members[i].JoinedAt < members[j].JoinedAt })
		rooms = append(rooms, LiveVoiceRoom{
			ChannelID: room.ChannelID,
			CrewID:    room.CrewID,
			Members:   members,
		})
	}
	sort.Slice(rooms, func(i, j int) bool {
		if rooms[i].CrewID != rooms[j].CrewID {
			return rooms[i].CrewID < rooms[j].CrewID
		}
		return rooms[i].ChannelID < rooms[j].ChannelID
	})
	return rooms
}

// applyChannelNames resolves each room's display name from its crew's
// voice_channels document. Crews are read once each. A missing definition
// falls back to the channel ID so a room never renders nameless.
func applyChannelNames(ctx context.Context, nk runtime.NakamaModule, rooms []LiveVoiceRoom) {
	crews := map[string]bool{}
	for _, r := range rooms {
		crews[r.CrewID] = true
	}
	names := map[string]string{}
	for crewID := range crews {
		list, err := GetVoiceChannels(ctx, nk, crewID)
		if err != nil {
			continue
		}
		for _, ch := range list.Channels {
			names[ch.ID] = ch.Name
		}
	}
	for i := range rooms {
		if name, ok := names[rooms[i].ChannelID]; ok && name != "" {
			rooms[i].ChannelName = name
		} else {
			rooms[i].ChannelName = rooms[i].ChannelID
		}
	}
}

// applyCrewNames fills display names for every crew in the snapshot with a
// single batched group read. Every crew runs a "General" channel, so the
// name is what tells rooms apart. A missing group falls back to the crew
// ID prefix so a room never renders nameless.
func applyCrewNames(ctx context.Context, nk runtime.NakamaModule, snap *LiveSnapshot) {
	ids := map[string]bool{}
	for _, r := range snap.VoiceRooms {
		ids[r.CrewID] = true
	}
	for _, s := range snap.Streams {
		ids[s.CrewID] = true
	}
	if len(ids) == 0 {
		return
	}
	list := make([]string, 0, len(ids))
	for id := range ids {
		list = append(list, id)
	}
	names := map[string]string{}
	if groups, err := nk.GroupsGetId(ctx, list); err == nil {
		for _, g := range groups {
			if name := g.GetName(); name != "" {
				names[g.GetId()] = name
			}
		}
	}
	for i := range snap.VoiceRooms {
		snap.VoiceRooms[i].CrewName = displayCrewName(names, snap.VoiceRooms[i].CrewID)
	}
	for i := range snap.Streams {
		snap.Streams[i].CrewName = displayCrewName(names, snap.Streams[i].CrewID)
	}
}

func displayCrewName(names map[string]string, crewID string) string {
	if name, ok := names[crewID]; ok {
		return name
	}
	if len(crewID) > 8 {
		return crewID[:8] + "…"
	}
	return crewID
}

// collectLiveStreams scans the stream_meta collection, where one object per
// crew means one active stream. Same read as the stream GC.
func collectLiveStreams(ctx context.Context, nk runtime.NakamaModule) ([]LiveStream, error) {
	streams := []LiveStream{}
	cursor := ""
	for {
		objects, nextCursor, err := nk.StorageList(ctx, "", SystemUserID, StreamMetaCollection, 100, cursor)
		if err != nil {
			return nil, err
		}
		for _, obj := range objects {
			var meta StreamMeta
			if err := json.Unmarshal([]byte(obj.Value), &meta); err != nil {
				continue
			}
			streams = append(streams, LiveStream{
				StreamerID:       meta.StreamerID,
				StreamerUsername: meta.StreamerUsername,
				StreamID:         meta.StreamID,
				CrewID:           meta.CrewID,
				Title:            meta.Title,
				StartedAt:        meta.StartedAt,
				ThumbnailURL:     meta.ThumbnailURL,
				ViewerCount:      len(meta.ViewerIDs),
			})
		}
		if nextCursor == "" {
			break
		}
		cursor = nextCursor
	}
	sort.Slice(streams, func(i, j int) bool { return streams[i].StartedAt < streams[j].StartedAt })
	return streams, nil
}
