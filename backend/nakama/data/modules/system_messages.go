package main

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/heroiclabs/nakama-common/runtime"
)

// systemEnvelope is the structured envelope for system messages.
type systemEnvelope struct {
	V     int                    `json:"v"`
	Type  string                 `json:"type"`
	Body  string                 `json:"body"`
	Event string                 `json:"event,omitempty"`
	Data  map[string]interface{} `json:"data,omitempty"`
}

// SendSystemMessage sends a system-type chat message to a crew's text channel.
// The channelID must be the group channel ID (type 2 = group).
func SendSystemMessage(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, channelID string, event string, body string, data map[string]interface{}) {
	env := systemEnvelope{
		V:     1,
		Type:  "system",
		Body:  body,
		Event: event,
		Data:  data,
	}
	contentBytes, err := json.Marshal(env)
	if err != nil {
		logger.Error("Failed to marshal system message: %v", err)
		return
	}

	var contentMap map[string]interface{}
	if err := json.Unmarshal(contentBytes, &contentMap); err != nil {
		logger.Error("Failed to unmarshal system message for API: %v", err)
		return
	}

	_, err = nk.ChannelMessageSend(ctx, channelID, contentMap, "", "", true)
	if err != nil {
		logger.Warn("Failed to send system message to channel %s: %v", channelID, err)
	}
}

// SendMemberJoinedSystemMessage emits a "X joined the crew" system message.
func SendMemberJoinedSystemMessage(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, channelID string, displayName string) {
	body := fmt.Sprintf("%s joined the crew", displayName)
	SendSystemMessage(ctx, logger, nk, channelID, "member_joined", body, nil)
}

// SendMemberLeftSystemMessage emits a "X left the crew" system message.
func SendMemberLeftSystemMessage(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, channelID string, displayName string) {
	body := fmt.Sprintf("%s left the crew", displayName)
	SendSystemMessage(ctx, logger, nk, channelID, "member_left", body, nil)
}

// SendStreamStartedSystemMessage emits a "X started streaming" system message.
func SendStreamStartedSystemMessage(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, channelID string, displayName string) {
	body := fmt.Sprintf("%s started streaming", displayName)
	SendSystemMessage(ctx, logger, nk, channelID, "stream_started", body, nil)
}
