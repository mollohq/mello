package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Riot Games API proxy (spec 18 §9, the "gated" tier).
//
// The API key lives here, server-side only (RIOT_API_KEY env var) — it is
// never shipped in the desktop client. Users opt in by linking their own
// Riot ID; we then fetch only *their own* recent LoL/TFT matches right after
// a play session to verify/enrich the client-reported session record.
// Everything is best-effort: if the key is unset, the link is missing, or
// Riot is unreachable, sessions record exactly as before.
// ---------------------------------------------------------------------------

const (
	RiotAccountCollection = "riot_account"
	riotAccountKey        = "link"

	// Per-request timeout and how many match details we fetch per session.
	// Enrichment runs inside GameSessionEndRPC, so the total budget stays
	// small: worst case ~4 requests (ids + 3 matches).
	riotMaxMatchFetches = 3

	// LoL matches shorter than this are remakes and don't count (Riot's own
	// convention: no LP change under ~5 minutes).
	lolRemakeMaxSec = 300

	// TFT convention: top half of the 8-player lobby counts as a win.
	tftWinMaxPlacement = 4
)

var riotHTTP = &http.Client{Timeout: 2500 * time.Millisecond}

// Regional routing clusters used by account-v1, match-v5, and tft-match-v1.
var riotRegions = map[string]bool{
	"americas": true,
	"europe":   true,
	"asia":     true,
	"sea":      true,
}

func riotAPIKey() string { return os.Getenv("RIOT_API_KEY") }

// RiotAccountLink is the stored opt-in: owner-readable, server-writable.
// Deleted on unlink, which stops all Riot API requests for the user.
type RiotAccountLink struct {
	RiotID   string `json:"riot_id"` // canonical "GameName#TAG" from account-v1
	PUUID    string `json:"puuid"`
	Region   string `json:"region"` // routing cluster, e.g. "europe"
	LinkedAt int64  `json:"linked_at"`
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested without network)
// ---------------------------------------------------------------------------

// parseRiotID splits "GameName#TAG" and applies Riot's length bounds loosely
// (game name up to 16, tag up to 5, both non-empty).
func parseRiotID(s string) (game, tag string, ok bool) {
	game, tag, found := strings.Cut(strings.TrimSpace(s), "#")
	game = strings.TrimSpace(game)
	tag = strings.TrimSpace(tag)
	if !found || game == "" || tag == "" || len(game) > 16 || len(tag) > 5 {
		return "", "", false
	}
	return game, tag, true
}

// lolMatchOutcome extracts our player's result from a match-v5 body.
// counted=false for remakes and matches the player isn't in.
func lolMatchOutcome(body []byte, puuid string) (win, counted bool) {
	var m struct {
		Info struct {
			GameDuration int64 `json:"gameDuration"` // seconds (match-v5 ≥ patch 11.20)
			Participants []struct {
				PUUID string `json:"puuid"`
				Win   bool   `json:"win"`
			} `json:"participants"`
		} `json:"info"`
	}
	if err := json.Unmarshal(body, &m); err != nil {
		return false, false
	}
	if m.Info.GameDuration > 0 && m.Info.GameDuration < lolRemakeMaxSec {
		return false, false
	}
	for _, p := range m.Info.Participants {
		if p.PUUID == puuid {
			return p.Win, true
		}
	}
	return false, false
}

// tftMatchOutcome extracts our player's placement from a tft-match-v1 body.
// Top-half placements count as wins.
func tftMatchOutcome(body []byte, puuid string) (win, counted bool) {
	var m struct {
		Info struct {
			Participants []struct {
				PUUID     string `json:"puuid"`
				Placement int    `json:"placement"`
			} `json:"participants"`
		} `json:"info"`
	}
	if err := json.Unmarshal(body, &m); err != nil {
		return false, false
	}
	for _, p := range m.Info.Participants {
		if p.PUUID == puuid && p.Placement > 0 {
			return p.Placement <= tftWinMaxPlacement, true
		}
	}
	return false, false
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

func readRiotLink(ctx context.Context, nk runtime.NakamaModule, userID string) *RiotAccountLink {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: RiotAccountCollection, Key: riotAccountKey, UserID: userID},
	})
	if err != nil || len(objects) == 0 {
		return nil
	}
	var link RiotAccountLink
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &link); err != nil {
		return nil
	}
	if link.PUUID == "" || !riotRegions[link.Region] {
		return nil
	}
	return &link
}

