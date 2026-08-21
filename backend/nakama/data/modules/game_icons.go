package main

// Crew-shared game icons (spec 17/18 extension: unknown-game tracking).
//
// A client that confirms a custom game extracts the icon from the game's exe
// and uploads a small PNG here so every crew member sees real artwork instead
// of a letter badge. Mirrors the crew-avatar pattern: small base64 payloads
// in system-owned Nakama storage, public read, server-only write. First
// writer wins per game id — icons never flap and can't be replaced by a
// later (possibly griefing) upload.

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/base64"
	"encoding/json"
	"image/png"
	"regexp"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	GameIconCollection = "game_icons"
	// Decoded PNG size cap.
	//
	// Sized for 256px icons, which is what games actually ship and what the
	// client now extracts: a typical one is 20-100 KB and a worst-case noisy
	// one about 150 KB. The old 48 KB cap assumed 128px and rejected every
	// upload once extraction moved up. 256 KB leaves headroom without letting
	// an arbitrary file through — the PNG decode below still bounds what this
	// can be.
	gameIconMaxBytes = 256 * 1024
)

var gameIconIDPattern = regexp.MustCompile(`^[a-z0-9-]{1,64}$`)

// validateGameIconUpload checks id shape, base64, size cap, and that the
// payload really is a PNG. Returns the decoded bytes or a non-empty error
// string. Pure, so the validation matrix is unit-testable.
func validateGameIconUpload(gameID, data string) ([]byte, string) {
	if !gameIconIDPattern.MatchString(gameID) {
		return nil, "invalid game_id"
	}
	raw, err := base64.StdEncoding.DecodeString(data)
	if err != nil || len(raw) == 0 {
		return nil, "invalid icon data"
	}
	if len(raw) > gameIconMaxBytes {
		return nil, "icon too large"
	}
	if _, err := png.Decode(bytes.NewReader(raw)); err != nil {
		return nil, "icon is not a valid PNG"
	}
	return raw, ""
}

type gameIconDoc struct {
	Data       string `json:"data"` // base64 PNG
	UploaderID string `json:"uploader_id"`
	UpdatedAt  int64  `json:"updated_at"`
}

// GameIconSetRPC stores an icon for a game id unless one already exists.
// Request: { "game_id": "custom-night-stones", "data": "<base64 png>" }
// Response: { "success": true, "stored": bool }
func GameIconSetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok || userID == "" {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		GameID string `json:"game_id"`
		Data   string `json:"data"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	raw, verr := validateGameIconUpload(req.GameID, req.Data)
	if verr != "" {
		return "", runtime.NewError(verr, 3)
	}

	// First writer wins: an existing icon is kept and the duplicate ack'd.
	existing, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: GameIconCollection, Key: req.GameID, UserID: SystemUserID},
	})
	if err == nil && len(existing) > 0 {
		logger.Info("game icon for %s already stored; keeping existing", req.GameID)
		return `{"success":true,"stored":false}`, nil
	}

	doc, _ := json.Marshal(gameIconDoc{
		Data:       req.Data,
		UploaderID: userID,
		UpdatedAt:  time.Now().UnixMilli(),
	})
	if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      GameIconCollection,
			Key:             req.GameID,
			UserID:          SystemUserID,
			Value:           string(doc),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	}); err != nil {
		logger.Error("failed to store game icon %s: %v", req.GameID, err)
		return "", runtime.NewError("failed to store icon", 13)
	}
	logger.Info("stored game icon %s (%d bytes) from %s", req.GameID, len(raw), userID)
	return `{"success":true,"stored":true}`, nil
}

// GameIconGetRPC returns { "data": "<base64 png>" } or error code 5 (like the
// crew-avatar RPC) when no icon exists for the id.
func GameIconGetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	var req struct {
		GameID string `json:"game_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || !gameIconIDPattern.MatchString(req.GameID) {
		return "", runtime.NewError("invalid request", 3)
	}

	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: GameIconCollection, Key: req.GameID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return "", runtime.NewError("icon not found", 5)
	}

	var doc gameIconDoc
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &doc); err != nil {
		return "", runtime.NewError("icon corrupt", 13)
	}
	resp, _ := json.Marshal(map[string]string{"data": doc.Data})
	return string(resp), nil
}
