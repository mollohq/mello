package main

import "testing"

func TestIsValidStatus(t *testing.T) {
	valid := []string{StatusOnline, StatusIdle, StatusDoNotDisturb, StatusOffline}
	for _, s := range valid {
		if !IsValidStatus(s) {
			t.Errorf("expected %q to be valid", s)
		}
	}

	invalid := []string{"", "busy", "away", "ONLINE", "invisible"}
	for _, s := range invalid {
		if IsValidStatus(s) {
			t.Errorf("expected %q to be invalid", s)
		}
	}
}

func TestIsValidActivityType(t *testing.T) {
	valid := []string{ActivityNone, ActivityInVoice, ActivityStreaming, ActivityWatching}
	for _, a := range valid {
		if !IsValidActivityType(a) {
			t.Errorf("expected %q to be valid", a)
		}
	}

	invalid := []string{"", "gaming", "afk", "STREAMING"}
	for _, a := range invalid {
		if IsValidActivityType(a) {
			t.Errorf("expected %q to be invalid", a)
		}
	}
}
