package main

import "testing"

func TestParseRiotID(t *testing.T) {
	cases := []struct {
		in        string
		game, tag string
		ok        bool
	}{
		{"Mello#EUW", "Mello", "EUW", true},
		{"  Spaced Name # 123 ", "Spaced Name", "123", true},
		{"NoTag", "", "", false},
		{"#OnlyTag", "", "", false},
		{"Name#", "", "", false},
		{"", "", "", false},
		{"WayTooLongGameName17#EUW", "", "", false},
		{"Mello#TOOLONG", "", "", false},
	}
	for _, c := range cases {
		game, tag, ok := parseRiotID(c.in)
		if game != c.game || tag != c.tag || ok != c.ok {
			t.Fatalf("parseRiotID(%q) = (%q, %q, %v), want (%q, %q, %v)",
				c.in, game, tag, ok, c.game, c.tag, c.ok)
		}
	}
}

func TestLolMatchOutcome(t *testing.T) {
	win := []byte(`{"info":{"gameDuration":1900,"participants":[
		{"puuid":"me","win":true},{"puuid":"foe","win":false}]}}`)
	loss := []byte(`{"info":{"gameDuration":1900,"participants":[
		{"puuid":"me","win":false},{"puuid":"foe","win":true}]}}`)
	remake := []byte(`{"info":{"gameDuration":180,"participants":[
		{"puuid":"me","win":false}]}}`)
	notInMatch := []byte(`{"info":{"gameDuration":1900,"participants":[
		{"puuid":"foe","win":true}]}}`)

	if w, c := lolMatchOutcome(win, "me"); !w || !c {
		t.Fatalf("win match: got (%v, %v)", w, c)
	}
	if w, c := lolMatchOutcome(loss, "me"); w || !c {
		t.Fatalf("loss match: got (%v, %v)", w, c)
	}
	if _, c := lolMatchOutcome(remake, "me"); c {
		t.Fatal("remake should not count")
	}
	if _, c := lolMatchOutcome(notInMatch, "me"); c {
		t.Fatal("match without the player should not count")
	}
	if _, c := lolMatchOutcome([]byte("garbage"), "me"); c {
		t.Fatal("garbage should not count")
	}
}

func TestTftMatchOutcome(t *testing.T) {
	top4 := []byte(`{"info":{"participants":[
		{"puuid":"me","placement":3},{"puuid":"foe","placement":1}]}}`)
	bottom4 := []byte(`{"info":{"participants":[
		{"puuid":"me","placement":7}]}}`)
	missing := []byte(`{"info":{"participants":[{"puuid":"foe","placement":1}]}}`)

	if w, c := tftMatchOutcome(top4, "me"); !w || !c {
		t.Fatalf("top-4: got (%v, %v)", w, c)
	}
	if w, c := tftMatchOutcome(bottom4, "me"); w || !c {
		t.Fatalf("bottom-4: got (%v, %v)", w, c)
	}
	if _, c := tftMatchOutcome(missing, "me"); c {
		t.Fatal("match without the player should not count")
	}
}
