package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"os"

	"github.com/heroiclabs/nakama-common/runtime"
)

type IceServer struct {
	URLs       []string `json:"urls"`
	Username   string   `json:"username,omitempty"`
	Credential string   `json:"credential,omitempty"`
}

type GetIceServersResponse struct {
	IceServers []IceServer `json:"ice_servers"`
}

func GetIceServersRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	_, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
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

	// Add TURN server from env if configured
	turnHost := os.Getenv("TURN_HOST")
	turnUser := os.Getenv("TURN_USERNAME")
	turnPass := os.Getenv("TURN_PASSWORD")
	if turnHost != "" && turnUser != "" && turnPass != "" {
		servers = append(servers, IceServer{
			URLs:       []string{"turn:" + turnHost},
			Username:   turnUser,
			Credential: turnPass,
		})
		logger.Info("ICE: returning STUN + TURN (%s)", turnHost)
	}

	resp := GetIceServersResponse{IceServers: servers}
	respJSON, _ := json.Marshal(resp)
	return string(respJSON), nil
}
