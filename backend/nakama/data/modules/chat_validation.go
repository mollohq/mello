package main

import (
	"context"
	"database/sql"
	"encoding/json"

	"github.com/heroiclabs/nakama-common/rtapi"
	"github.com/heroiclabs/nakama-common/runtime"
)

// messageEnvelope mirrors the structured JSON envelope from TEXT-CHAT spec §2.
type messageEnvelope struct {
	V        *float64               `json:"v"`
	Type     string                 `json:"type"`
	Body     string                 `json:"body"`
	ReplyTo  *string                `json:"reply_to,omitempty"`
	Mentions []string               `json:"mentions,omitempty"`
	Gif      map[string]interface{} `json:"gif,omitempty"`
	Event    string                 `json:"event,omitempty"`
	Data     map[string]interface{} `json:"data,omitempty"`
}

const maxBodyLength = 2000

// BeforeChannelMessageSendHook validates the structured message envelope.
func BeforeChannelMessageSendHook(
	ctx context.Context,
	logger runtime.Logger,
	db *sql.DB,
	nk runtime.NakamaModule,
	in *rtapi.Envelope,
) (*rtapi.Envelope, error) {

	send := in.GetChannelMessageSend()
	if send == nil {
		return in, nil
	}

	content := send.GetContent()
	if content == "" {
		return nil, runtime.NewError("empty message content", 3)
	}

	var raw map[string]interface{}
	if err := json.Unmarshal([]byte(content), &raw); err != nil {
		return nil, runtime.NewError("invalid message format", 3)
	}

	// Allow signaling messages to pass through
	if sig, ok := raw["signal"]; ok {
		if sigBool, isBool := sig.(bool); isBool && sigBool {
			return in, nil
		}
	}

	// Allow legacy {"text":"..."} format
	if _, hasText := raw["text"]; hasText {
		if _, hasV := raw["v"]; !hasV {
			return in, nil
		}
	}

	// Validate structured envelope
	var env messageEnvelope
	if err := json.Unmarshal([]byte(content), &env); err != nil {
		return nil, runtime.NewError("invalid message format", 3)
	}

	if env.V == nil {
		return nil, runtime.NewError("missing version field", 3)
	}

	if *env.V != 1 {
		return nil, runtime.NewError("unsupported message version", 3)
	}

	if env.Type != "text" && env.Type != "gif" {
		return nil, runtime.NewError("invalid message type", 3)
	}

	if len(env.Body) > maxBodyLength {
		return nil, runtime.NewError("message too long (max 2000)", 3)
	}

	if env.Type == "gif" && env.Gif == nil {
		return nil, runtime.NewError("gif type requires gif object", 3)
	}

	return in, nil
}
