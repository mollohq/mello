package main

import (
	"context"
	"crypto/hmac"
	"crypto/sha1"
	"database/sql"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

type IceServer struct {
	URLs       []string `json:"urls"`
	Username   string   `json:"username,omitempty"`
	Credential string   `json:"credential,omitempty"`
}

type GetIceServersResponse struct {
	IceServers []IceServer `json:"ice_servers"`
	TTL        int         `json:"ttl"`
}

func GetIceServersRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	servers := []IceServer{
		{
			URLs: []string{
				"stun:stun.l.google.com:19302",
				"stun:stun1.l.google.com:19302",
			},
		},
	}

	turnHost := os.Getenv("TURN_HOST")
	turnSecret := os.Getenv("TURN_SECRET")
	if turnHost != "" && turnSecret != "" {
		// Generate time-limited HMAC-SHA1 credentials
		// Format: username = "expiry_timestamp:userID"
		// Credential = HMAC-SHA1(secret, username) base64-encoded
		// This is the standard coturn use-auth-secret scheme
		timestamp := time.Now().Add(24 * time.Hour).Unix()
		username := fmt.Sprintf("%d:%s", timestamp, userID)

		mac := hmac.New(sha1.New, []byte(turnSecret))
		mac.Write([]byte(username))
		credential := base64.StdEncoding.EncodeToString(mac.Sum(nil))

		servers = append(servers, IceServer{
			URLs: []string{
				fmt.Sprintf("turn:%s:3478?transport=udp", turnHost),
				fmt.Sprintf("turn:%s:3478?transport=tcp", turnHost),
				fmt.Sprintf("turns:%s:5349?transport=tcp", turnHost),
			},
			Username:   username,
			Credential: credential,
		})
		logger.Info("ICE: returning STUN + TURN (%s) for user %s", turnHost, userID)
	} else {
		logger.Warn("ICE: TURN not configured (TURN_HOST=%s, TURN_SECRET set=%v)", turnHost, turnSecret != "")
	}

	resp := GetIceServersResponse{IceServers: servers, TTL: 86400}
	respJSON, _ := json.Marshal(resp)
	return string(respJSON), nil
}