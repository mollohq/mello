package main

import (
	"reflect"
	"testing"
	"time"
)

// Thursday 22 October 2026, 15:30 UTC.
var dailyNow = time.Date(2026, time.October, 22, 15, 30, 0, 0, time.UTC).UnixMilli()

func dailyAt(daysAgo int, hour int) int64 {
	d := time.UnixMilli(dailyNow).UTC().AddDate(0, 0, -daysAgo)
	return time.Date(d.Year(), d.Month(), d.Day(), hour, 0, 0, 0, time.UTC).UnixMilli()
}

func voiceAt(id string, ts int64, min int, who ...string) feedCard {
	return feedCard{id: id, feedType: "session", backendType: "voice_session", ts: ts,
		data: VoiceSessionData{DurationMin: min, ParticipantIDs: who}}
}

func gameAt(id string, ts int64, game, gameID string, min int, who ...string) feedCard {
	return feedCard{id: id, feedType: "session", backendType: "game_session", ts: ts,
		data: GameSessionData{GameName: game, GameID: gameID, DurationMin: min, PlayerIDs: who}}
}

func clipAt(id string, ts int64, actor string) feedCard {
	return feedCard{id: id, feedType: "clip", backendType: "clip", ts: ts,
		data: StoredClip{EventID: id, ActorID: actor, Ts: ts}}
}

func dailyData(t *testing.T, e FeedEntry) DailyRecapData {
	t.Helper()
	d, ok := e.Data.(DailyRecapData)
	if !ok {
		t.Fatalf("entry %q data is %T", e.ID, e.Data)
	}
	return d
}

// Each day of the last three has its own entry, newest day first, with its own
// sums. An event goes to the UTC day of its timestamp.
func TestDailyRecapsGroupByUTCDay(t *testing.T) {
	cards := []feedCard{
		voiceAt("v-today", dailyAt(0, 9), 30, "u1", "u2"),
		gameAt("g-today", dailyAt(0, 11), "Rocket League", "rocket-league", 60, "u1"),
		clipAt("c-today", dailyAt(0, 12), "u2"),
		voiceAt("v-yday", dailyAt(1, 23), 20, "u3", "u4", "u5"),
		// 00:00 sharp belongs to the day that starts, not the one that ends.
		voiceAt("v-edge", dailyAt(1, 0), 10, "u3"),
		gameAt("g-2d", dailyAt(2, 20), "Valorant", "valorant", 45, "u1", "u3", "u4"),
		voiceAt("v-3d", dailyAt(3, 12), 99, "u9"),
	}
	got := buildDailyRecaps(cards, nil, dailyNow)
	if len(got) != 3 {
		t.Fatalf("want 3 days, got %d", len(got))
	}
	ids := []string{got[0].ID, got[1].ID, got[2].ID}
	if want := []string{"daily_20261022", "daily_20261021", "daily_20261020"}; !reflect.DeepEqual(ids, want) {
		t.Fatalf("ids %v, want %v", ids, want)
	}
	today := dailyData(t, got[0])
	if today.TotalHangoutMin != 30 || today.TotalGameMin != 60 || today.TotalMin != 90 {
		t.Fatalf("today minutes %+v", today)
	}
	if today.SessionCount != 2 || today.ClipCount != 1 {
		t.Fatalf("today counts %+v", today)
	}
	if today.TopGame != "Rocket League" || today.GameID != "rocket-league" {
		t.Fatalf("today top game %q (%q)", today.TopGame, today.GameID)
	}
	if want := []string{"u1", "u2"}; !reflect.DeepEqual(today.MemberIDs, want) {
		// u1 took part twice, u2 twice (voice and clip): ties go by id.
		t.Fatalf("today members %v, want %v", today.MemberIDs, want)
	}
	yday := dailyData(t, got[1])
	if yday.TotalMin != 30 || yday.SessionCount != 2 {
		t.Fatalf("yesterday %+v", yday)
	}
	if yday.DayStart != dailyAt(1, 0) || yday.DayEnd != dailyAt(0, 0) {
		t.Fatalf("yesterday bounds %d..%d", yday.DayStart, yday.DayEnd)
	}
	if dailyData(t, got[2]).TopGame != "Valorant" {
		t.Fatalf("day before: %+v", dailyData(t, got[2]))
	}
}

// A day with nothing has no card. The days around it keep theirs.
func TestDailyRecapSkipsAnEmptyDay(t *testing.T) {
	cards := []feedCard{
		voiceAt("v-today", dailyAt(0, 9), 30, "u1", "u2", "u3"),
		voiceAt("v-2d", dailyAt(2, 9), 15, "u1", "u2", "u3"),
	}
	got := buildDailyRecaps(cards, nil, dailyNow)
	if len(got) != 2 {
		t.Fatalf("want 2 entries, got %d", len(got))
	}
	if got[0].ID != "daily_20261022" || got[1].ID != "daily_20261020" {
		t.Fatalf("ids %q %q", got[0].ID, got[1].ID)
	}
	if n := len(buildDailyRecaps(nil, nil, dailyNow)); n != 0 {
		t.Fatalf("no cards, want no entries, got %d", n)
	}
}

