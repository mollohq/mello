package main

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"math/big"
	"sort"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const (
	ClipMaxDurationSec   = 60
	ClipMaxPerUserPerDay = 50

	CrewClipsCollection = "crew_clips"
	// CrewClipsMaxRetained caps the per-crew clips document. Clips live in a
	// single Nakama storage object which is hard-limited to 256KB. A StoredClip
	// with participant arrays and a waveform serializes to roughly 500-700
	// bytes, so 250 entries keeps a safe margin under the limit (~175KB worst
	// case). Older clips are trimmed; unbounded history is future m3llo+ work.
	CrewClipsMaxRetained = 250
	clipWaveformBytes    = 64

	// An empty clip_type is a voice clip. A game clip is F8 footage
	// (CLIPS.md 15.8).
	clipTypeVoice = "voice"
	clipTypeGame  = "game"
)

// ---------------------------------------------------------------------------
// Clip data model
// ---------------------------------------------------------------------------

type ClipData struct {
	ClipID           string   `json:"clip_id"`
	ClipType         string   `json:"clip_type"`
	ClipperName      string   `json:"clipper_name"`
	DurationSeconds  float64  `json:"duration_seconds"`
	Participants     []string `json:"participants,omitempty"`
	ParticipantNames []string `json:"participant_names,omitempty"`
	Game             string   `json:"game,omitempty"`
	LocalPath        string   `json:"local_path,omitempty"`
	MediaURL         string   `json:"media_url,omitempty"`
}

// ---------------------------------------------------------------------------
// Durable clips store — separate per-crew document, outside the ledger trim
// ---------------------------------------------------------------------------

type StoredClip struct {
	EventID          string   `json:"event_id"` // time-sortable, from generateEventID()
	ClipID           string   `json:"clip_id"`
	ActorID          string   `json:"actor_id"`
	Ts               int64    `json:"ts"`
	Score            int      `json:"score"`
	ClipType         string   `json:"clip_type"`
	ClipperName      string   `json:"clipper_name"`
	DurationSeconds  float64  `json:"duration_seconds"`
	Participants     []string `json:"participants,omitempty"`
	ParticipantNames []string `json:"participant_names,omitempty"`
	Game             string   `json:"game,omitempty"`
	LocalPath        string   `json:"local_path,omitempty"`
	MediaURL         string   `json:"media_url,omitempty"`
	Waveform         string   `json:"waveform,omitempty"`
	// Game clips only. PosterURL is set when the poster is up, MediaURL when
	// the MP4 is up.
	Width     int    `json:"width,omitempty"`
	Height    int    `json:"height,omitempty"`
	HasVoice  bool   `json:"has_voice,omitempty"`
	PosterURL string `json:"poster_url,omitempty"`
}

type CrewClipsDoc struct {
	CrewID    string       `json:"crew_id"`
	Clips     []StoredClip `json:"clips"` // newest appended last
	UpdatedAt int64        `json:"updated_at"`
}

func readClipsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string) (*CrewClipsDoc, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewClipsCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return &CrewClipsDoc{CrewID: crewID, Clips: []StoredClip{}}, ""
	}

	var doc CrewClipsDoc
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &doc); err != nil {
		return &CrewClipsDoc{CrewID: crewID, Clips: []StoredClip{}}, ""
	}
	return &doc, objects[0].GetVersion()
}

func writeClipsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string, doc *CrewClipsDoc, version string) error {
	data, err := json.Marshal(doc)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      CrewClipsCollection,
			Key:             crewID,
			UserID:          SystemUserID,
			Value:           string(data),
			Version:         version,
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})
	return err
}

// capClips trims a clip slice to the most-recent CrewClipsMaxRetained by
// timestamp. Pure (no I/O) so the cap behavior is unit-testable.
func capClips(clips []StoredClip) []StoredClip {
	if len(clips) <= CrewClipsMaxRetained {
		return clips
	}
	sort.Slice(clips, func(i, j int) bool { return clips[i].Ts < clips[j].Ts })
	return clips[len(clips)-CrewClipsMaxRetained:]
}