func writeRiotLink(ctx context.Context, nk runtime.NakamaModule, userID string, link *RiotAccountLink) error {
	data, err := json.Marshal(link)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      RiotAccountCollection,
			Key:             riotAccountKey,
			UserID:          userID,
			Value:           string(data),
			PermissionRead:  1, // owner only
			PermissionWrite: 0, // server only
		},
	})
	return err
}

// ---------------------------------------------------------------------------
// Riot HTTP (thin wrapper; every caller treats failures as "no data")
// ---------------------------------------------------------------------------

func riotGet(region, path string) ([]byte, int, error) {
	req, err := http.NewRequest("GET", fmt.Sprintf("https://%s.api.riotgames.com%s", region, path), nil)
	if err != nil {
		return nil, 0, err
	}
	req.Header.Set("X-Riot-Token", riotAPIKey())
	resp, err := riotHTTP.Do(req)
	if err != nil {
		return nil, 0, err
	}
	defer resp.Body.Close()
	// Sanity cap: match bodies are ~100 KB.
	body, err := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if err != nil {
		return nil, resp.StatusCode, err
	}
	return body, resp.StatusCode, nil
}

// ---------------------------------------------------------------------------
// Link / unlink / status RPCs
// ---------------------------------------------------------------------------

type RiotLinkRequest struct {
	RiotID string `json:"riot_id"` // "GameName#TAG"
	Region string `json:"region"`  // "americas" | "europe" | "asia" | "sea"
}

func RiotLinkRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	if riotAPIKey() == "" {
		return "", runtime.NewError("riot integration not configured", 9)
	}

	var req RiotLinkRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	game, tag, ok := parseRiotID(req.RiotID)
	if !ok {
		return "", runtime.NewError("riot_id must look like GameName#TAG", 3)
	}
	if !riotRegions[req.Region] {
		return "", runtime.NewError("region must be americas, europe, asia, or sea", 3)
	}

	// Resolve the Riot ID to a PUUID — this both validates the id exists and
	// gives us the stable identifier match lookups need.
	path := fmt.Sprintf("/riot/account/v1/accounts/by-riot-id/%s/%s",
		url.PathEscape(game), url.PathEscape(tag))
	body, status, err := riotGet(req.Region, path)
	if err != nil {
		logger.Warn("riot account lookup failed for user %s: %v", userID, err)
		return "", runtime.NewError("riot api unreachable, try again", 14)
	}
	switch {
	case status == 404:
		return "", runtime.NewError("riot id not found", 5)
	case status == 429:
		return "", runtime.NewError("riot api rate limited, try again shortly", 8)
	case status != 200:
		logger.Error("riot account lookup status %d for user %s", status, userID)
		return "", runtime.NewError("riot api error", 13)
	}

	var account struct {
		PUUID    string `json:"puuid"`
		GameName string `json:"gameName"`
		TagLine  string `json:"tagLine"`
	}
	if err := json.Unmarshal(body, &account); err != nil || account.PUUID == "" {
		return "", runtime.NewError("riot api error", 13)
	}

	link := &RiotAccountLink{
		RiotID:   fmt.Sprintf("%s#%s", account.GameName, account.TagLine),
		PUUID:    account.PUUID,
		Region:   req.Region,
		LinkedAt: time.Now().UnixMilli(),
	}
	if err := writeRiotLink(ctx, nk, userID, link); err != nil {
		logger.Error("riot link write failed for user %s: %v", userID, err)
		return "", runtime.NewError("failed to save link", 13)
	}

	logger.Info("User %s linked riot account %s (%s)", userID, link.RiotID, link.Region)
	resp, _ := json.Marshal(map[string]interface{}{
		"success": true, "riot_id": link.RiotID, "region": link.Region,
	})
	return string(resp), nil
}

func RiotUnlinkRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	err := nk.StorageDelete(ctx, []*runtime.StorageDelete{
		{Collection: RiotAccountCollection, Key: riotAccountKey, UserID: userID},
	})
	if err != nil {
		logger.Error("riot unlink failed for user %s: %v", userID, err)
		return "", runtime.NewError("failed to unlink", 13)
	}
	logger.Info("User %s unlinked riot account", userID)
	resp, _ := json.Marshal(map[string]interface{}{"success": true})
	return string(resp), nil
}

func RiotStatusRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	out := map[string]interface{}{
		"available": riotAPIKey() != "",
		"linked":    false,
	}
	if link := readRiotLink(ctx, nk, userID); link != nil {
		out["linked"] = true
		out["riot_id"] = link.RiotID
		out["region"] = link.Region
	}
	resp, _ := json.Marshal(out)
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// Session enrichment (called from GameSessionEndRPC)
// ---------------------------------------------------------------------------

// enrichGameSessionFromRiot fetches the linked player's own matches that
// started during the reported session and returns server-verified win/loss
// counts. ok=false (client numbers stand) when the game isn't Riot's, the
// user hasn't linked, the key is unset, or Riot can't be reached — enrichment
// must never block or fail a session record.
//
// TFT runs inside the LoL client, so sessions detected as league-of-legends
// check match-v5 first and fall back to tft-match-v1 when no LoL matches
// exist in the window.
func enrichGameSessionFromRiot(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, userID, gameID string, durationMin int) (wins, losses int, ok bool) {
	if gameID != "league-of-legends" || riotAPIKey() == "" {
		return 0, 0, false
	}
	link := readRiotLink(ctx, nk, userID)
	if link == nil {
		return 0, 0, false
	}

	// Session window with a 10-minute lead buffer (client detection can start
	// after the first queue pop).
	startTime := time.Now().Unix() - int64(durationMin)*60 - 600

	wins, losses = tallyRiotMatches(logger, link,
		fmt.Sprintf("/lol/match/v5/matches/by-puuid/%s/ids?startTime=%d&count=%d",
			url.PathEscape(link.PUUID), startTime, riotMaxMatchFetches),
		"/lol/match/v5/matches/", lolMatchOutcome)
	if wins+losses == 0 {
		wins, losses = tallyRiotMatches(logger, link,
			fmt.Sprintf("/tft/match/v1/matches/by-puuid/%s/ids?startTime=%d&count=%d",
				url.PathEscape(link.PUUID), startTime, riotMaxMatchFetches),
			"/tft/match/v1/matches/", tftMatchOutcome)
	}
	if wins+losses == 0 {
		return 0, 0, false
	}
	logger.Info("riot enrichment for user %s: %dW-%dL", userID, wins, losses)
	return wins, losses, true
}

// tallyRiotMatches fetches a match-id list and folds each match's outcome for
// the linked player. Bails out quietly on the first error.
func tallyRiotMatches(logger runtime.Logger, link *RiotAccountLink, idsPath, matchPathPrefix string, outcome func([]byte, string) (bool, bool)) (wins, losses int) {
	body, status, err := riotGet(link.Region, idsPath)
	if err != nil || status != 200 {
		if err != nil {
			logger.Debug("riot match ids fetch failed: %v", err)
		}
		return 0, 0
	}
	var ids []string
	if err := json.Unmarshal(body, &ids); err != nil {
		return 0, 0
	}
	if len(ids) > riotMaxMatchFetches {
		ids = ids[:riotMaxMatchFetches]
	}
	for _, id := range ids {
		body, status, err := riotGet(link.Region, matchPathPrefix+url.PathEscape(id))
		if err != nil || status != 200 {
			return wins, losses
		}
		if win, counted := outcome(body, link.PUUID); counted {
			if win {
				wins++
			} else {
				losses++
			}
		}
	}
	return wins, losses
}
