package main

import (
	"context"
	"sync"
	"testing"
)

type sentNote struct {
	userID  string
	code    int
	content map[string]interface{}
}

var (
	feedNotesMu sync.Mutex
	feedNotes   []sentNote
)

func (f *fakeGuestNk) NotificationSend(_ context.Context, userID, _ string, content map[string]interface{}, code int, _ string, _ bool) error {
	feedNotesMu.Lock()
	feedNotes = append(feedNotes, sentNote{userID, code, content})
	feedNotesMu.Unlock()
	return nil
}

func feedChangedTo(userID string) []sentNote {
	feedNotesMu.Lock()
	defer feedNotesMu.Unlock()
	var out []sentNote
	for _, n := range feedNotes {
		if n.userID == userID && n.code == NotifyFeedChanged {
			out = append(out, n)
		}
	}
	return out
}

// openCrew makes the user a subscriber with the crew open, as the client does.
func openCrew(userID, crewID string) {
	sub := getOrCreateSubscription(userID)
	subscriptionsMu.Lock()
	sub.ActiveCrew = crewID
	subscriptionsMu.Unlock()
	addCrewSubscriber(crewID, userID)
}

func feedChangedFixture(crew string) *fakeGuestNk {
	nk := newFakeGuestNk()
	nk.addMember(crew, "fc-actor", "actor")
	nk.addMember(crew, "fc-mate", "mate")
	openCrew("fc-actor", crew)
	openCrew("fc-mate", crew)
	feedNotesMu.Lock()
	feedNotes = nil
	feedNotesMu.Unlock()
	return nk
}

func TestGameSessionEndTellsTheCrewMatesTheFeedChanged(t *testing.T) {
	nk := feedChangedFixture("fc-crew-1")
	payload := `{"crew_id":"fc-crew-1","game_name":"G","game_id":"steam-1","duration_min":30,"active_min":30}`
	if _, err := GameSessionEndRPC(ctxWithUser("fc-actor"), testLogger(), nil, nk, payload); err != nil {
		t.Fatal(err)
	}
	got := feedChangedTo("fc-mate")
	if len(got) != 1 || got[0].content["crew_id"] != "fc-crew-1" || got[0].content["reason"] != "game_session" {
		t.Fatalf("mate got %+v", got)
	}
	if n := feedChangedTo("fc-actor"); len(n) != 0 {
		t.Fatalf("the actor reloads on its own, got %+v", n)
	}
}

func TestLibraryReportTellsTheCrewMatesOnlyWhenItChangedSomething(t *testing.T) {
	nk := feedChangedFixture("fc-crew-2")
	report := func(p string) {
		t.Helper()
		if _, err := LibraryReportRPC(ctxWithUser("fc-actor"), testLogger(), nil, nk, p); err != nil {
			t.Fatal(err)
		}
	}
	p := `{"games":[{"game_id":"steam-1","name":"A","launcher":"steam","playtime_min":60}]}`
	report(p)
	if n := len(feedChangedTo("fc-mate")); n != 1 {
		t.Fatalf("first report: %d notifications", n)
	}
	report(p) // the client repeats its report: nothing changed
	if n := len(feedChangedTo("fc-mate")); n != 1 {
		t.Fatalf("a repeated report must not notify, got %d", n)
	}
	report(`{"games":[{"game_id":"steam-1","name":"A","launcher":"steam","playtime_min":90}]}`)
	if n := len(feedChangedTo("fc-mate")); n != 2 {
		t.Fatalf("a changed playtime must notify, got %d", n)
	}
}

func TestPostClipTellsTheCrewMatesTheFeedChanged(t *testing.T) {
	nk := feedChangedFixture("fc-crew-3")
	payload := `{"crew_id":"fc-crew-3","clip_id":"c1","clip_type":"voice","duration_seconds":10}`
	if _, err := PostClipRPC(ctxWithUser("fc-actor"), testLogger(), nil, nk, payload); err != nil {
		t.Fatal(err)
	}
	got := feedChangedTo("fc-mate")
	if len(got) != 1 || got[0].content["reason"] != "clip" {
		t.Fatalf("mate got %+v", got)
	}
}

func TestFeedChangedSkipsMembersWhoDoNotHaveTheCrewOpen(t *testing.T) {
	nk := feedChangedFixture("fc-crew-4")
	openCrew("fc-mate", "some-other-crew")
	PushFeedChanged(context.Background(), testLogger(), nk, "fc-crew-4", "fc-actor", "test")
	if n := feedChangedTo("fc-mate"); len(n) != 0 {
		t.Fatalf("got %+v", n)
	}
}