// errClipIDTaken means another actor already posted a clip with this clip_id.
var errClipIDTaken = errors.New("clip_id taken by another actor")

// existingClip finds clipID in a crew's clips. A clip of the same actor is
// returned: the client retries post_clip when an answer is lost. taken is
// true when only another actor holds the clip_id. The pointer is into clips.
// Pure, so the retry rule is testable.
func existingClip(clips []StoredClip, clipID, actorID string) (found *StoredClip, taken bool) {
	for i := range clips {
		if clips[i].ClipID != clipID {
			continue
		}
		if clips[i].ActorID == actorID {
			return &clips[i], false
		}
		taken = true
	}
	return nil, taken
}

// ownVoiceClip finds the caller's voice clip in a crew's clips. The error is a
// Nakama error: 5 when the clip is not there, 7 when it is another actor's,
// 3 when it is not a voice clip. A clip from before clip_type is voice. Pure,
// so the ownership rule is testable.
func ownVoiceClip(clips []StoredClip, clipID, userID string) (*StoredClip, error) {
	clip, taken := existingClip(clips, clipID, userID)
	switch {
	case clip == nil && taken:
		return nil, runtime.NewError("not the clipper", 7)
	case clip == nil:
		return nil, runtime.NewError("clip not found", 5)
	case clip.ClipType != "" && clip.ClipType != clipTypeVoice:
		return nil, runtime.NewError("not a voice clip", 3)
	}
	return clip, nil
}

