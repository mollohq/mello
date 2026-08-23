package main

import (
	"context"
	"encoding/json"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// gameSessionDataOf decodes a ledger event's payload as a game session.
//
// CrewEvent.Data is `interface{}` — a struct when the event was just built,
// a map when it came back through storage — so this round-trips via JSON
// rather than type-asserting, which would work in tests and fail in
// production.
func gameSessionDataOf(event CrewEvent) (GameSessionData, bool) {
	var data GameSessionData
	raw, err := json.Marshal(event.Data)
	if err != nil {
		return data, false
	}
	if err := json.Unmarshal(raw, &data); err != nil {
		return data, false
	}
	return data, true
}

// ---------------------------------------------------------------------------
// Co-play attribution.
//
// "You and kim played 2h of CS2 last night" is the crew-feel payoff, and it
// needs to know who was in a game *together*. GameSessionData has carried
// PlayerIDs since spec 16, but it was always a one-element stub containing the
// actor, so nothing downstream could tell a solo night from a squad night.
//
// This is deliberately data production, not presentation: it fills the field
// so spec 19 can decide how (or whether) to surface it. See
// plans/GAME-SENSING-V2.md §13 — if it renders, it belongs to 19.
//
// Two sources, because either alone misses half the cases:
//
//   Ledger overlap  catches crewmates who already finished. Their session is
//                   recorded with a start and end, so a real interval overlap
//                   is computable.
//   Live presence   catches crewmates who are *still* playing. They have no
//                   ledger event yet, and waiting for one would mean the first
//                   person to quit never gets credited with company.
// ---------------------------------------------------------------------------

// sessionWindow is a half-open interval [StartMs, EndMs) of play.
type sessionWindow struct {
	UserID  string
	GameID  string
	StartMs int64
	EndMs   int64
}

// overlaps reports whether two play windows intersect at all.
//
// Touching endpoints do not count: a session that ends exactly as another
// begins is a handoff, not company.
func overlaps(a, b sessionWindow) bool {
	return a.StartMs < b.EndMs && b.StartMs < a.EndMs
}

// overlapMinutes reports how many full minutes two half-open windows intersect.
//
// Touching endpoints and sub-minute overlap both yield 0, consistent with
// overlaps.
func overlapMinutes(a, b sessionWindow) int {
	start := a.StartMs
	if b.StartMs > start {
		start = b.StartMs
	}
	end := a.EndMs
	if b.EndMs < end {
		end = b.EndMs
	}
	overlapMs := end - start
	if overlapMs <= 0 {
		return 0
	}
	return int(overlapMs / 60_000)
}

// coPlayersFromLedger returns the distinct users whose recorded sessions of the
// same game overlapped `window`, excluding the actor, plus aligned overlap
// minutes for each.
//
// Pure so the interval logic is unit-testable; the Nakama read happens in
// collectCoPlayers.
func coPlayersFromLedger(window sessionWindow, others []sessionWindow) ([]string, []int) {
	seen := map[string]bool{window.UserID: true}
	var ids []string
	var overlapMins []int
	for _, other := range others {
		if other.GameID != window.GameID || seen[other.UserID] {
			continue
		}
		if overlaps(window, other) {
			seen[other.UserID] = true
			ids = append(ids, other.UserID)
			overlapMins = append(overlapMins, overlapMinutes(window, other))
		}
	}
	return ids, overlapMins
}

// ledgerWindows extracts play windows from a crew's game_session events.
//
// The ledger stores an end timestamp and a duration, so the start is derived.
// Events without a duration are skipped rather than treated as instantaneous —
// a zero-length window can never overlap anything and would silently drop the
// crewmate.
func ledgerWindows(ledger *CrewEventLedger, sinceMs int64) []sessionWindow {
	if ledger == nil {
		return nil
	}
	var out []sessionWindow
	for _, event := range ledger.Events {
		if event.Type != "game_session" || event.Timestamp < sinceMs {
			continue
		}
		data, ok := gameSessionDataOf(event)
		if !ok || data.DurationMin <= 0 || data.GameID == "" {
			continue
		}
		out = append(out, sessionWindow{
			UserID:  event.ActorID,
			GameID:  data.GameID,
			StartMs: event.Timestamp - int64(data.DurationMin)*60_000,
			EndMs:   event.Timestamp,
		})
	}
	return out
}

// gamePresenceStartedAtMs parses a live game presence start time to Unix ms.
// Unparseable or missing values return 0 so stale presences are ignored.
func gamePresenceStartedAtMs(p *GamePresence) int64 {
	if p == nil || p.StartedAt == "" {
		return 0
	}
	t, err := time.Parse(time.RFC3339, p.StartedAt)
	if err != nil {
		return 0
	}
	return t.UnixMilli()
}

// presenceCountsAsCoPlay reports whether a crewmate's live game presence should
// attribute them as co-playing in window.
//
// Pure so the freshness rules are unit-testable; the Nakama read happens in
// collectCoPlayers.
func presenceCountsAsCoPlay(gameID string, startedAtMs int64, window sessionWindow) bool {
	if gameID != window.GameID {
		return false
	}
	if startedAtMs <= 0 || startedAtMs >= window.EndMs {
		return false
	}
	return true
}

// collectCoPlayers unions the two sources and resolves usernames.
//
// Best-effort throughout: co-play is an enrichment, and a storage hiccup must
// degrade the card, never block the session from being recorded.
func collectCoPlayers(
	ctx context.Context,
	logger runtime.Logger,
	nk runtime.NakamaModule,
	crewID string,
	window sessionWindow,
	actorDurationMin int,
	ledger *CrewEventLedger,
) ([]string, []string, []int) {
	seen := map[string]bool{window.UserID: true}
	ids := []string{window.UserID}
	overlapMins := []int{actorDurationMin}

	ledgerIDs, ledgerOverlaps := coPlayersFromLedger(window, ledgerWindows(ledger, window.StartMs))
	for i, id := range ledgerIDs {
		if !seen[id] {
			seen[id] = true
			ids = append(ids, id)
			overlapMins = append(overlapMins, ledgerOverlaps[i])
		}
	}

	// Still-playing crewmates have no ledger event yet.
	members, _, err := nk.GroupUsersList(ctx, crewID, 100, nil, "")
	if err != nil {
		logger.Debug("co-play: member list unavailable for %s: %v", crewID, err)
	} else {
		for _, m := range members {
			if m.GetUser() == nil {
				continue
			}
			id := m.GetUser().GetId()
			if seen[id] {
				continue
			}
			presence, err := ReadPresence(ctx, nk, id)
			if err != nil || presence == nil || presence.Game == nil {
				continue
			}
			startedAtMs := gamePresenceStartedAtMs(presence.Game)
			if presenceCountsAsCoPlay(presence.Game.GameID, startedAtMs, window) {
				seen[id] = true
				ids = append(ids, id)
				overlapMins = append(overlapMins, overlapMinutes(window, sessionWindow{
					StartMs: startedAtMs,
					EndMs:   window.EndMs,
				}))
			}
		}
	}

	names := make([]string, 0, len(ids))
	for _, id := range ids {
		names = append(names, resolveUsername(ctx, nk, id))
	}
	return ids, names, overlapMins
}
