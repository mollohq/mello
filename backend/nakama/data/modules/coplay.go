package main

import (
	"context"
	"encoding/json"

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

// coPlayersFromLedger returns the distinct users whose recorded sessions of the
// same game overlapped `window`, excluding the actor.
//
// Pure so the interval logic is unit-testable; the Nakama read happens in
// collectCoPlayers.
func coPlayersFromLedger(window sessionWindow, others []sessionWindow) []string {
	seen := map[string]bool{window.UserID: true}
	var out []string
	for _, other := range others {
		if other.GameID != window.GameID || seen[other.UserID] {
			continue
		}
		if overlaps(window, other) {
			seen[other.UserID] = true
			out = append(out, other.UserID)
		}
	}
	return out
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
	ledger *CrewEventLedger,
) ([]string, []string) {
	seen := map[string]bool{window.UserID: true}
	ids := []string{window.UserID}

	for _, id := range coPlayersFromLedger(window, ledgerWindows(ledger, window.StartMs)) {
		if !seen[id] {
			seen[id] = true
			ids = append(ids, id)
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
			if presence.Game.GameID == window.GameID {
				seen[id] = true
				ids = append(ids, id)
			}
		}
	}

	names := make([]string, 0, len(ids))
	for _, id := range ids {
		names = append(names, resolveUsername(ctx, nk, id))
	}
	return ids, names
}
