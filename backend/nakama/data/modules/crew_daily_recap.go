package main

import (
	"sort"
	"time"
)

// ---------------------------------------------------------------------------
// Daily recap (CLIPS.md section 7, spec 19)
//
// A compact version of the weekly recap for one day. The server builds one
// entry for each of the last three days (UTC), from the same sources the
// weekly recap reads: voice, stream and game sessions, clips, and the holders
// doc for belts that moved. A day with nothing in it has no entry: the
// empty-card rule (cardHasContent). The client labels the day ("Today so far",
// "Yesterday", "Tuesday") from DayStart.
// ---------------------------------------------------------------------------

const (
	// feedDailyRecapDays is how many days back the feed looks, today included.
	feedDailyRecapDays = 3
	// feedDailyRecapBelts is the most belts a daily recap lists.
	feedDailyRecapBelts = 2
	// feedDailyRecapMembers is the most active members a daily recap lists.
	feedDailyRecapMembers = 8

	dayMs = 24 * 60 * 60 * 1000
)

// DailyRecapData is the payload of a daily-recap entry. The field names that
// the weekly recap uses keep their names (total_min, clip_count, top_game,
// game_id, belts_moved), so the client reads both with the same helpers.
type DailyRecapData struct {
	// DayStart is 00:00 UTC of the day. DayEnd is the start of the next day.
	DayStart int64 `json:"day_start"`
	DayEnd   int64 `json:"day_end"`
	// Voice plus stream minutes. Game time is separate, as in the weekly recap.
	TotalHangoutMin int `json:"total_hangout_min"`
	TotalGameMin    int `json:"total_game_min"`
	// TotalMin is TotalHangoutMin + TotalGameMin: the figure the card leads with.
	TotalMin int `json:"total_min"`
	// Voice, stream and game sessions with reportable minutes.
	SessionCount int `json:"session_count"`
	ClipCount    int `json:"clip_count"`
	// TopGame is the game with the most reportable minutes that day. GameID is
	// its games.json id when the sessions carry one.
	TopGame string `json:"top_game,omitempty"`
	GameID  string `json:"game_id,omitempty"`
	// MemberIDs are the members who took part that day, the most active first.
	MemberIDs []string `json:"member_ids"`
	// BeltsMoved are the belts that changed hands that day, newest first, at
	// most feedDailyRecapBelts.
	BeltsMoved []RecapBeltMoved `json:"belts_moved,omitempty"`
}

// dailyRecapHasContent is the empty-card rule of a daily recap: minutes or a
// clip. A belt alone is not content, as in the weekly recap: holding titles
// must not turn an empty day into a card.
func dailyRecapHasContent(d DailyRecapData) bool {
	return d.TotalMin > 0 || d.TotalHangoutMin > 0 || d.TotalGameMin > 0 || d.ClipCount > 0
}

// utcDayStart is 00:00 UTC of the day that holds ms.
func utcDayStart(ms int64) int64 {
	t := time.UnixMilli(ms).UTC()
	return time.Date(t.Year(), t.Month(), t.Day(), 0, 0, 0, 0, time.UTC).UnixMilli()
}

// dailyRecapAcc collects one day while the cards are grouped.
type dailyRecapAcc struct {
	data      DailyRecapData
	lastTs    int64
	gameMin   map[string]int
	gameIDs   map[string]string
	memberHit map[string]int
}

func (a *dailyRecapAcc) touch(ids ...string) {
	for _, id := range ids {
		if id != "" {
			a.memberHit[id]++
		}
	}
}