// AppendClip appends a clip to the durable clips doc, trims to the most-recent
// cap, and retries on version conflict. When the doc already holds the
// clip_id of the same actor, it appends nothing and returns that clip, so a
// retried post_clip stores one clip. Another actor's clip_id returns
// errClipIDTaken. The check runs on each attempt, so two concurrent retries
// also store one clip.
func AppendClip(ctx context.Context, nk runtime.NakamaModule, crewID string, clip StoredClip) (*StoredClip, error) {
	for attempt := 0; attempt < 3; attempt++ {
		doc, version := readClipsDoc(ctx, nk, crewID)
		prev, taken := existingClip(doc.Clips, clip.ClipID, clip.ActorID)
		if prev != nil {
			return prev, nil
		}
		if taken {
			return nil, errClipIDTaken
		}
		doc.Clips = capClips(append(doc.Clips, clip))
		doc.UpdatedAt = time.Now().UnixMilli()
		if err := writeClipsDoc(ctx, nk, crewID, doc, version); err == nil {
			return nil, nil
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return nil, fmt.Errorf("crew_clips write failed after 3 retries for crew %s", crewID)
}

// validateClipWaveform checks an optional base64-encoded peak waveform.
// Empty is allowed for older clients. When present it must decode to exactly
// clipWaveformBytes u8 peaks. Returns a non-empty error string on failure.
func validateClipWaveform(waveform string) string {
	if waveform == "" {
		return ""
	}
	raw, err := base64.StdEncoding.DecodeString(waveform)
	if err != nil {
		return "invalid waveform"
	}
	if len(raw) != clipWaveformBytes {
		return "waveform must be 64 bytes"
	}
	return ""
}

// ---------------------------------------------------------------------------
// PostClip RPC — store clip metadata in the crew event ledger
// ---------------------------------------------------------------------------

type PostClipRequest struct {
	CrewID          string   `json:"crew_id"`
	ClipID          string   `json:"clip_id"`
	ClipType        string   `json:"clip_type"`
	DurationSeconds float64  `json:"duration_seconds"`
	Participants    []string `json:"participants,omitempty"`
	Game            string   `json:"game,omitempty"`
	LocalPath       string   `json:"local_path,omitempty"`
	Waveform        string   `json:"waveform,omitempty"`
	// Game clips only (validatePostClipType).
	Width    int  `json:"width,omitempty"`
	Height   int  `json:"height,omitempty"`
	HasVoice bool `json:"has_voice,omitempty"`
}

// postClipResponse is the post_clip answer, the same for a new clip and a
// retry.
// clipPostedPayload is the data of the clip_posted crew event (code 111). The
// desktop client plays the crew clip sound on it. Ids only.
func clipPostedPayload(clipID, clipperID, clipType string) map[string]interface{} {
	return map[string]interface{}{
		"clip_id":    clipID,
		"clipper_id": clipperID,
		"clip_type":  clipType,
	}
}

func postClipResponse(eventID, clipID string) string {
	resp, _ := json.Marshal(map[string]interface{}{
		"success":  true,
		"event_id": eventID,
		"clip_id":  clipID,
	})
	return string(resp)
}

func PostClipRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req PostClipRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.ClipID == "" {
		return "", runtime.NewError("crew_id and clip_id required", 3)
	}
	if req.ClipType == "" {
		req.ClipType = clipTypeVoice
	}
	if errStr := validatePostClipType(req); errStr != "" {
		return "", runtime.NewError(errStr, 3)
	}
	if req.DurationSeconds <= 0 || req.DurationSeconds > ClipMaxDurationSec {
		return "", runtime.NewError(fmt.Sprintf("duration must be 0-%d seconds", ClipMaxDurationSec), 3)
	}
	if errStr := validateClipWaveform(req.Waveform); errStr != "" {
		return "", runtime.NewError(errStr, 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	// A retry of a stored clip answers as the first call did. It runs before
	// the rate limit, so a retry never counts against it.
	clipsDoc, _ := readClipsDoc(ctx, nk, req.CrewID)
	prev, taken := existingClip(clipsDoc.Clips, req.ClipID, userID)
	if prev != nil {
		logger.Info("post_clip retry: crew=%s clip_id=%s, returning the stored clip", req.CrewID, req.ClipID)
		return postClipResponse(prev.EventID, prev.ClipID), nil
	}
	if taken {
		return "", runtime.NewError("clip_id already exists", 6)
	}

	// Rate limit: max clips per user per day, counted from the durable clips doc
	dayStart := time.Now().Truncate(24 * time.Hour).UnixMilli()
	clipCount := 0
	for _, c := range clipsDoc.Clips {
		if c.ActorID == userID && c.Ts >= dayStart {
			clipCount++
		}
	}
	if clipCount >= ClipMaxPerUserPerDay {
		return "", runtime.NewError("clip rate limit exceeded", 8)
	}

	username := resolveUsername(ctx, nk, userID)
	participantNames := make([]string, 0, len(req.Participants))
	for _, pid := range req.Participants {
		if n := resolveUsername(ctx, nk, pid); n != "" {
			participantNames = append(participantNames, n)
		}
	}

	eventID := generateEventID()
	clip := StoredClip{
		EventID:          eventID,
		ClipID:           req.ClipID,
		ActorID:          userID,
		Ts:               time.Now().UnixMilli(),
		Score:            50,
		ClipType:         req.ClipType,
		ClipperName:      username,
		DurationSeconds:  req.DurationSeconds,
		Participants:     req.Participants,
		ParticipantNames: participantNames,
		Game:             req.Game,
		LocalPath:        req.LocalPath,
		Waveform:         req.Waveform,
	}
	if req.ClipType == clipTypeGame {
		clip.Width = req.Width
		clip.Height = req.Height
		clip.HasVoice = req.HasVoice
	}

	stored, err := AppendClip(ctx, nk, req.CrewID, clip)
	if errors.Is(err, errClipIDTaken) {
		return "", runtime.NewError("clip_id already exists", 6)
	}
	if err != nil {
		logger.Error("Failed to append clip: %v", err)
		return "", runtime.NewError("failed to save clip", 13)
	}
	if stored != nil {
		// A concurrent retry stored the clip first.
		logger.Info("post_clip retry: crew=%s clip_id=%s, returning the stored clip", req.CrewID, req.ClipID)
		return postClipResponse(stored.EventID, stored.ClipID), nil
	}

	logger.Info("User %s (%s) clipped in crew %s: clip_id=%s type=%s",
		userID, username, req.CrewID, req.ClipID, req.ClipType)
	recordClipActivity(ctx, logger, nk, userID)
	questsMarkClip(ctx, logger, nk, req.CrewID, clip)
	recomputeHead2headForUser(ctx, logger, nk, userID, head2headTriggerSession)
	PushFeedChanged(ctx, logger, nk, req.CrewID, "clip")
	PushCrewEvent(ctx, logger, nk, req.CrewID, "clip_posted", clipPostedPayload(req.ClipID, userID, req.ClipType))

	return postClipResponse(eventID, req.ClipID), nil
}

// ---------------------------------------------------------------------------
// CrewTimeline RPC — paginated feed of all crew events (newest first)
// ---------------------------------------------------------------------------

type TimelineEntry struct {
	ID      string      `json:"id"`
	Type    string      `json:"type"`
	ActorID string      `json:"actor_id"`
	Ts      int64       `json:"ts"`
	Score   int         `json:"score"`
	Data    interface{} `json:"data"`
}

// buildMergedTimeline merges three bounded sources into the live feed: the
// 7-day ledger, recent durable clips (last 7 days), and the single latest
// recap. Result is sorted by timestamp descending (newest first). Durable
// history beyond this window is the zones history (crewFeedZones).
func buildMergedTimeline(ctx context.Context, nk runtime.NakamaModule, crewID string) []TimelineEntry {
	ledger, _ := readLedger(ctx, nk, crewID)

	all := make([]TimelineEntry, 0, len(ledger.Events))
	for _, e := range ledger.Events {
		all = append(all, TimelineEntry{
			ID:      e.ID,
			Type:    e.Type,
			ActorID: e.ActorID,
			Ts:      e.Timestamp,
			Score:   e.Score,
			Data:    e.Data,
		})
	}

	clipCutoff := time.Now().Add(-time.Duration(EventRollingWindowDays) * 24 * time.Hour).UnixMilli()
	clipsDoc, _ := readClipsDoc(ctx, nk, crewID)
	for _, c := range clipsDoc.Clips {
		if c.Ts < clipCutoff {
			continue
		}
		all = append(all, TimelineEntry{
			ID:      c.EventID,
			Type:    "clip",
			ActorID: c.ActorID,
			Ts:      c.Ts,
			Score:   c.Score,
			Data:    c,
		})
	}

	recapsDoc, _ := readRecapsDoc(ctx, nk, crewID)
	stampRecapWeeks(ctx, nk, crewID, recapsDoc.Recaps)
	if len(recapsDoc.Recaps) > 0 {
		latest := recapsDoc.Recaps[0]
		for _, r := range recapsDoc.Recaps[1:] {
			if r.WeekStart > latest.WeekStart {
				latest = r
			}
		}
		all = append(all, TimelineEntry{
			ID:      fmt.Sprintf("recap_%d", latest.WeekStart),
			Type:    "weekly_recap",
			ActorID: "",
			Ts:      latest.GeneratedAt,
			Score:   30,
			Data:    latest,
		})
	}

	sort.Slice(all, func(i, j int) bool {
		return all[i].Ts > all[j].Ts
	})

	return all
}

// ---------------------------------------------------------------------------
// ClipUploadURL RPC — return a presigned PUT URL for direct R2/S3 upload
// ---------------------------------------------------------------------------

type ClipUploadURLRequest struct {
	ClipID string `json:"clip_id"`
	CrewID string `json:"crew_id"`
	// Game clips only (gameClipUploadURL). An empty clip_type is voice.
	ClipType    string `json:"clip_type,omitempty"`
	PosterBytes int64  `json:"poster_bytes,omitempty"`
	VideoBytes  int64  `json:"video_bytes,omitempty"`
	UploadID    string `json:"upload_id,omitempty"`
}

type ClipUploadURLResponse struct {
	UploadURL string `json:"upload_url"`
	MediaURL  string `json:"media_url"`
}

func ClipUploadURLRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req ClipUploadURLRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.ClipID == "" || req.CrewID == "" {
		return "", runtime.NewError("clip_id and crew_id required", 3)
	}
	switch req.ClipType {
	case "", clipTypeVoice:
	case clipTypeGame:
		return gameClipUploadURL(ctx, logger, nk, userID, req)
	default:
		return "", runtime.NewError("clip_type must be voice or game", 3)
	}

	// Only the clipper gets a PUT URL for the clip's key, after post_clip.
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}
	doc, _ := readClipsDoc(ctx, nk, req.CrewID)
	if _, err := ownVoiceClip(doc.Clips, req.ClipID, userID); err != nil {
		return "", err
	}

	if !S3IsConfigured() {
		logger.Warn("clip_upload_url: S3 not configured, returning empty URLs")
		resp := ClipUploadURLResponse{}
		data, _ := json.Marshal(resp)
		return string(data), nil
	}

	key := fmt.Sprintf("crews/%s/%s.mp4", req.CrewID, req.ClipID)
	uploadURL, err := GeneratePresignedPUT(key, "audio/mp4", 15*time.Minute)
	if err != nil {
		logger.Error("clip_upload_url: presign failed: %v", err)
		return "", runtime.NewError("failed to generate upload URL", 13)
	}

	mediaURL := S3PublicURL(key)

	resp := ClipUploadURLResponse{
		UploadURL: uploadURL,
		MediaURL:  mediaURL,
	}
	data, _ := json.Marshal(resp)
	return string(data), nil
}

// ---------------------------------------------------------------------------
// ClipUploadComplete RPC — set media_url on clip event after successful upload
// ---------------------------------------------------------------------------

type ClipUploadCompleteRequest struct {
	ClipID string `json:"clip_id"`
	CrewID string `json:"crew_id"`
	// Game clips only (gameClipUploadComplete). An empty clip_type is voice.
	// part is poster or video; upload_id and parts are for the video.
	ClipType string              `json:"clip_type,omitempty"`
	Part     string              `json:"part,omitempty"`
	UploadID string              `json:"upload_id,omitempty"`
	Parts    []CompletedClipPart `json:"parts,omitempty"`
}

func ClipUploadCompleteRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req ClipUploadCompleteRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.ClipID == "" || req.CrewID == "" {
		return "", runtime.NewError("clip_id and crew_id required", 3)
	}
	switch req.ClipType {
	case "", clipTypeVoice:
	case clipTypeGame:
		return gameClipUploadComplete(ctx, logger, nk, userID, req)
	default:
		return "", runtime.NewError("clip_type must be voice or game", 3)
	}

	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	key := fmt.Sprintf("crews/%s/%s.mp4", req.CrewID, req.ClipID)
	mediaURL := S3PublicURL(key)

	for attempt := 0; attempt < 3; attempt++ {
		doc, version := readClipsDoc(ctx, nk, req.CrewID)
		clip, err := ownVoiceClip(doc.Clips, req.ClipID, userID)
		if err != nil {
			return "", err
		}
		clip.MediaURL = mediaURL
		doc.UpdatedAt = time.Now().UnixMilli()
		if err := writeClipsDoc(ctx, nk, req.CrewID, doc, version); err == nil {
			break
		}
		if attempt == 2 {
			logger.Error("clip_upload_complete: writeClipsDoc failed after 3 retries")
			return "", runtime.NewError("failed to update clip media url", 13)
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}

	logger.Info("Clip upload complete: crew=%s clip=%s media_url=%s", req.CrewID, req.ClipID, mediaURL)
	resp, _ := json.Marshal(map[string]interface{}{
		"success":   true,
		"media_url": mediaURL,
	})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// CrewClips RPC — paginated durable clip history (newest first)
// ---------------------------------------------------------------------------
