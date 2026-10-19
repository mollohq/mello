package main

import (
	"context"
	"testing"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// noopLogger is a runtime.Logger that discards everything, so we can exercise
// code paths that log without wiring a real Nakama logger.
type noopLogger struct{}

func (noopLogger) Debug(format string, v ...interface{})                     {}
func (noopLogger) Info(format string, v ...interface{})                      {}
func (noopLogger) Warn(format string, v ...interface{})                      {}
func (noopLogger) Error(format string, v ...interface{})                     {}
func (l noopLogger) WithField(key string, v interface{}) runtime.Logger      { return l }
func (l noopLogger) WithFields(fields map[string]interface{}) runtime.Logger { return l }
func (noopLogger) Fields() map[string]interface{}                            { return nil }

func testLogger() runtime.Logger { return noopLogger{} }

// cleanupVoiceOnCrewExit must only evict a user when their CURRENT voice channel
// belongs to the crew they're leaving. These guard paths return before touching
// the Nakama module, so we can assert them without a full nk mock. The positive
// (eviction) path delegates to the well-covered voiceLeaveInternal and is
// exercised via the dev_fault RPC / soak harness instead.

func TestCleanupVoiceOnCrewExit_NotInVoice(t *testing.T) {
	resetVoiceState()
	// user_1 is in no voice channel -> no-op, nk untouched (pass nil safely).
	cleanupVoiceOnCrewExit(context.Background(), testLogger(), nil, "user_1", "crew_a")
	voiceUserChannelMu.RLock()
	_, present := voiceUserChannel["user_1"]
	voiceUserChannelMu.RUnlock()
	if present {
		t.Error("expected user to remain absent from voiceUserChannel")
	}
}

func TestCleanupVoiceOnCrewExit_DifferentCrew(t *testing.T) {
	resetVoiceState()

	// user_1 is in crew_b's channel; leaving crew_a must NOT evict them.
	channelID := "ch_b"
	voiceRoomsMu.Lock()
	voiceRooms[channelID] = &VoiceRoom{
		ChannelID: channelID,
		CrewID:    "crew_b",
		Members:   map[string]*VoiceMemberState{"user_1": {UserID: "user_1", Username: "alice"}},
	}
	voiceRoomsMu.Unlock()
	voiceUserChannelMu.Lock()
	voiceUserChannel["user_1"] = channelID
	voiceUserChannelMu.Unlock()
	voiceChannelCrewMu.Lock()
	voiceChannelCrew[channelID] = "crew_b"
	voiceChannelCrewMu.Unlock()

	cleanupVoiceOnCrewExit(context.Background(), testLogger(), nil, "user_1", "crew_a")

	voiceUserChannelMu.RLock()
	ch := voiceUserChannel["user_1"]
	voiceUserChannelMu.RUnlock()
	if ch != channelID {
		t.Errorf("user in a different crew's voice must not be evicted; got channel %q", ch)
	}
}

func TestDropNextVoicePush(t *testing.T) {
	crewID := "crew_drop_test"
	// Reset any prior state for this crew.
	voiceDropNextPushMu.Lock()
	delete(voiceDropNextPush, crewID)
	voiceDropNextPushMu.Unlock()

	if shouldDropVoicePush(crewID) {
		t.Fatal("nothing scheduled: should not drop")
	}

	DropNextVoicePush(crewID, 2)
	if !shouldDropVoicePush(crewID) {
		t.Error("expected first push to be dropped")
	}
	if !shouldDropVoicePush(crewID) {
		t.Error("expected second push to be dropped")
	}
	if shouldDropVoicePush(crewID) {
		t.Error("expected third push to pass through")
	}
}

func TestBackdateVoiceJoinMovesOneMember(t *testing.T) {
	seatVoice("crew", "ch", map[string]int{"a": 0, "b": 0})
	if !backdateVoiceJoin("a", 80*time.Minute) {
		t.Fatal("a is in a room")
	}
	if backdateVoiceJoin("nobody", time.Minute) {
		t.Fatal("a user outside voice must report false")
	}
	nk := newFakeGuestNk()
	voiceLeaveInternal(context.Background(), testLogger(), nk, "a")
	voiceLeaveInternal(context.Background(), testLogger(), nk, "b")
	if got := readActivity(t, nk, "a").VoiceMin; got != 80 {
		t.Fatalf("a voice_min = %d, want 80", got)
	}
	if _, ok := nk.storage[fakeStorageKey(UserActivityStatsCollection, userActivityStatsKey, "b")]; ok {
		t.Fatal("b stayed under a minute and adds nothing")
	}
}
