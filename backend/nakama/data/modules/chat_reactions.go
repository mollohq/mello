package main

// Chat reactions, server side. One storage object per message
// (`chat_reactions/{message_id}`, system-owned, readable by all) holds
// emoji -> user ids. chat_react adds or removes the caller, then every crew
// member gets the message's new state as a notification, so clients only
// replace. chat_reactions reads the objects of the loaded window in one go.

import (
	"context"
	"database/sql"
	"encoding/json"
	"sort"
	"unicode"

	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	ChatReactionsCollection = "chat_reactions"
	// maxReactionEmojiBytes bounds one emoji sequence (a family with skin
	// tones is 28 bytes).
	maxReactionEmojiBytes = 40
	// maxReactionsPerMessage bounds distinct emoji on one message.
	maxReactionsPerMessage = 20
	// maxReactionReads bounds one chat_reactions batch.
	maxReactionReads = 250
)

// ReactionsDoc is the stored state of one message's reactions.
type ReactionsDoc struct {
	CrewID string `json:"crew_id"`
	// Emoji -> user ids, in the order they reacted.
	Reactions map[string][]string `json:"reactions"`
}

// ReactionUpdate is the notification every crew member gets after a change.
type ReactionUpdate struct {
	CrewID    string              `json:"crew_id"`
	MessageID string              `json:"message_id"`
	Reactions map[string][]string `json:"reactions"`
}

// validReactionEmoji refuses text: anything with a letter, a digit or a
// space, anything empty, anything long. It does not try to know every emoji.
func validReactionEmoji(emoji string) bool {
	if emoji == "" || len(emoji) > maxReactionEmojiBytes {
		return false
	}
	for _, r := range emoji {
		if unicode.IsLetter(r) || unicode.IsDigit(r) || unicode.IsSpace(r) || unicode.IsControl(r) {
			// Keycap bases ("1️⃣") start with a digit; allow them only as
			// a full keycap sequence.
			if unicode.IsDigit(r) && len(emoji) > 1 && containsKeycap(emoji) {
				continue
			}
			return false
		}
	}
	return true
}

func containsKeycap(s string) bool {
	for _, r := range s {
		if r == 0x20E3 {
			return true
		}
	}
	return false
}

// applyReaction adds or removes `userID` under `emoji`. It returns the new
// map and whether anything changed. An emoji with no users left is dropped.
// A new emoji past the cap is refused (changed=false).
func applyReaction(reactions map[string][]string, emoji, userID string, add bool) (map[string][]string, bool) {
	if reactions == nil {
		reactions = map[string][]string{}
	}
	users := reactions[emoji]
	idx := -1
	for i, u := range users {
		if u == userID {
			idx = i
			break
		}
	}
	if add {
		if idx >= 0 {
			return reactions, false
		}
		if _, exists := reactions[emoji]; !exists && len(reactions) >= maxReactionsPerMessage {
			return reactions, false
		}
		reactions[emoji] = append(users, userID)
		return reactions, true
	}
	if idx < 0 {
		return reactions, false
	}
	users = append(users[:idx], users[idx+1:]...)
	if len(users) == 0 {
		delete(reactions, emoji)
	} else {
		reactions[emoji] = users
	}
	return reactions, true
}

func readReactionsDoc(ctx context.Context, nk runtime.NakamaModule, messageID string) (*ReactionsDoc, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: ChatReactionsCollection, Key: messageID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return &ReactionsDoc{Reactions: map[string][]string{}}, ""
	}
	var doc ReactionsDoc
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &doc); err != nil {
		return &ReactionsDoc{Reactions: map[string][]string{}}, ""
	}
	if doc.Reactions == nil {
		doc.Reactions = map[string][]string{}
	}
	return &doc, objects[0].GetVersion()
}

func writeReactionsDoc(ctx context.Context, nk runtime.NakamaModule, messageID string, doc *ReactionsDoc, version string) error {
	data, err := json.Marshal(doc)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      ChatReactionsCollection,
			Key:             messageID,
			UserID:          SystemUserID,
			Value:           string(data),
			Version:         version,
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})
	return err
}