// buildDailyRecaps groups the cards of the last feedDailyRecapDays days by UTC
// day and returns one entry for each day that has content, newest day first.
// A day with no minutes and no clips has no entry. belts are the belts moved
// (any age): each one joins the day of its MovedAtMs, newest first.
//
// cards must already have passed the empty-card rule, so a session with no
// reportable minutes is not counted. The cards are read, not changed. Pure.
func buildDailyRecaps(cards []feedCard, belts []RecapBeltMoved, nowMs int64) []FeedEntry {
	today := utcDayStart(nowMs)
	days := make([]*dailyRecapAcc, feedDailyRecapDays)
	for i := range days {
		start := today - int64(i)*dayMs
		days[i] = &dailyRecapAcc{
			data:      DailyRecapData{DayStart: start, DayEnd: start + dayMs, MemberIDs: []string{}},
			gameMin:   map[string]int{},
			gameIDs:   map[string]string{},
			memberHit: map[string]int{},
		}
	}
	dayOf := func(ts int64) *dailyRecapAcc {
		if ts > nowMs {
			return nil
		}
		i := int((today - utcDayStart(ts)) / dayMs)
		if i < 0 || i >= feedDailyRecapDays {
			return nil
		}
		return days[i]
	}

	for _, c := range cards {
		a := dayOf(c.ts)
		if a == nil {
			continue
		}
		counted := false
		switch {
		case c.feedType == "clip":
			a.data.ClipCount++
			if s, ok := decodeCardData[StoredClip](c.data); ok {
				a.touch(s.ActorID)
				a.touch(s.Participants...)
			}
			counted = true
		case c.backendType == "voice_session":
			d, _ := decodeCardData[VoiceSessionData](c.data)
			if d.DurationMin <= 0 {
				continue
			}
			a.data.TotalHangoutMin += d.DurationMin
			a.data.SessionCount++
			a.touch(d.ParticipantIDs...)
			counted = true
		case c.backendType == "stream_session":
			d, _ := decodeCardData[StreamSessionData](c.data)
			if d.DurationMin <= 0 {
				continue
			}
			a.data.TotalHangoutMin += d.DurationMin
			a.data.SessionCount++
			a.touch(d.StreamerID)
			a.touch(d.ViewerIDs...)
			counted = true
		case c.backendType == "game_session":
			d, _ := decodeCardData[GameSessionData](c.data)
			min := reportableMinutes(d.DurationMin, d.ActiveMin)
			if min <= 0 {
				continue
			}
			a.data.TotalGameMin += min
			a.data.SessionCount++
			if d.GameName != "" {
				a.gameMin[d.GameName] += min
				if d.GameID != "" {
					a.gameIDs[d.GameName] = d.GameID
				}
			}
			a.touch(d.PlayerIDs...)
			counted = true
		}
		if counted && c.ts > a.lastTs {
			a.lastTs = c.ts
		}
	}

	sorted := append([]RecapBeltMoved(nil), belts...)
	sort.SliceStable(sorted, func(i, j int) bool { return sorted[i].MovedAtMs > sorted[j].MovedAtMs })
	for _, b := range sorted {
		a := dayOf(b.MovedAtMs)
		if a == nil || len(a.data.BeltsMoved) >= feedDailyRecapBelts {
			continue
		}
		a.data.BeltsMoved = append(a.data.BeltsMoved, b)
	}

	entries := make([]FeedEntry, 0, feedDailyRecapDays)
	for _, a := range days {
		a.data.TotalMin = a.data.TotalHangoutMin + a.data.TotalGameMin
		if !dailyRecapHasContent(a.data) {
			continue
		}
		a.data.TopGame, a.data.GameID = topDailyGame(a.gameMin, a.gameIDs)
		a.data.MemberIDs = topDailyMembers(a.memberHit, feedDailyRecapMembers)
		ts := a.lastTs
		if ts == 0 {
			ts = a.data.DayStart
		}
		entries = append(entries, FeedEntry{
			ID:   dailyRecapID(a.data.DayStart),
			Type: "daily-recap",
			Role: "standard",
			Size: "md",
			Ts:   ts,
			Data: a.data,
		})
	}
	return entries
}

func dailyRecapID(dayStart int64) string {
	return "daily_" + time.UnixMilli(dayStart).UTC().Format("20060102")
}

// topDailyGame is the game with the most minutes. A tie goes to the name that
// sorts first, so the card does not change between two loads.
func topDailyGame(minutes map[string]int, ids map[string]string) (string, string) {
	best, bestMin := "", 0
	for name, m := range minutes {
		if m > bestMin || (m == bestMin && m > 0 && name < best) {
			best, bestMin = name, m
		}
	}
	return best, ids[best]
}

// topDailyMembers orders the members by how many sessions and clips they took
// part in, the most first, ties by id. At most limit.
func topDailyMembers(hits map[string]int, limit int) []string {
	ids := make([]string, 0, len(hits))
	for id := range hits {
		ids = append(ids, id)
	}
	sort.Slice(ids, func(i, j int) bool {
		if hits[ids[i]] != hits[ids[j]] {
			return hits[ids[i]] > hits[ids[j]]
		}
		return ids[i] < ids[j]
	})
	if len(ids) > limit {
		ids = ids[:limit]
	}
	return ids
}
