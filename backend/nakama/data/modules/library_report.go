package main

// Installed-library report (plans/next-gen-feed.md section 5, spec 17
// section 3.3, spec 16 section 3.5).
//
// The client sends the games installed on the device after a library scan,
// only when the user has "Show crew what I play" on. The server keeps one
// document per user and replaces it on every report. It is not a ledger event:
// the ledger has a size budget (spec 16 section 3.4) and the library is state,
// not history. The crew feed reads the documents of the crew members as the
// system to build the recent games card.

import (
	"context"
	"database/sql"
	"encoding/json"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/heroiclabs/nakama-common/runtime"
)

// UserLibraryCollection holds one document per user under UserLibraryKey.
// Owner can read, only the server can write.
const (
	UserLibraryCollection = "user_library"
	UserLibraryKey        = "snapshot"
)

const (
	maxLibraryGames       = 500
	maxLibraryPayloadSize = 256 * 1024
	maxLibraryIDLen       = 128
	maxLibraryNameRunes   = 128
	maxLibraryLauncherLen = 32
	// 120,000 h in minutes. Steam's highest real lifetime hours are under
	// 100,000 h, so a larger value is corrupt or hostile. It is clamped, not
	// rejected, so one bad entry does not lose the whole report.
	maxLibraryPlaytimeMin = 120_000 * 60
)

// LibraryGame is one installed game as stored. Optional figures are pointers,
// so "no figure" and "zero" stay different.
type LibraryGame struct {
	GameID         string `json:"game_id"`
	Name           string `json:"name"`
	Launcher       string `json:"launcher"`
	PlaytimeMin    *int   `json:"playtime_min,omitempty"`
	LastPlayedUnix *int64 `json:"last_played_unix,omitempty"`
}

// LibraryDoc is the per-user document. UpdatedAt is Unix milliseconds.
type LibraryDoc struct {
	Games     []LibraryGame `json:"games"`
	UpdatedAt int64         `json:"updated_at"`
}

// libraryReportGame is one entry of the request. Hidden is not part of the
// contract: the client filters hidden games before it sends. If the field is
// present and true, the server drops the entry too.
type libraryReportGame struct {
	LibraryGame
	Hidden *bool `json:"hidden,omitempty"`
}

type libraryReportRequest struct {
	Games []libraryReportGame `json:"games"`
}

func validLibraryGameID(id string) bool {
	if id == "" || len(id) > maxLibraryIDLen {
		return false
	}
	// `<prefix>-<external_id>`: both parts must exist.
	i := strings.Index(id, "-")
	if i <= 0 || i == len(id)-1 {
		return false
	}
	for _, r := range id {
		if r <= ' ' || r == 0x7f {
			return false
		}
	}
	return true
}

func validLibraryLauncher(s string) bool {
	if s == "" || len(s) > maxLibraryLauncherLen {
		return false
	}
	for _, r := range s {
		if !(r >= 'a' && r <= 'z' || r >= '0' && r <= '9' || r == '_') {
			return false
		}
	}
	return true
}

// normalizeLibraryReport validates the request and returns the games to store.
// Hidden entries are dropped. A repeated game_id keeps its first entry.
// A playtime above the cap is clamped; the second result counts those entries.
func normalizeLibraryReport(req libraryReportRequest) ([]LibraryGame, int, error) {
	if len(req.Games) > maxLibraryGames {
		return nil, 0, runtime.NewError("too many games, the limit is 500", 3)
	}
	clamped := 0
	out := make([]LibraryGame, 0, len(req.Games))
	seen := make(map[string]bool, len(req.Games))
	for _, g := range req.Games {
		if !validLibraryGameID(g.GameID) {
			return nil, 0, runtime.NewError("invalid game_id", 3)
		}
		name := strings.TrimSpace(g.Name)
		if name == "" || utf8.RuneCountInString(name) > maxLibraryNameRunes || strings.ContainsAny(name, "\x00\r\n") {
			return nil, 0, runtime.NewError("invalid name", 3)
		}
		if !validLibraryLauncher(g.Launcher) {
			return nil, 0, runtime.NewError("invalid launcher", 3)
		}
		if g.PlaytimeMin != nil && *g.PlaytimeMin < 0 {
			return nil, 0, runtime.NewError("invalid playtime_min", 3)
		}
		if g.PlaytimeMin != nil && *g.PlaytimeMin > maxLibraryPlaytimeMin {
			v := maxLibraryPlaytimeMin
			g.PlaytimeMin = &v
			clamped++
		}
		if g.LastPlayedUnix != nil && *g.LastPlayedUnix < 0 {
			return nil, 0, runtime.NewError("invalid last_played_unix", 3)
		}
		if g.Hidden != nil && *g.Hidden {
			continue
		}
		if seen[g.GameID] {
			continue
		}
		seen[g.GameID] = true
		out = append(out, LibraryGame{
			GameID:         g.GameID,
			Name:           name,
			Launcher:       g.Launcher,
			PlaytimeMin:    g.PlaytimeMin,
			LastPlayedUnix: g.LastPlayedUnix,
		})
	}
	return out, clamped, nil
}

