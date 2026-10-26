package main

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/heroiclabs/nakama-common/runtime"
)

func libCtx(userID string) context.Context {
	return context.WithValue(context.Background(), runtime.RUNTIME_CTX_USER_ID, userID)
}

func libReport(t *testing.T, nk runtime.NakamaModule, userID, payload string) error {
	t.Helper()
	_, err := LibraryReportRPC(libCtx(userID), testLogger(), nil, nk, payload)
	return err
}

func libGames(n int) string {
	games := make([]string, 0, n)
	for i := 0; i < n; i++ {
		games = append(games, fmt.Sprintf(`{"game_id":"steam-%d","name":"Game %d","launcher":"steam"}`, i+1, i+1))
	}
	return `{"games":[` + strings.Join(games, ",") + `]}`
}

func storedLibrary(t *testing.T, nk *fakeGuestNk, userID string) LibraryDoc {
	t.Helper()
	docs := readLibraryDocs(context.Background(), nk, []string{userID})
	d := docs[userID]
	if d == nil {
		t.Fatalf("no library doc for %s", userID)
	}
	return *d
}

func TestLibraryReport_StoresGamesWithOptionalFigures(t *testing.T) {
	nk := newFakeGuestNk()
	err := libReport(t, nk, "u1", `{"games":[
		{"game_id":"steam-730","name":"Counter-Strike 2","launcher":"steam","playtime_min":249008,"last_played_unix":1760000000},
		{"game_id":"epic-fortnite","name":"Fortnite","launcher":"epic"}]}`)
	if err != nil {
		t.Fatalf("report: %v", err)
	}
	d := storedLibrary(t, nk, "u1")
	if len(d.Games) != 2 || d.UpdatedAt == 0 {
		t.Fatalf("doc = %+v", d)
	}
	if d.Games[0].PlaytimeMin == nil || *d.Games[0].PlaytimeMin != 249008 || *d.Games[0].LastPlayedUnix != 1760000000 {
		t.Fatalf("figures lost: %+v", d.Games[0])
	}
	if d.Games[1].PlaytimeMin != nil || d.Games[1].LastPlayedUnix != nil {
		t.Fatalf("absent figures must stay absent: %+v", d.Games[1])
	}
}

func TestLibraryReport_ReplacesNotAppends(t *testing.T) {
	nk := newFakeGuestNk()
	if err := libReport(t, nk, "u1", libGames(5)); err != nil {
		t.Fatal(err)
	}
	if err := libReport(t, nk, "u1", `{"games":[{"game_id":"gog-77","name":"Witcher","launcher":"gog"}]}`); err != nil {
		t.Fatal(err)
	}
	d := storedLibrary(t, nk, "u1")
	if len(d.Games) != 1 || d.Games[0].GameID != "gog-77" {
		t.Fatalf("second report must replace the first: %+v", d.Games)
	}
	// Another user is untouched.
	if err := libReport(t, nk, "u2", libGames(2)); err != nil {
		t.Fatal(err)
	}
	if len(storedLibrary(t, nk, "u1").Games) != 1 {
		t.Fatal("a report must only touch the caller's document")
	}
}

func TestLibraryReport_EmptyReportClearsTheDoc(t *testing.T) {
	nk := newFakeGuestNk()
	if err := libReport(t, nk, "u1", libGames(3)); err != nil {
		t.Fatal(err)
	}
	if err := libReport(t, nk, "u1", `{"games":[]}`); err != nil {
		t.Fatalf("empty report: %v", err)
	}
	if got := len(storedLibrary(t, nk, "u1").Games); got != 0 {
		t.Fatalf("games after an empty report = %d, want 0", got)
	}
}

func TestLibraryReport_CapsAt500(t *testing.T) {
	nk := newFakeGuestNk()
	if err := libReport(t, nk, "u1", libGames(500)); err != nil {
		t.Fatalf("500 entries must pass: %v", err)
	}
	err := libReport(t, nk, "u1", libGames(501))
	if errCode(err) != 3 {
		t.Fatalf("501 entries: err = %v, want code 3", err)
	}
	if got := len(storedLibrary(t, nk, "u1").Games); got != 500 {
		t.Fatalf("a rejected report must keep the old doc, got %d games", got)
	}
}