// ---------------------------------------------------------------------------
// chat_react RPC
// ---------------------------------------------------------------------------

type ChatReactRequest struct {
	CrewID    string `json:"crew_id"`
	MessageID string `json:"message_id"`
	Emoji     string `json:"emoji"`
	// "add" or "remove".
	Op string `json:"op"`
}

func ChatReactRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req ChatReactRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.MessageID == "" {
		return "", runtime.NewError("crew_id and message_id required", 3)
	}
	if req.Op != "add" && req.Op != "remove" {
		return "", runtime.NewError("op must be add or remove", 3)
	}
	if !validReactionEmoji(req.Emoji) {
		return "", runtime.NewError("not an emoji", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	var doc *ReactionsDoc
	for attempt := 0; attempt < 3; attempt++ {
		var version string
		doc, version = readReactionsDoc(ctx, nk, req.MessageID)
		if doc.CrewID != "" && doc.CrewID != req.CrewID {
			return "", runtime.NewError("message is not in this crew", 7)
		}
		doc.CrewID = req.CrewID
		changed := false
		doc.Reactions, changed = applyReaction(doc.Reactions, req.Emoji, userID, req.Op == "add")
		if !changed {
			break
		}
		if err := writeReactionsDoc(ctx, nk, req.MessageID, doc, version); err == nil {
			break
		} else if attempt == 2 {
			logger.Error("chat_react: write failed: %v", err)
			return "", runtime.NewError("failed to store reaction", 13)
		}
	}

	update := ReactionUpdate{CrewID: req.CrewID, MessageID: req.MessageID, Reactions: doc.Reactions}
	content := map[string]interface{}{
		"crew_id":    update.CrewID,
		"message_id": update.MessageID,
		"reactions":  update.Reactions,
	}
	members, _, err := nk.GroupUsersList(ctx, req.CrewID, 100, nil, "")
	if err != nil {
		logger.Warn("chat_react: members of %s: %v", req.CrewID, err)
	}
	for _, m := range members {
		if m.GetUser() == nil {
			continue
		}
		pushNotification(ctx, nk, m.GetUser().GetId(), NotifyChatReaction, content)
	}

	data, _ := json.Marshal(update)
	return string(data), nil
}

// ---------------------------------------------------------------------------
// chat_reactions RPC: the reactions of many messages at once
// ---------------------------------------------------------------------------

type ChatReactionsRequest struct {
	CrewID     string   `json:"crew_id"`
	MessageIDs []string `json:"message_ids"`
}

type ChatReactionsResponse struct {
	Items []ReactionUpdate `json:"items"`
}

func ChatReactionsRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req ChatReactionsRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	if len(req.MessageIDs) == 0 || len(req.MessageIDs) > maxReactionReads {
		return "", runtime.NewError("message_ids required (1 to 250)", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	reads := make([]*runtime.StorageRead, 0, len(req.MessageIDs))
	for _, id := range req.MessageIDs {
		if id == "" {
			continue
		}
		reads = append(reads, &runtime.StorageRead{Collection: ChatReactionsCollection, Key: id, UserID: SystemUserID})
	}
	objects, err := nk.StorageRead(ctx, reads)
	if err != nil {
		logger.Error("chat_reactions: read failed: %v", err)
		return "", runtime.NewError("failed to read reactions", 13)
	}
	resp := ChatReactionsResponse{Items: []ReactionUpdate{}}
	for _, o := range objects {
		var doc ReactionsDoc
		if err := json.Unmarshal([]byte(o.GetValue()), &doc); err != nil || doc.CrewID != req.CrewID {
			continue
		}
		if len(doc.Reactions) == 0 {
			continue
		}
		resp.Items = append(resp.Items, ReactionUpdate{CrewID: doc.CrewID, MessageID: o.GetKey(), Reactions: doc.Reactions})
	}
	sort.Slice(resp.Items, func(i, j int) bool { return resp.Items[i].MessageID < resp.Items[j].MessageID })
	data, _ := json.Marshal(resp)
	return string(data), nil
}