// LibraryReportRPC replaces the caller's library document. An empty list is
// valid and clears the document: the client sends it when the user turns
// sharing off.
func LibraryReportRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok || userID == "" {
		return "", runtime.NewError("authentication required", 16)
	}
	if len(payload) > maxLibraryPayloadSize {
		return "", runtime.NewError("payload too large", 3)
	}
	var req libraryReportRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	games, clamped, err := normalizeLibraryReport(req)
	if err != nil {
		return "", err
	}
	if clamped > 0 {
		logger.Warn("library_report: user=%s clamped playtime_min on %d entries to %d", userID, clamped, maxLibraryPlaytimeMin)
	}

	changed := libraryGamesChanged(readLibraryDocs(ctx, nk, []string{userID})[userID], games)
	doc := LibraryDoc{Games: games, UpdatedAt: time.Now().UnixMilli()}
	data, err := json.Marshal(doc)
	if err != nil {
		return "", runtime.NewError("internal error", 13)
	}
	// No version: the write is a replace, last report wins.
	if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection:      UserLibraryCollection,
		Key:             UserLibraryKey,
		UserID:          userID,
		Value:           string(data),
		PermissionRead:  1, // owner only
		PermissionWrite: 0, // server only
	}}); err != nil {
		logger.Error("library_report: write failed for user %s: %v", userID, err)
		return "", runtime.NewError("failed to store library", 13)
	}
	logger.Debug("library_report: user=%s games=%d", userID, len(games))
	// Derived data: a failure is logged inside and never fails the report. An
	// empty list (sharing off) recomputes too, so the user leaves the titles.
	recomputeHead2headForUser(ctx, logger, nk, userID, head2headTriggerLibrary)
	// The recent games card and the head2head titles read the library. A
	// report that changes nothing (the client repeats it on every start) does
	// not wake the feeds of the crew.
	if changed {
		PushFeedChangedForUser(ctx, logger, nk, userID, "library")
	}
	return `{"success":true,"count":` + strconv.Itoa(len(games)) + `}`, nil
}

// libraryGamesChanged reports whether the new list differs from the stored
// document: another game, launcher or playtime. The time stamp does not count.
func libraryGamesChanged(prev *LibraryDoc, next []LibraryGame) bool {
	if prev == nil {
		return len(next) > 0
	}
	if len(prev.Games) != len(next) {
		return true
	}
	byID := make(map[string]LibraryGame, len(prev.Games))
	for _, g := range prev.Games {
		byID[g.GameID] = g
	}
	for _, g := range next {
		p, ok := byID[g.GameID]
		if !ok || p.Name != g.Name || p.Launcher != g.Launcher || !equalIntPtr(p.PlaytimeMin, g.PlaytimeMin) {
			return true
		}
	}
	return false
}

func equalIntPtr(a, b *int) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
}

// readLibraryDocs reads the library documents of the given users as the
// system. A user with no document, or a document that does not decode, is
// absent from the result.
func readLibraryDocs(ctx context.Context, nk runtime.NakamaModule, userIDs []string) map[string]*LibraryDoc {
	out := make(map[string]*LibraryDoc, len(userIDs))
	if len(userIDs) == 0 {
		return out
	}
	reads := make([]*runtime.StorageRead, 0, len(userIDs))
	for _, id := range userIDs {
		reads = append(reads, &runtime.StorageRead{Collection: UserLibraryCollection, Key: UserLibraryKey, UserID: id})
	}
	objects, err := nk.StorageRead(ctx, reads)
	if err != nil {
		return out
	}
	for _, o := range objects {
		var d LibraryDoc
		if json.Unmarshal([]byte(o.GetValue()), &d) != nil {
			continue
		}
		out[o.GetUserId()] = &d
	}
	return out
}