func TestLibraryReport_DropsHiddenEntries(t *testing.T) {
	nk := newFakeGuestNk()
	err := libReport(t, nk, "u1", `{"games":[
		{"game_id":"steam-1","name":"Shown","launcher":"steam","hidden":false},
		{"game_id":"steam-2","name":"Secret","launcher":"steam","hidden":true},
		{"game_id":"steam-3","name":"Plain","launcher":"steam"}]}`)
	if err != nil {
		t.Fatal(err)
	}
	d := storedLibrary(t, nk, "u1")
	if len(d.Games) != 2 {
		t.Fatalf("games = %+v, want 2", d.Games)
	}
	for _, g := range d.Games {
		if g.GameID == "steam-2" {
			t.Fatal("a hidden game must never be stored")
		}
	}
	raw, _ := json.Marshal(d)
	if strings.Contains(string(raw), "hidden") {
		t.Fatal("the stored doc must not carry a hidden field")
	}
}

func TestLibraryReport_Validation(t *testing.T) {
	long := strings.Repeat("x", 129)
	cases := []struct {
		name, payload string
	}{
		{"not json", `nope`},
		{"missing game_id", `{"games":[{"name":"A","launcher":"steam"}]}`},
		{"game_id without prefix", `{"games":[{"game_id":"730","name":"A","launcher":"steam"}]}`},
		{"game_id without external id", `{"games":[{"game_id":"steam-","name":"A","launcher":"steam"}]}`},
		{"game_id with space", `{"games":[{"game_id":"steam-7 3","name":"A","launcher":"steam"}]}`},
		{"game_id too long", `{"games":[{"game_id":"steam-` + long + `","name":"A","launcher":"steam"}]}`},
		{"empty name", `{"games":[{"game_id":"steam-1","name":"  ","launcher":"steam"}]}`},
		{"name too long", `{"games":[{"game_id":"steam-1","name":"` + long + `","launcher":"steam"}]}`},
		{"missing launcher", `{"games":[{"game_id":"steam-1","name":"A"}]}`},
		{"launcher upper case", `{"games":[{"game_id":"steam-1","name":"A","launcher":"Steam"}]}`},
		{"negative playtime", `{"games":[{"game_id":"steam-1","name":"A","launcher":"steam","playtime_min":-1}]}`},
		{"negative last played", `{"games":[{"game_id":"steam-1","name":"A","launcher":"steam","last_played_unix":-5}]}`},
		{"payload too large", `{"games":[],"pad":"` + strings.Repeat("a", maxLibraryPayloadSize) + `"}`},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			nk := newFakeGuestNk()
			if err := libReport(t, nk, "u1", c.payload); errCode(err) != 3 {
				t.Fatalf("err = %v, want code 3", err)
			}
			if len(readLibraryDocs(context.Background(), nk, []string{"u1"})) != 0 {
				t.Fatal("an invalid report must store nothing")
			}
		})
	}
}

func TestLibraryReport_RequiresAuthentication(t *testing.T) {
	nk := newFakeGuestNk()
	_, err := LibraryReportRPC(context.Background(), testLogger(), nil, nk, libGames(1))
	if errCode(err) != 16 {
		t.Fatalf("err = %v, want code 16", err)
	}
}

func TestLibraryReport_DuplicateIDsKeepTheFirst(t *testing.T) {
	nk := newFakeGuestNk()
	err := libReport(t, nk, "u1", `{"games":[
		{"game_id":"steam-1","name":"First","launcher":"steam"},
		{"game_id":"steam-1","name":"Second","launcher":"steam"}]}`)
	if err != nil {
		t.Fatal(err)
	}
	d := storedLibrary(t, nk, "u1")
	if len(d.Games) != 1 || d.Games[0].Name != "First" {
		t.Fatalf("games = %+v", d.Games)
	}
}

func TestNormalizeLibraryReportClampsPlaytime(t *testing.T) {
	over, ok := maxLibraryPlaytimeMin+1, maxLibraryPlaytimeMin
	games, clamped, err := normalizeLibraryReport(libraryReportRequest{Games: []libraryReportGame{
		{LibraryGame: LibraryGame{GameID: "steam-1", Name: "A", Launcher: "steam", PlaytimeMin: &over}},
		{LibraryGame: LibraryGame{GameID: "steam-2", Name: "B", Launcher: "steam", PlaytimeMin: &ok}},
	}})
	if err != nil || clamped != 1 || len(games) != 2 {
		t.Fatalf("err=%v clamped=%d games=%d", err, clamped, len(games))
	}
	if *games[0].PlaytimeMin != maxLibraryPlaytimeMin || *games[1].PlaytimeMin != maxLibraryPlaytimeMin {
		t.Fatalf("playtimes %d %d, want cap", *games[0].PlaytimeMin, *games[1].PlaytimeMin)
	}
	if maxLibraryPlaytimeMin != 7_200_000 {
		t.Fatalf("cap = %d, want 7,200,000 min", maxLibraryPlaytimeMin)
	}
}