// Zero-minute sessions and a belt alone do not make a day. A clip alone does.
func TestDailyRecapNeedsMinutesOrAClip(t *testing.T) {
	belt := RecapBeltMoved{TitleID: "the_carry", HolderUserID: "u1", MovedAtMs: dailyAt(0, 10)}
	cards := []feedCard{
		voiceAt("v0", dailyAt(0, 9), 0, "u1"),
		gameAt("g0", dailyAt(0, 9), "Valorant", "valorant", 0, "u1"),
		clipAt("c-yday", dailyAt(1, 9), "u2"),
	}
	got := buildDailyRecaps(cards, []RecapBeltMoved{belt}, dailyNow)
	if len(got) != 1 || got[0].ID != "daily_20261021" {
		t.Fatalf("want only yesterday, got %+v", got)
	}
	d := dailyData(t, got[0])
	if d.TotalMin != 0 || d.ClipCount != 1 || d.SessionCount != 0 {
		t.Fatalf("clip-only day %+v", d)
	}
}

// Belts that moved on a day join that day, newest first, two at most.
func TestDailyRecapBeltsAreTheDaysAndAreCapped(t *testing.T) {
	cards := []feedCard{
		voiceAt("v-today", dailyAt(0, 9), 30, "u1"),
		voiceAt("v-yday", dailyAt(1, 9), 30, "u1"),
	}
	belts := []RecapBeltMoved{
		{TitleID: "the_carry", MovedAtMs: dailyAt(0, 8)},
		{TitleID: "night_owl", MovedAtMs: dailyAt(0, 10)},
		{TitleID: "marathon", MovedAtMs: dailyAt(0, 12)},
		{TitleID: "clipper", MovedAtMs: dailyAt(1, 12)},
		{TitleID: "mouth", MovedAtMs: dailyAt(5, 12)},
	}
	got := buildDailyRecaps(cards, belts, dailyNow)
	today, yday := dailyData(t, got[0]), dailyData(t, got[1])
	var titles []string
	for _, b := range today.BeltsMoved {
		titles = append(titles, b.TitleID)
	}
	if want := []string{"marathon", "night_owl"}; !reflect.DeepEqual(titles, want) {
		t.Fatalf("today belts %v, want %v", titles, want)
	}
	if len(yday.BeltsMoved) != 1 || yday.BeltsMoved[0].TitleID != "clipper" {
		t.Fatalf("yesterday belts %+v", yday.BeltsMoved)
	}
}

// The daily recaps sit after the recap and the head2head entry and before the
// sessions, as standard cells. Old catch-up events add no card.
func TestDailyRecapsSitAfterTheRecapBeforeSessions(t *testing.T) {
	cards := []feedCard{
		recapAt("recap_1", dailyAt(0, 1)),
		voiceAt("v-today", dailyAt(0, 9), 30, "u1"),
		gameAt("g-today", dailyAt(0, 10), "Rocket League", "rocket-league", 60, "u1", "u2", "u3"),
	}
	cards[0].data = WeeklyRecapData{TotalHangoutMin: 90, ClipCount: 1}
	entries := buildThisWeek(cards, dailyNow)
	var order []string
	for _, e := range entries {
		order = append(order, e.Type)
	}
	if len(order) < 3 || order[0] != "recap" || order[1] != "daily-recap" {
		t.Fatalf("order %v, want recap then daily-recap first", order)
	}
	if entries[1].Role != "standard" || entries[1].Size != "md" {
		t.Fatalf("daily recap role/size %q/%q", entries[1].Role, entries[1].Size)
	}
	for _, e := range entries[2:] {
		if e.Type == "daily-recap" {
			t.Fatalf("a second daily recap with one active day: %v", order)
		}
	}
}

func TestDailyRecapTopGameTiesGoByName(t *testing.T) {
	cards := []feedCard{
		gameAt("g1", dailyAt(0, 9), "Valorant", "valorant", 30, "u1", "u2", "u3"),
		gameAt("g2", dailyAt(0, 10), "Fall Guys", "fall-guys", 30, "u1"),
	}
	d := dailyData(t, buildDailyRecaps(cards, nil, dailyNow)[0])
	if d.TopGame != "Fall Guys" || d.GameID != "fall-guys" {
		t.Fatalf("top game %q (%q)", d.TopGame, d.GameID)
	}
}

// A day shows only when it adds something to the weekly recap: a clip, a belt
// that moved that day, or three or more active members. Two members with
// minutes and nothing else is a repeat.
func TestDailyRecapOnlyWhenItAddsSomething(t *testing.T) {
	belt := RecapBeltMoved{TitleID: "the_carry", HolderUserID: "u1", MovedAtMs: dailyAt(1, 10)}
	cards := []feedCard{
		// Today: two members, minutes only. No entry.
		voiceAt("v-today", dailyAt(0, 9), 233, "u1", "u2"),
		// Yesterday: two members, a belt moved. Entry.
		voiceAt("v-yday", dailyAt(1, 9), 40, "u1", "u2"),
		// Two days ago: two members and a clip. Entry.
		voiceAt("v-2d", dailyAt(2, 9), 40, "u1", "u2"),
		clipAt("c-2d", dailyAt(2, 10), "u1"),
	}
	got := buildDailyRecaps(cards, []RecapBeltMoved{belt}, dailyNow)
	var ids []string
	for _, e := range got {
		ids = append(ids, e.ID)
	}
	if want := []string{"daily_20261021", "daily_20261020"}; !reflect.DeepEqual(ids, want) {
		t.Fatalf("ids %v, want %v", ids, want)
	}

	three := []feedCard{voiceAt("v3", dailyAt(0, 9), 30, "u1", "u2", "u3")}
	if n := len(buildDailyRecaps(three, nil, dailyNow)); n != 1 {
		t.Fatalf("three members, want an entry, got %d", n)
	}
	one := []feedCard{voiceAt("v1", dailyAt(0, 9), 30, "u1")}
	if n := len(buildDailyRecaps(one, nil, dailyNow)); n != 0 {
		t.Fatalf("one member, want no entry, got %d", n)
	}
}
